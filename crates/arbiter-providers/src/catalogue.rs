//! What can this key actually run, and what does it cost? — read live.
//!
//! A panel is a list of model ids, and until now the only way to get one into
//! it was to know it by heart. That is a bad deal for the operator and a worse
//! one for this file: an aggregator's catalogue turns over weekly, so any list
//! written down here would be wrong within the month and wrong *silently* —
//! the run would fail at `positions.generate` with a 404 from a vendor, which
//! is the most expensive place to discover a typo.
//!
//! So nothing is written down. [`list_models`] asks the provider, through the
//! same listing endpoints [`crate::probe`] already uses for key verification,
//! and reports what came back.
//!
//! Two properties come out of it, and they are not the same kind of claim:
//!
//! - **`free`** is read from the vendor's own published price. OpenRouter
//!   quotes `pricing.prompt` and `pricing.completion` per model, and a model
//!   quoting `0` for both is free *by the vendor's own statement*. Providers
//!   that publish no price with their listing report `None` — "this endpoint
//!   didn't say", which is the truth and renders as a blank, never as free.
//! - **`open_weights`** is inferred from the model's name, and is therefore a
//!   label rather than a licence audit. It says "this is a member of a family
//!   whose weights are published for download", which is a fact about
//!   Llama, DeepSeek, Qwen, Mistral, Gemma and the rest. It does **not** say
//!   the licence is OSI-approved — Llama's community licence and Gemma's terms
//!   both carry conditions, and `command-r`'s weights are non-commercial. An
//!   operator who needs a specific licence has to read that vendor's, and the
//!   UI says so rather than implying this settled it.
//!
//! Neither property is used to decide anything: they filter a list a person is
//! choosing from. Nothing here can put a model on a panel by itself.

use crate::http;
use crate::keys::SecretString;
use crate::probe::listing_request;
use arbiter_core::ProviderId;
use arbiter_kernel::provider::ProviderError;
use std::time::Duration;

/// Listing a whole catalogue is a bigger answer than [`crate::probe`]'s one
/// row, and OpenRouter's runs to a few hundred entries.
const CATALOGUE_TIMEOUT_SECS: u64 = 30;

/// One model a key can address, as its own vendor describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelListing {
    /// Exactly the string `--panel provider:<id>` wants — never a display
    /// name, and never rewritten. A pasted id that does not round-trip is a
    /// run that 404s at the provider.
    pub id: String,
    /// The vendor's own label, when it gives one distinct from the id.
    pub name: Option<String>,
    /// `Some(true)` when the vendor quotes a zero price, `Some(false)` when it
    /// quotes a non-zero one, `None` when its listing quotes no price at all.
    pub free: Option<bool>,
    /// Whether the id names a family whose weights are published. A label, not
    /// a licence — see this module's own note.
    pub open_weights: bool,
    /// Context window in tokens, when the listing states one.
    pub context_length: Option<u64>,
}

/// Families that publish downloadable weights, matched on the vendor prefix an
/// aggregator puts before the `/`. Deliberately a *family* judgement: every
/// `meta-llama/...` id is a Llama, whatever the fine-tune after it.
const OPEN_WEIGHT_VENDORS: [&str; 21] = [
    "meta-llama",
    "deepseek",
    "deepseek-ai",
    "qwen",
    "mistralai",
    "nousresearch",
    "teknium",
    "allenai",
    "tiiuae",
    "ibm-granite",
    "nvidia",
    "moonshotai",
    "z-ai",
    "thudm",
    "minimax",
    "arcee-ai",
    "liquid",
    "huggingfaceh4",
    "cognitivecomputations",
    "gryphe",
    "sao10k",
];

/// Family names that appear in ids carrying no vendor prefix — Groq's own
/// (`llama-3.3-70b-versatile`, `gemma2-9b-it`) among them. Matched as a
/// prefix of the bare id so `gpt-4o` can never be caught by `gpt-oss`.
const OPEN_WEIGHT_FAMILIES: [&str; 13] = [
    "llama", "gemma", "qwen", "deepseek", "mistral", "mixtral", "gpt-oss", "phi-", "olmo",
    "falcon", "granite", "smollm", "kimi",
];

/// Whether this id names a family whose weights are published.
///
/// `google/gemma-*` is open and `google/gemini-*` is not; `openai/gpt-oss-*`
/// is open and every other `openai/*` is not. Both pairs share a vendor
/// prefix, so the prefix alone cannot decide it — the model half is what
/// carries the answer, and it is checked for every id, prefixed or not.
pub fn has_open_weights(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let (vendor, model) = match lower.split_once('/') {
        Some((v, m)) => (Some(v), m),
        None => (None, lower.as_str()),
    };
    // Strip a routing suffix (`:free`, `:nitro`) before matching: it names a
    // way of serving the model, not a different model.
    let model = model.split(':').next().unwrap_or(model);
    if OPEN_WEIGHT_FAMILIES.iter().any(|f| model.starts_with(f)) {
        return true;
    }
    vendor.is_some_and(|v| OPEN_WEIGHT_VENDORS.contains(&v))
}

/// A price string as an aggregator quotes it — `"0"`, `"0.00000015"`, and
/// occasionally `"-1"` for "ask us". Anything that does not parse as a number
/// is not evidence of being free.
fn is_zero_price(value: Option<&serde_json::Value>) -> Option<bool> {
    let text = value?.as_str()?;
    let parsed: f64 = text.trim().parse().ok()?;
    Some(parsed == 0.0)
}

/// Reads one provider's model list into [`ModelListing`]s.
///
/// Shapes, all seen in the wild: OpenAI-compatible listings put the rows under
/// `data` with an `id`; Gemini uses `models` with a `name` of
/// `models/<id>`. Rows missing an id are dropped rather than guessed at.
fn parse_catalogue(body: &str) -> Vec<ModelListing> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let rows = ["data", "models"]
        .iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_array()))
        .cloned()
        .unwrap_or_default();

    rows.iter()
        .filter_map(|row| {
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                // Gemini: `"name": "models/gemini-3.6-flash"`, and the panel
                // wants the half after the slash.
                .or_else(|| {
                    row.get("name")
                        .and_then(|v| v.as_str())
                        .and_then(|n| n.strip_prefix("models/"))
                })?
                .to_string();
            if id.is_empty() {
                return None;
            }
            let name = row
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| *n != id && !n.starts_with("models/"))
                .map(|n| n.to_string());
            let pricing = row.get("pricing");
            let free = match (
                is_zero_price(pricing.and_then(|p| p.get("prompt"))),
                is_zero_price(pricing.and_then(|p| p.get("completion"))),
            ) {
                (Some(a), Some(b)) => Some(a && b),
                // One half priced and the other unquoted says nothing
                // conclusive, so it stays unknown rather than becoming a
                // guess in either direction.
                _ => None,
            };
            Some(ModelListing {
                open_weights: has_open_weights(&id),
                free,
                context_length: row
                    .get("context_length")
                    .or_else(|| row.get("inputTokenLimit"))
                    .and_then(|v| v.as_u64()),
                name,
                id,
            })
        })
        .collect()
}

/// Asks a provider what this key can run.
///
/// Errors carry the vendor's own sentence, the way every other call in this
/// crate does: a catalogue that cannot be read is a thing the operator has to
/// act on ("your key expired"), not a spinner that never resolves.
pub async fn list_models(
    provider: &ProviderId,
    key: &SecretString,
) -> Result<Vec<ModelListing>, ProviderError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(CATALOGUE_TIMEOUT_SECS))
        .build()
        .map_err(|e| ProviderError::Other(e.to_string()))?;
    let request = listing_request(&client, provider, key, false).ok_or_else(|| {
        ProviderError::Other(format!(
            "`{provider}` publishes no model list this build knows how to read"
        ))
    })?;

    let response = request
        .send()
        .await
        .map_err(|e| http::transport_error(provider.as_str(), e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ProviderError::Http {
            status: status.as_u16(),
            message: http::message_in(&body).unwrap_or_else(|| body.trim().to_string()),
        });
    }
    Ok(parse_catalogue(&body))
}

/// Picks `n` free open-weight models, at most one per family.
///
/// The per-family cap is the point. ARCHITECTURE §6.2 penalises a panel whose
/// members fail together, and five fine-tunes of one base model fail together
/// on exactly the questions the base model is weak on — they would be five
/// positions and one opinion. Taking Llama, then DeepSeek, then Qwen, then
/// Mistral, then Gemma gives five genuinely different training runs, which is
/// the most independence a single aggregator key can buy.
///
/// Order is the provider's own, so the same catalogue always yields the same
/// panel; a second pass relaxes the family cap only if the first cannot fill
/// `n`, and even then never repeats an id.
pub fn free_open_weight_panel(models: &[ModelListing], n: usize) -> Vec<String> {
    let eligible: Vec<&ModelListing> = models
        .iter()
        .filter(|m| m.open_weights && m.free == Some(true))
        .collect();

    let mut chosen: Vec<String> = Vec::new();
    let mut families: Vec<String> = Vec::new();
    for model in &eligible {
        if chosen.len() == n {
            break;
        }
        let family = family_of(&model.id);
        if families.contains(&family) {
            continue;
        }
        families.push(family);
        chosen.push(model.id.clone());
    }
    for model in &eligible {
        if chosen.len() == n {
            break;
        }
        if !chosen.contains(&model.id) {
            chosen.push(model.id.clone());
        }
    }
    chosen
}

/// The training-run identity behind an id: its vendor when it has one
/// (`meta-llama/llama-3.3-70b` and `meta-llama/llama-4-scout` are one
/// family), else the first `-`-separated word of a bare id
/// (`llama-3.3-70b-versatile` and `llama-3.1-8b-instant` likewise).
fn family_of(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    match lower.split_once('/') {
        Some((vendor, _)) => vendor.to_string(),
        None => lower.split('-').next().unwrap_or(&lower).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_prefix_names_the_family_whose_weights_are_published() {
        assert!(has_open_weights("meta-llama/llama-3.3-70b-instruct"));
        assert!(has_open_weights("deepseek/deepseek-chat-v3-0324:free"));
        assert!(has_open_weights("qwen/qwen-2.5-72b-instruct"));
        assert!(has_open_weights("mistralai/mistral-small-3.2-24b-instruct"));
    }

    /// The two pairs a prefix rule alone gets wrong: one vendor ships both an
    /// open family and a closed one, under the same prefix.
    #[test]
    fn one_vendor_can_ship_both_open_and_closed_families() {
        assert!(has_open_weights("google/gemma-3-27b-it"));
        assert!(!has_open_weights("google/gemini-3.6-flash"));
        assert!(has_open_weights("openai/gpt-oss-120b"));
        assert!(!has_open_weights("openai/gpt-4o"));
    }

    #[test]
    fn a_bare_id_is_matched_on_its_family_name() {
        // Groq's own ids carry no vendor prefix.
        assert!(has_open_weights("llama-3.3-70b-versatile"));
        assert!(has_open_weights("gemma2-9b-it"));
        assert!(!has_open_weights("grok-2-latest"));
        assert!(!has_open_weights("claude-sonnet-4-5"));
    }

    #[test]
    fn a_price_of_zero_on_both_halves_is_what_makes_a_model_free() {
        let body = r#"{"data":[
            {"id":"free/one","pricing":{"prompt":"0","completion":"0"}},
            {"id":"paid/one","pricing":{"prompt":"0","completion":"0.0000004"}},
            {"id":"unpriced/one"}
        ]}"#;
        let models = parse_catalogue(body);
        assert_eq!(models[0].free, Some(true));
        assert_eq!(models[1].free, Some(false), "output is still billed");
        assert_eq!(
            models[2].free, None,
            "a listing that quotes no price must not read as free"
        );
    }

    #[test]
    fn gemini_rows_are_read_from_their_slash_prefixed_name() {
        let body = r#"{"models":[{"name":"models/gemini-3.6-flash","inputTokenLimit":1048576}]}"#;
        let models = parse_catalogue(body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.6-flash");
        assert_eq!(models[0].context_length, Some(1048576));
    }

    #[test]
    fn a_catalogue_this_build_cannot_parse_is_empty_rather_than_wrong() {
        assert!(parse_catalogue("not json").is_empty());
        assert!(parse_catalogue(r#"{"unexpected":true}"#).is_empty());
        assert!(
            parse_catalogue(r#"{"data":[{"object":"model"}]}"#).is_empty(),
            "a row with no id is dropped, never guessed at"
        );
    }

    fn listing(id: &str, free: bool) -> ModelListing {
        ModelListing {
            id: id.to_string(),
            name: None,
            free: Some(free),
            open_weights: has_open_weights(id),
            context_length: None,
        }
    }

    /// The whole point of the picker: five *different* training runs, not five
    /// fine-tunes of one, because members that fail together are what §6.2's
    /// independence term is about.
    #[test]
    fn the_picked_panel_takes_one_model_per_family() {
        let catalogue = vec![
            listing("meta-llama/llama-3.3-70b-instruct:free", true),
            listing("meta-llama/llama-3.1-8b-instruct:free", true),
            listing("deepseek/deepseek-chat-v3-0324:free", true),
            listing("qwen/qwen-2.5-72b-instruct:free", true),
            listing("mistralai/mistral-small-3.2-24b-instruct:free", true),
            listing("google/gemma-3-27b-it:free", true),
        ];
        let picked = free_open_weight_panel(&catalogue, 5);
        assert_eq!(picked.len(), 5);
        assert!(
            !picked.contains(&"meta-llama/llama-3.1-8b-instruct:free".to_string()),
            "the second Llama must wait until every other family has a seat: {picked:?}"
        );
        let families: std::collections::BTreeSet<String> =
            picked.iter().map(|id| family_of(id)).collect();
        assert_eq!(families.len(), 5, "{picked:?}");
    }

    #[test]
    fn nothing_priced_or_closed_reaches_the_picked_panel() {
        let catalogue = vec![
            listing("openai/gpt-4o", true),                      // free but closed
            listing("meta-llama/llama-3.3-70b-instruct", false), // open but billed
            listing("deepseek/deepseek-chat-v3-0324:free", true), // both
        ];
        assert_eq!(
            free_open_weight_panel(&catalogue, 5),
            vec!["deepseek/deepseek-chat-v3-0324:free".to_string()]
        );
    }

    /// Asking for more families than exist falls back to a second model from
    /// one already seated rather than returning short -- but never the same id
    /// twice, which would be one model claiming two seats on the panel.
    #[test]
    fn a_short_catalogue_repeats_a_family_but_never_a_model() {
        let catalogue = vec![
            listing("meta-llama/llama-3.3-70b-instruct:free", true),
            listing("meta-llama/llama-3.1-8b-instruct:free", true),
        ];
        let picked = free_open_weight_panel(&catalogue, 5);
        assert_eq!(picked.len(), 2);
        assert_eq!(
            picked
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );
    }
}
