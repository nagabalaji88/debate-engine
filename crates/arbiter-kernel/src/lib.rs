//! Orchestration: StageGraph, the budget ledger, the provider-call state machine,
//! bounded concurrency, and everything that touches the outside world on the
//! engine's behalf (ARCHITECTURE.md §7, §11).
//!
//! Depends on `arbiter-core` only, for the decision types it operates over. This
//! crate *defines* the `Store` and `Provider` trait seams (INTERFACES §1, §5);
//! `arbiter-store` and `arbiter-providers` depend on it to implement them, never
//! the reverse (PLAN_DEVIATIONS.md D1).
#![forbid(unsafe_code)]

pub mod bounds;
pub mod budget;
pub mod cache;
pub mod calls;
pub mod event;
pub mod ids;
pub mod prompt;
pub mod provider;
pub mod stage;
pub mod store;
