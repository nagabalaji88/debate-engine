//! Stage 1: `init` (ARCHITECTURE §5: "validate question, snapshot config and
//! prompt pack hash, seed RNG, open log"). No LLM call.
//!
//! Not a `Stage` — K3's `Stage` trait takes a `&StageContext`, which itself
//! needs an already-open run to exist; `init` is what opens the run every
//! later stage runs inside of. Lives here, not in `arbiter-kernel`, because it
//! needs both `RunStore`'s concrete implementation and the hash-chaining
//! machinery ([`crate::events::ChainState`]/[`crate::events::append_chained`])
//! in the same call — `arbiter-kernel` cannot depend on either (D1).

use crate::events::{ChainState, append_chained};
use crate::now_rfc3339;
use arbiter_core::RunId;
use arbiter_kernel::event::{Event, EventType};
use arbiter_kernel::ids::{EventId, StageName};
use arbiter_kernel::init::{EmptyQuestion, validate_question};
use arbiter_kernel::store::{Manifest, RunStore, RunWriter, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(transparent)]
    EmptyQuestion(#[from] EmptyQuestion),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `RUN_STARTED`'s payload: the question plus the manifest this run is pinned
/// to. Neither spec file gives `RUN_STARTED` an explicit field list
/// (PLAN_DEVIATIONS.md D30) — the manifest is exactly "the run states what
/// governed it," the same reasoning INTERFACES §23 already gives for
/// `pack_hash` alone, extended here to the whole manifest so every constant
/// `--repolicy`/`--repack` would otherwise need to diff is in the one place a
/// human or `explain` can already read it from.
fn run_started_payload(question: &str, manifest: &Manifest) -> serde_json::Value {
    serde_json::json!({
        "question": question,
        "policy_version": manifest.policy_version.as_str(),
        "config_hash": manifest.config_hash,
        "pack_hash": manifest.pack_hash,
        "correlation_table_version": manifest.correlation_table_version,
        "rng_seed": manifest.rng_seed,
    })
}

/// Validates `question`, opens the run (`RunStore::create`), and appends the
/// chain's first event, `RUN_STARTED` — durable, since a lifecycle event is
/// exactly the kind ARCHITECTURE §8.3 says commits immediately rather than
/// batching to the next stage boundary. Returns the open [`RunWriter`] every
/// later stage writes through.
pub fn init(
    store: &dyn RunStore,
    run_id: &RunId,
    question: &str,
    manifest: &Manifest,
) -> Result<Box<dyn RunWriter>, InitError> {
    validate_question(question)?;
    let mut writer = store.create(run_id, manifest)?;

    let mut chain = ChainState::empty();
    let event = Event {
        schema_version: 1,
        event_id: EventId::new(format!("evt_{}_init", run_id.as_str())),
        run_id: run_id.clone(),
        sequence: None,
        timestamp: now_rfc3339(),
        stage: StageName::new("init"),
        event_type: EventType::RunStarted,
        durable: true,
        payload: run_started_payload(question, manifest),
        content_hash: String::new(),
        previous_event_hash: None,
    };
    append_chained(writer.as_mut(), &mut chain, event)?;

    Ok(writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite_store::SqliteRunStore;
    use arbiter_core::PolicyVersion;

    fn temp_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "arbiter_init_test_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn manifest() -> Manifest {
        Manifest {
            policy_version: PolicyVersion::new("argument-v1"),
            config_hash: "blake3:cfg".to_string(),
            pack_hash: "blake3:pack".to_string(),
            correlation_table_version: "2026.1".to_string(),
            rng_seed: 42,
        }
    }

    #[test]
    fn an_empty_question_is_refused_before_the_run_opens() {
        let root = temp_root("empty_question");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");

        let result = init(&store, &run_id, "   ", &manifest());
        assert!(matches!(result, Err(InitError::EmptyQuestion(_))));

        // No run.db must have been created -- the question is validated
        // before the store is ever touched.
        assert!(!root.join("run_1").join("run.db").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn init_opens_the_run_and_appends_a_self_verifying_run_started_event() {
        let root = temp_root("happy_path");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");

        let writer = init(
            &store,
            &run_id,
            "Should we adopt microservices?",
            &manifest(),
        )
        .unwrap();
        drop(writer);

        let reader = store.reader(&run_id).unwrap();
        let events: Vec<Event> = reader.events().unwrap().collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::RunStarted);
        assert_eq!(events[0].previous_event_hash, None);
        assert_eq!(
            events[0].payload.get("question").and_then(|v| v.as_str()),
            Some("Should we adopt microservices?")
        );
        assert_eq!(
            events[0].payload.get("pack_hash").and_then(|v| v.as_str()),
            Some("blake3:pack")
        );

        let status = reader.verify_chain().unwrap();
        assert!(matches!(status, arbiter_kernel::store::ChainStatus::Intact));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn init_on_an_already_open_run_is_already_open() {
        let root = temp_root("already_open");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");

        init(&store, &run_id, "first question", &manifest()).unwrap();
        let result = init(&store, &run_id, "second question", &manifest());
        assert!(matches!(
            result,
            Err(InitError::Store(StoreError::AlreadyOpen))
        ));

        let _ = std::fs::remove_dir_all(&root);
    }
}
