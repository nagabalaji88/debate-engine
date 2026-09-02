//! F2 — key config refusal + redaction (P3), ARCHITECTURE §18's CI suite.

use arbiter_providers::keys::{Redactor, SecretString, scan_configs_for_key_shaped_values};
use std::io::Write;

fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "arbiter_fixtures_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
    path
}

/// `key_in_config_refused`: "an `api_key` value in `config.toml` fails the
/// run and names the file." ARCHITECTURE §11.1's own rule -- a key ever
/// written to a config file on disk is refused outright, never silently
/// accepted, because a config file (unlike an env var) is the kind of thing
/// that gets committed to a repo. `scan_configs_for_key_shaped_values` must
/// name both the exact file and the 1-indexed line the value was found on,
/// and a value-less `api_key = ""` (or one commented out) must not
/// false-positive.
#[test]
fn key_in_config_refused() {
    let path = temp_file(
        "config_with_key",
        "# arbiter config\n[anthropic]\nmodel = \"claude\"\napi_key = \"sk-live-totally-real-secret\"\n",
    );

    let result = scan_configs_for_key_shaped_values(&[&path]);
    let err = result
        .expect_err("a config.toml carrying a live api_key must be refused, not silently accepted");
    assert_eq!(
        err.path,
        path.display().to_string(),
        "the refusal must name the exact file"
    );
    assert_eq!(
        err.line, 4,
        "the refusal must name the exact 1-indexed line the key-shaped value was found on"
    );

    let _ = std::fs::remove_file(&path);

    let clean_path = temp_file(
        "config_clean",
        "# api_key = \"this is just documentation\"\nmodel = \"claude\"\n",
    );
    assert!(
        scan_configs_for_key_shaped_values(&[&clean_path]).is_ok(),
        "a commented-out example must never false-positive"
    );
    let _ = std::fs::remove_file(&clean_path);
}

/// `key_redaction`: "a key echoed in a provider error body never reaches
/// the log, cache, manifest or export." A `Redactor` with the real secret
/// registered must scrub it out of every one of those four kinds of text --
/// none of them ever seeing the raw substring again, only the fixed
/// `[REDACTED]` placeholder.
#[test]
fn key_redaction() {
    let redactor = Redactor::new();
    let secret = SecretString::new("sk-live-totally-real-secret");
    redactor.register(&secret);
    let raw = secret.expose().to_string();

    let error_body = format!("upstream 401: invalid credentials for key {raw}");
    let cached_response = format!("{{\"error\": \"auth failed, key={raw} was rejected\"}}");
    let manifest_text = format!("{{\"debug_note\": \"resolved from env, value={raw}\"}}");
    let export_text = format!("run export — last error seen: {raw}");

    for (label, text) in [
        ("error body", error_body.as_str()),
        ("cached response", cached_response.as_str()),
        ("manifest", manifest_text.as_str()),
        ("export", export_text.as_str()),
    ] {
        let scrubbed = redactor.redact(text);
        assert!(
            !scrubbed.contains(&raw),
            "the raw key must never survive redaction in the {label}"
        );
        assert!(
            scrubbed.contains("[REDACTED]"),
            "the {label} must carry the redaction placeholder in its place"
        );
    }
}
