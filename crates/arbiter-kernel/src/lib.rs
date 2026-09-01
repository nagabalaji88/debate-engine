//! Orchestration: StageGraph, the budget ledger, the provider-call state machine,
//! bounded concurrency, and everything that touches the outside world on the
//! engine's behalf (ARCHITECTURE.md §7, §11). Depends on `arbiter-core` for the
//! decision types it operates over, and `arbiter-store` for durability.
#![forbid(unsafe_code)]
