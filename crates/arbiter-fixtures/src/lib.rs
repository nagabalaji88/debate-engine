//! Golden fixtures: 36 CI cases against the scripted mock provider, zero LLM
//! tokens (ARCHITECTURE.md §18). Each fixture is owned by exactly one
//! implementation task — see IMPLEMENTATION_PLAN.md §8 for the ledger.
#![forbid(unsafe_code)]

pub mod harness;
