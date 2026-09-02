//! Provider-call state machine: resume classification and capability-gated
//! retry, ARCHITECTURE §8.4 / INTERFACES §5.

use crate::provider::{CallState, IdempotencyStyle};

/// What `resume` does with a `provider_calls` row found in a non-terminal state
/// — ARCHITECTURE §8.4's own table *is* the implementation, transcribed
/// directly: the branch is on the state, never on whether a cache entry exists,
/// because the state is what says whether the request reached the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// `RESERVED`, no `CALL_STARTED` committed: never dispatched. Release the
    /// reservation, mark `FAILED`, retry freely.
    ReleaseAndFail,
    /// `SENT`, no request id: may have been billed, nothing to reconcile by yet.
    /// Hold the reservation, report it in the orphaned total.
    HoldOrphaned,
    /// `ACKNOWLEDGED`: may have been billed, but a `request_id` exists — hold
    /// and report, reconcilable against a usage export.
    HoldOrphanedReconcilable,
    /// `COMPLETED`: already committed, nothing to do.
    NoAction,
}

/// The four rows of ARCHITECTURE §8.4's resume table. Any other `CallState`
/// (`Retryable`, `Failed`, `Orphaned`, `Recovered`) is not a state a fresh
/// `resume` should ever find as the *last committed* row — those are themselves
/// the outcomes of already having classified a call, not raw material for
/// classification — so this function is total but returns `NoAction` for them
/// rather than panicking on an input the table doesn't cover.
pub fn classify_on_resume(last_committed_state: CallState) -> ResumeAction {
    match last_committed_state {
        CallState::Reserved => ResumeAction::ReleaseAndFail,
        CallState::Sent => ResumeAction::HoldOrphaned,
        CallState::Acknowledged => ResumeAction::HoldOrphanedReconcilable,
        CallState::Completed
        | CallState::Retryable
        | CallState::Failed
        | CallState::Orphaned
        | CallState::Recovered => ResumeAction::NoAction,
    }
}

/// What to do with a `SENT`/`ACKNOWLEDGED` call on resume, once a cache lookup
/// has actually been tried (INTERFACES §5's fuller branch, beyond the table
/// above: cache hit, idempotency-gated retry, or hold as orphaned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Cache hit on `(provider, model, params, prompt_hash)` — never
    /// `prompt_hash` alone. Emit `CALL_RECOVERED`, use the cached response,
    /// commit the actual cost.
    Recover,
    /// Cache miss, the adapter declares `idempotency: Some(_)`. Retry with
    /// `blake3(prompt_hash ‖ reservation_id)`; the reservation stays held across
    /// the retry, never released and re-reserved.
    RetryWithIdempotencyKey,
    /// Cache miss, `idempotency: None` — the Anthropic default. **Never
    /// retries.** Mark `ORPHANED`, hold the reservation, degrade the stage via
    /// `SkipItem`. `ORPHANED` must never become `FAILED`.
    Orphan,
    /// `RESERVED`, nothing to retry — release and fail freely (mirrors
    /// [`classify_on_resume`]'s `ReleaseAndFail`, reachable here too since a
    /// caller may route both tables through this single decision point).
    ReleaseAndFail,
}

pub fn decide_retry(
    state: CallState,
    cache_hit: bool,
    idempotency: Option<IdempotencyStyle>,
) -> RetryDecision {
    match state {
        CallState::Reserved => RetryDecision::ReleaseAndFail,
        CallState::Sent | CallState::Acknowledged => {
            if cache_hit {
                RetryDecision::Recover
            } else if idempotency.is_some() {
                RetryDecision::RetryWithIdempotencyKey
            } else {
                RetryDecision::Orphan
            }
        }
        CallState::Completed
        | CallState::Retryable
        | CallState::Failed
        | CallState::Orphaned
        | CallState::Recovered => RetryDecision::ReleaseAndFail, // nothing pending to retry
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{from:?} cannot transition to {to:?}")]
pub struct InvalidTransition {
    pub from: CallState,
    pub to: CallState,
}

/// Guards the transition diagram in ARCHITECTURE §8.4. The one edge that must
/// never exist, named explicitly in the spec's own prose: "`ORPHANED` must never
/// become `FAILED`" — orphaned money may already be spent, and silently
/// reclassifying it as "never billed" would double-release funds that were
/// never confirmed safe to release.
pub fn validate_transition(from: CallState, to: CallState) -> Result<(), InvalidTransition> {
    use CallState::*;
    let allowed = matches!(
        (from, to),
        (Reserved, Sent)
            | (Reserved, Failed)
            | (Sent, Acknowledged)
            | (Sent, Retryable)
            | (Sent, Failed)
            | (Sent, Orphaned)
            | (Acknowledged, Completed)
            | (Acknowledged, Retryable)
            | (Acknowledged, Failed)
            | (Acknowledged, Orphaned)
            | (Retryable, Sent)
            | (Orphaned, Recovered)
    );
    if allowed {
        Ok(())
    } else {
        Err(InvalidTransition { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_classification_matches_the_spec_table_exactly() {
        assert_eq!(
            classify_on_resume(CallState::Reserved),
            ResumeAction::ReleaseAndFail
        );
        assert_eq!(
            classify_on_resume(CallState::Sent),
            ResumeAction::HoldOrphaned
        );
        assert_eq!(
            classify_on_resume(CallState::Acknowledged),
            ResumeAction::HoldOrphanedReconcilable
        );
        assert_eq!(
            classify_on_resume(CallState::Completed),
            ResumeAction::NoAction
        );
    }

    #[test]
    fn no_retry_without_idempotency() {
        let decision = decide_retry(CallState::Sent, false, None);
        assert_eq!(decision, RetryDecision::Orphan);

        let decision = decide_retry(CallState::Acknowledged, false, None);
        assert_eq!(decision, RetryDecision::Orphan);
    }

    #[test]
    fn retry_only_happens_with_idempotency_and_no_cache_hit() {
        let style = Some(IdempotencyStyle::Header("Idempotency-Key".to_string()));
        assert_eq!(
            decide_retry(CallState::Sent, false, style.clone()),
            RetryDecision::RetryWithIdempotencyKey
        );
        // A cache hit wins even when idempotency is available -- no need to
        // spend twice when the answer is already known.
        assert_eq!(
            decide_retry(CallState::Sent, true, style),
            RetryDecision::Recover
        );
    }

    #[test]
    fn orphaned_never_becomes_failed() {
        assert!(validate_transition(CallState::Orphaned, CallState::Failed).is_err());
        // The only valid way out of Orphaned is Recovered.
        assert!(validate_transition(CallState::Orphaned, CallState::Recovered).is_ok());
    }

    #[test]
    fn every_disallowed_edge_is_rejected_not_silently_allowed() {
        use CallState::*;
        // A spot-check of edges the diagram does not draw.
        assert!(validate_transition(Reserved, Completed).is_err());
        assert!(validate_transition(Completed, Sent).is_err());
        assert!(validate_transition(Failed, Sent).is_err());
        assert!(validate_transition(Recovered, Orphaned).is_err());
    }
}
