//! Stage 1: `init`'s pure half — question validation (ARCHITECTURE §5: "validate
//! question, snapshot config and prompt pack hash, seed RNG, open log").
//!
//! The concrete "open the run and append `RUN_STARTED`" orchestration needs both
//! `RunStore`'s concrete implementation and the hash-chaining machinery that
//! computes a correctly linked event (`arbiter_store::events::ChainState`/
//! `append_chained`) — both live in `arbiter-store`, which this crate cannot
//! depend on (D1) — so that half is implemented there, in `arbiter_store::init`,
//! not here.

/// ARCHITECTURE §5: "validate question." Neither spec file states an upper
/// bound, only that a question must exist to debate at all — rejecting
/// empty/whitespace-only input is the conservative minimum `init`'s own name
/// implies, not an invented length rule (PLAN_DEVIATIONS.md D30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the question is empty")]
pub struct EmptyQuestion;

pub fn validate_question(question: &str) -> Result<(), EmptyQuestion> {
    if question.trim().is_empty() {
        return Err(EmptyQuestion);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_question_validates() {
        assert!(validate_question("Should we adopt microservices?").is_ok());
    }

    #[test]
    fn an_empty_question_is_rejected() {
        assert_eq!(validate_question(""), Err(EmptyQuestion));
    }

    #[test]
    fn a_whitespace_only_question_is_rejected() {
        assert_eq!(validate_question("   \n\t  "), Err(EmptyQuestion));
    }
}
