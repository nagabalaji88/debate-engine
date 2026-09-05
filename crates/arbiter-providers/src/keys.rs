//! Credential resolution, redaction, and the verification cache —
//! ARCHITECTURE §11.1, INTERFACES §25.
//!
//! PLAN_DEVIATIONS.md D46 covers where this deviates from the literal spec
//! text: `KeySource::ArbiterEnv`/`ProviderEnv` hold owned `String`s, not
//! `&'static str` (the env var name is derived per-provider at runtime, so
//! nothing here could be `'static`); `SecretString` has a manual `Debug`
//! impl that always prints a redacted placeholder rather than no `Debug`
//! impl at all (this workspace's own `missing_debug_implementations = "warn"`
//! lint, run under `-D warnings`, would otherwise turn "no Debug" into a
//! hard build failure — a redacting impl gives the same practical guarantee
//! the spec's prose is actually after: "the most common way a secret
//! reaches a log is a struct derived with `#[derive(Debug)]` three layers
//! up" — without fighting the lint policy).

use arbiter_core::ProviderId;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use zeroize::Zeroize;

/// A resolved credential value. Never `Display`, never a *transparent*
/// `Debug` — its own `Debug` impl always prints a fixed placeholder, and its
/// `Drop` zeroes the buffer, so a `#[derive(Debug)]` struct three layers up
/// that happens to hold one can never print the plaintext, and the memory
/// it occupied is overwritten before being freed.
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The one place a caller must consciously ask for the raw value —
    /// resolving it into a provider request, or registering it with a
    /// [`Redactor`]. Never call this to log, print, or serialize.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `blake3(key)[..16]` (ARCHITECTURE §11.1: "keyed by a fingerprint,
    /// never by the key... storing the fingerprint rather than the key
    /// means rotating a key invalidates its cached result automatically").
    /// The first 16 *hex characters* of the hash, not 16 bytes — the
    /// slice notation names a width, and hex characters are what every
    /// other fingerprint-shaped string in this codebase already is
    /// (`content_hash`, `pack_hash`, ...).
    pub fn fingerprint(&self) -> String {
        blake3::hash(self.0.as_bytes()).to_hex()[..16].to_string()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SecretString").field(&"[REDACTED]").finish()
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Where a resolved credential came from — ARCHITECTURE §11.1's own
/// three-source resolution order, first match wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// `ARBITER_<PROVIDER>_API_KEY` — the exact variable name resolved,
    /// e.g. `"ARBITER_ANTHROPIC_API_KEY"`.
    ArbiterEnv(String),
    /// The provider's own conventional variable, e.g. `"ANTHROPIC_API_KEY"`.
    ProviderEnv(String),
    /// `arbiter keys set <provider>` — the OS keychain.
    Keychain,
}

/// One provider's key state, ARCHITECTURE §11.1's own four-state table —
/// "three different things get called valid, and the UI must not confuse
/// them."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Missing,
    Present {
        source: KeySource,
    },
    Verified {
        source: KeySource,
        at: String,
    },
    Rejected {
        source: KeySource,
        status: u16,
        at: String,
    },
}

/// INTERFACES §25, copied signature-for-signature except `SecretString`'s
/// own resolution (this file) fills in what INTERFACES leaves abstract.
pub trait CredentialSource: Send + Sync {
    /// First match wins. Returns the value AND where it came from — the
    /// operator needs to know which of the three sources is winning when
    /// the wrong key is in use.
    fn resolve(&self, provider: &ProviderId) -> Option<(SecretString, KeySource)>;
}

/// A provider id's own conventional environment variable, where one is
/// known. Only Anthropic's is named anywhere in this workspace's spec
/// (ARCHITECTURE §11.1's own example); extending this table is how a
/// future P4 adapter registers its own (PLAN_DEVIATIONS.md D46) —
/// inventing names for gateways no adapter exists for yet would be
/// guessing at a contract nothing has committed to.
fn conventional_env_var(provider: &ProviderId) -> Option<&'static str> {
    match provider.as_str() {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

/// Sources 1 and 2 of ARCHITECTURE §11.1's resolution order — the two that
/// are pure environment reads, fully deterministic and testable without a
/// real OS keychain.
#[derive(Debug, Default)]
pub struct EnvCredentialSource;

impl CredentialSource for EnvCredentialSource {
    fn resolve(&self, provider: &ProviderId) -> Option<(SecretString, KeySource)> {
        resolve_from_env(|name| std::env::var(name).ok(), provider)
    }
}

/// The actual resolution logic behind [`EnvCredentialSource`], factored out
/// so it can be tested against a fake environment instead of the real
/// process one. Process env vars are global, mutable state shared across
/// every test in the binary; `std::env::set_var`/`remove_var` are also
/// `unsafe` fn as of this edition, which `#![forbid(unsafe_code)]` blocks
/// even inside `#[cfg(test)]` — so tests inject a lookup closure backed by
/// a local map instead of touching the real environment at all
/// (PLAN_DEVIATIONS.md D46).
fn resolve_from_env(
    lookup: impl Fn(&str) -> Option<String>,
    provider: &ProviderId,
) -> Option<(SecretString, KeySource)> {
    let arbiter_var = format!("ARBITER_{}_API_KEY", provider.as_str().to_uppercase());
    if let Some(value) = lookup(&arbiter_var).and_then(non_blank) {
        return Some((SecretString::new(value), KeySource::ArbiterEnv(arbiter_var)));
    }
    let provider_var = conventional_env_var(provider)?;
    let value = lookup(provider_var).and_then(non_blank)?;
    Some((
        SecretString::new(value),
        KeySource::ProviderEnv(provider_var.to_string()),
    ))
}

/// A variable that is set but empty is a *missing* credential, not an empty
/// one, and the distinction is load-bearing twice over.
///
/// An empty string sent as a bearer token or an `x-api-key` header reads as
/// "no auth" to some providers and as a malformed header to others; both fail
/// later, with a worse error than "no key configured" and pointing at the
/// wrong thing — the operator is told to replace a key when the real problem
/// is that their variable never got a value.
///
/// Worse, without this an `export ANTHROPIC_API_KEY=` in a shell profile
/// silently *shadows* a perfectly good keychain entry: the environment is
/// consulted first, it answers, and the key that would have worked is never
/// reached. Trimming is deliberate too — a trailing newline from
/// `export KEY=$(cat file)` is the same mistake wearing a different hat.
fn non_blank(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Source 3: the OS keychain, via `keyring` — the one cross-platform crate
/// reaching macOS Keychain / Linux Secret Service / Windows Credential
/// Manager behind one API (ARCHITECTURE §11.1's own three named backends).
/// `service` is fixed at `"arbiter"`; `username` is the provider id, so
/// `arbiter keys set anthropic` and this resolve agree on the same entry.
///
/// PLAN_DEVIATIONS.md D46: the actual OS round-trip (`set` then read back)
/// cannot be exercised in this sandbox — no D-Bus session bus, no real
/// macOS/Windows — the same class of gap P4 was already deferred for, just
/// narrower: only this one resolution source, not the whole task. The
/// wiring itself is real, standard `keyring` usage, not a stub.
#[derive(Debug, Default)]
pub struct KeychainCredentialSource;

const KEYCHAIN_SERVICE: &str = "arbiter";

impl KeychainCredentialSource {
    fn entry(provider: &ProviderId) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(KEYCHAIN_SERVICE, provider.as_str())
    }

    /// `arbiter keys set <provider>` — stores `value` for later resolution.
    pub fn set(provider: &ProviderId, value: &SecretString) -> Result<(), keyring::Error> {
        Self::entry(provider)?.set_password(value.expose())
    }

    /// `arbiter keys rm <provider>`.
    pub fn remove(provider: &ProviderId) -> Result<(), keyring::Error> {
        Self::entry(provider)?.delete_credential()
    }
}

impl CredentialSource for KeychainCredentialSource {
    fn resolve(&self, provider: &ProviderId) -> Option<(SecretString, KeySource)> {
        let entry = Self::entry(provider).ok()?;
        // Same rule as the environment: a blank entry is no entry. A keychain
        // can hold an empty string just as a shell can export one.
        let value = non_blank(entry.get_password().ok()?)?;
        Some((SecretString::new(value), KeySource::Keychain))
    }
}

/// Tries every source in order, first match wins — ARCHITECTURE §11.1's own
/// resolution order collapsed into one [`KeyState`]. A caller wanting the
/// resolved [`SecretString`] itself (to actually make a call) should try
/// each `CredentialSource::resolve` directly instead; this is the read-only
/// view `arbiter keys list`/`doctor`/the panel picker need.
pub fn resolve_state(sources: &[&dyn CredentialSource], provider: &ProviderId) -> KeyState {
    for source in sources {
        if let Some((_, key_source)) = source.resolve(provider) {
            return KeyState::Present { source: key_source };
        }
    }
    KeyState::Missing
}

/// ARCHITECTURE §11.1: "config files are never read for a key... a
/// key-shaped value under an `api_key` field fails the run with an error
/// naming the file [and line]." A line scan, not a full TOML-schema check —
/// what the spec asks for is catching the *shape* (an `api_key` assignment)
/// wherever it appears, in any of the several files config is never read
/// from (`config.toml`, `.arbiter/config.toml`, a plugin's `plugin.toml`),
/// not validating a schema no config-loading module exists yet to define
/// (PLAN_DEVIATIONS.md D46).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{path}:{line}: a key-shaped value under 'api_key' was found in a config file -- \
         config files are never read for a key (ARCHITECTURE §11.1). Use ARBITER_<PROVIDER>_API_KEY, \
         the provider's own environment variable, or `arbiter keys set <provider>` instead."
)]
pub struct KeyShapedConfigValue {
    pub path: String,
    pub line: usize,
}

/// A line is "key-shaped" when, after trimming whitespace and stripping a
/// TOML `[section]` header, it assigns a non-empty value to a bare or
/// dotted key literally named (or ending in) `api_key` — `api_key = "..."`,
/// `anthropic.api_key = "..."`, etc. Comments (`#...`) are stripped first
/// so a documented example in a comment doesn't false-positive.
fn line_is_key_shaped(line: &str) -> bool {
    let line = line.split('#').next().unwrap_or("").trim();
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    let key = key.trim();
    let value = value.trim();
    (key == "api_key" || key.ends_with(".api_key")) && !value.is_empty()
}

/// Scans every path in `paths` that actually exists; a missing file is not
/// an error (most of these candidate locations never exist on a given
/// machine). Returns the first key-shaped value found, naming its file and
/// 1-indexed line.
pub fn scan_configs_for_key_shaped_values(
    paths: &[impl AsRef<Path>],
) -> Result<(), KeyShapedConfigValue> {
    for path in paths {
        let path = path.as_ref();
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line_is_key_shaped(line) {
                return Err(KeyShapedConfigValue {
                    path: path.display().to_string(),
                    line: i + 1,
                });
            }
        }
    }
    Ok(())
}

/// ARCHITECTURE §11.1: "redaction is a write-path rule... every resolved
/// key value is registered with the redactor at `init`, and every event
/// payload, cached response, manifest, export and error string is scanned
/// for those values before it is written." This is the registry and the
/// scan; wiring every one of those write paths through it is a separate,
/// later integration (PLAN_DEVIATIONS.md D46) — there is no real secret
/// flowing through any of them yet, since `--panel mock` (the only panel
/// this codebase can run, L1) needs no key at all. `Debug` is manual, like
/// `SecretString`'s own, so a struct holding a `Redactor` can't leak the
/// registered secrets through a derive either.
pub struct Redactor {
    /// Raw secret substrings to scan for. Kept as plain `String`s, not
    /// `SecretString`s: the whole point is repeated substring search
    /// against arbitrary text, which needs `&str` access on every call,
    /// and a `Redactor` living for a run's whole lifetime is itself
    /// zeroized on drop (its `Drop` impl), so the exposure window is the
    /// same one any `SecretString` already accepts by existing at all.
    secrets: Mutex<Vec<String>>,
}

impl Redactor {
    pub fn new() -> Self {
        Self {
            secrets: Mutex::new(Vec::new()),
        }
    }

    pub fn register(&self, secret: &SecretString) {
        let value = secret.expose().to_string();
        if value.is_empty() {
            return;
        }
        self.secrets.lock().unwrap().push(value);
    }

    /// Replaces every occurrence of every registered secret with a fixed
    /// placeholder. Longest secrets first, so one secret that happens to be
    /// a substring of another is never partially matched and left with a
    /// dangling fragment of itself visible.
    pub fn redact(&self, text: &str) -> String {
        let mut secrets = self.secrets.lock().unwrap().clone();
        secrets.sort_by_key(|b| std::cmp::Reverse(b.len()));
        let mut out = text.to_string();
        for secret in &secrets {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
        out
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("registered", &self.secrets.lock().unwrap().len())
            .finish()
    }
}

impl Drop for Redactor {
    fn drop(&mut self) {
        for s in self.secrets.get_mut().unwrap() {
            s.zeroize();
        }
    }
}

/// ARCHITECTURE §11.1: "verification results are cached, keyed by a
/// fingerprint, never by the key... `(provider, model, blake3(key)[..16],
/// result, checked_at)` with a 24-hour TTL." In-memory only —
/// process-local, like `ResponseCache`/`BudgetLedger` were before their own
/// consuming tasks gave them a persistence path (L1/L3's own precedent);
/// nothing yet needs this cache to survive a process restart, since
/// `arbiter keys test` (the only thing that ever populates it) doesn't
/// exist as a live command yet either (PLAN_DEVIATIONS.md D46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    Verified,
    Rejected { status: u16 },
}

const VERIFICATION_TTL_SECS: u64 = 24 * 60 * 60;

/// `(provider, model, fingerprint)`.
type VerificationKey = (String, String, String);

#[derive(Debug, Default)]
pub struct VerificationCache {
    entries: Mutex<BTreeMap<VerificationKey, (VerifyResult, u64)>>,
}

impl VerificationCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(
        &self,
        provider: &str,
        model: &str,
        fingerprint: &str,
        result: VerifyResult,
        now_unix: u64,
    ) {
        self.entries.lock().unwrap().insert(
            (
                provider.to_string(),
                model.to_string(),
                fingerprint.to_string(),
            ),
            (result, now_unix),
        );
    }

    /// `None` on a miss *or* an expired entry — a caller has no reason to
    /// distinguish the two, both mean "verify again."
    pub fn get(
        &self,
        provider: &str,
        model: &str,
        fingerprint: &str,
        now_unix: u64,
    ) -> Option<VerifyResult> {
        let key = (
            provider.to_string(),
            model.to_string(),
            fingerprint.to_string(),
        );
        let entries = self.entries.lock().unwrap();
        let (result, checked_at) = entries.get(&key)?;
        if now_unix.saturating_sub(*checked_at) > VERIFICATION_TTL_SECS {
            return None;
        }
        Some(result.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_debug_never_reveals_the_value() {
        // The spec's own words are "does not implement Display or Debug" --
        // this workspace's `missing_debug_implementations` lint (run under
        // `-D warnings`) makes "no Debug impl at all" a hard build failure
        // for a public type, so the actual guarantee built here is the
        // practical one the spec's own reasoning names: a struct three
        // layers up that derives Debug can never print the plaintext
        // through it (PLAN_DEVIATIONS.md D46).
        let secret = SecretString::new("sk-super-secret-value");
        let debug_output = format!("{secret:?}");
        assert!(!debug_output.contains("sk-super-secret-value"));
        assert!(debug_output.contains("REDACTED"));
    }

    #[test]
    fn fingerprint_is_sixteen_hex_chars_and_stable() {
        let a = SecretString::new("key-a");
        let b = SecretString::new("key-a");
        let c = SecretString::new("key-b");
        assert_eq!(a.fingerprint().len(), 16);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
    }

    /// A `CredentialSource` backed by a local map instead of real process
    /// env vars — real env vars are global mutable state shared across
    /// every test binary-wide, and `set_var`/`remove_var` are `unsafe` fn
    /// this edition, which `#![forbid(unsafe_code)]` blocks even in tests
    /// (PLAN_DEVIATIONS.md D46). Exercises the exact same
    /// `resolve_from_env` logic `EnvCredentialSource` itself calls.
    struct FakeEnvSource(BTreeMap<String, String>);

    impl CredentialSource for FakeEnvSource {
        fn resolve(&self, provider: &ProviderId) -> Option<(SecretString, KeySource)> {
            resolve_from_env(|name| self.0.get(name).cloned(), provider)
        }
    }

    /// A variable that is set but empty is not a credential. The dangerous
    /// half of this is the shadowing: the environment is consulted before the
    /// keychain, so without the blank check an `export ANTHROPIC_API_KEY=` in
    /// a shell profile makes a perfectly good stored key unreachable, and the
    /// operator is told their key was rejected.
    #[test]
    fn a_blank_env_var_is_a_missing_credential_not_an_empty_one() {
        let provider = ProviderId::new("anthropic");
        for blank in ["", "   ", "\n", "\t \n"] {
            let env = FakeEnvSource(BTreeMap::from([(
                "ARBITER_ANTHROPIC_API_KEY".to_string(),
                blank.to_string(),
            )]));
            assert!(
                env.resolve(&provider).is_none(),
                "{blank:?} must not resolve as a key"
            );
        }
    }

    #[test]
    fn a_blank_arbiter_var_does_not_shadow_the_providers_own() {
        let provider = ProviderId::new("anthropic");
        let env = FakeEnvSource(BTreeMap::from([
            ("ARBITER_ANTHROPIC_API_KEY".to_string(), "  ".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "real-key".to_string()),
        ]));
        let (secret, source) = env.resolve(&provider).expect("the real key must win");
        assert_eq!(secret.expose(), "real-key");
        assert!(matches!(source, KeySource::ProviderEnv(_)));
    }

    /// `export KEY=$(cat keyfile)` carries the file's trailing newline, which
    /// travels into the auth header and is rejected by some providers with a
    /// message that says nothing about whitespace.
    #[test]
    fn surrounding_whitespace_is_trimmed_off_a_key() {
        let provider = ProviderId::new("anthropic");
        let env = FakeEnvSource(BTreeMap::from([(
            "ARBITER_ANTHROPIC_API_KEY".to_string(),
            "  sk-ant-value\n".to_string(),
        )]));
        let (secret, _) = env.resolve(&provider).unwrap();
        assert_eq!(secret.expose(), "sk-ant-value");
    }

    #[test]
    fn env_source_prefers_arbiter_scoped_var_over_the_providers_own() {
        let provider = ProviderId::new("anthropic");
        let env = FakeEnvSource(BTreeMap::from([
            (
                "ARBITER_ANTHROPIC_API_KEY".to_string(),
                "arbiter-scoped".to_string(),
            ),
            (
                "ANTHROPIC_API_KEY".to_string(),
                "provider-conventional".to_string(),
            ),
        ]));

        let (value, source) = env
            .resolve(&provider)
            .expect("both vars set, arbiter-scoped should win");
        assert_eq!(value.expose(), "arbiter-scoped");
        assert_eq!(
            source,
            KeySource::ArbiterEnv("ARBITER_ANTHROPIC_API_KEY".to_string())
        );
    }

    #[test]
    fn env_source_falls_back_to_the_providers_own_conventional_var() {
        let provider = ProviderId::new("anthropic");
        let env = FakeEnvSource(BTreeMap::from([(
            "ANTHROPIC_API_KEY".to_string(),
            "provider-conventional".to_string(),
        )]));

        let (value, source) = env.resolve(&provider).expect("provider var set");
        assert_eq!(value.expose(), "provider-conventional");
        assert_eq!(
            source,
            KeySource::ProviderEnv("ANTHROPIC_API_KEY".to_string())
        );
    }

    #[test]
    fn a_provider_with_no_known_env_var_and_no_arbiter_override_resolves_to_missing() {
        let provider = ProviderId::new("together");
        let env = FakeEnvSource(BTreeMap::new());

        assert!(env.resolve(&provider).is_none());
        assert_eq!(resolve_state(&[&env], &provider), KeyState::Missing);
    }

    #[test]
    fn config_file_key_fails_and_names_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "arbiter_keys_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[anthropic]\napi_key = \"sk-should-never-be-here\"\n",
        )
        .unwrap();

        let err = scan_configs_for_key_shaped_values(&[&config_path])
            .expect_err("a key-shaped value must be refused");
        assert_eq!(err.path, config_path.display().to_string());
        assert_eq!(err.line, 2);
        assert!(err.to_string().contains(&config_path.display().to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_file_with_no_key_shaped_value_passes() {
        let dir = std::env::temp_dir().join(format!(
            "arbiter_keys_test_clean_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            "# api_key = \"this is only documentation, not a real assignment\"\nport = 7777\n",
        )
        .unwrap();

        assert!(scan_configs_for_key_shaped_values(&[&config_path]).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_config_path_is_not_an_error() {
        let missing = std::env::temp_dir().join("arbiter_keys_test_never_created.toml");
        assert!(scan_configs_for_key_shaped_values(&[&missing]).is_ok());
    }

    #[test]
    fn key_echoed_in_an_error_body_is_redacted() {
        let redactor = Redactor::new();
        let secret = SecretString::new("sk-ant-abc123xyz");
        redactor.register(&secret);

        let error_body = "401 Unauthorized: the key sk-ant-abc123xyz was not recognized";
        let redacted = redactor.redact(error_body);

        assert!(!redacted.contains("sk-ant-abc123xyz"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn redaction_prefers_the_longest_matching_secret_so_no_fragment_leaks() {
        let redactor = Redactor::new();
        redactor.register(&SecretString::new("sk-short"));
        redactor.register(&SecretString::new("sk-short-but-longer"));

        let redacted = redactor.redact("key: sk-short-but-longer");
        assert_eq!(redacted, "key: [REDACTED]");
    }

    #[test]
    fn rotating_a_key_invalidates_its_cached_result() {
        let cache = VerificationCache::new();
        let old_key = SecretString::new("sk-old");
        let new_key = SecretString::new("sk-new");

        cache.put(
            "anthropic",
            "claude-opus-5",
            &old_key.fingerprint(),
            VerifyResult::Verified,
            1_000,
        );
        assert_eq!(
            cache.get("anthropic", "claude-opus-5", &old_key.fingerprint(), 1_000),
            Some(VerifyResult::Verified)
        );

        // The key rotates -- same provider/model, a different fingerprint.
        // The cache must not serve the old result for the new key.
        assert_eq!(
            cache.get("anthropic", "claude-opus-5", &new_key.fingerprint(), 1_000),
            None
        );
    }

    #[test]
    fn a_verification_result_expires_after_its_24_hour_ttl() {
        let cache = VerificationCache::new();
        let key = SecretString::new("sk-key");
        cache.put(
            "anthropic",
            "claude-opus-5",
            &key.fingerprint(),
            VerifyResult::Verified,
            0,
        );

        assert_eq!(
            cache.get(
                "anthropic",
                "claude-opus-5",
                &key.fingerprint(),
                VERIFICATION_TTL_SECS
            ),
            Some(VerifyResult::Verified),
            "exactly at the TTL boundary is still fresh"
        );
        assert_eq!(
            cache.get(
                "anthropic",
                "claude-opus-5",
                &key.fingerprint(),
                VERIFICATION_TTL_SECS + 1
            ),
            None,
            "one second past the TTL must miss"
        );
    }
}
