//! Loads and confines third-party code (ARCHITECTURE.md §10). Two tiers: JSON-RPC
//! subprocess (TRUSTED, best-effort confinement) and WASM (SANDBOXED, runtime-
//! enforced). Every plugin is labelled one or the other — never implicit.
//!
//! Ships in phase 1.5 (§19). This crate exists in the workspace now so the ABI
//! surface can be exercised by tests without waiting for the host to be built.
//!
//! No `wasmtime` dependency yet: it is added when the WASM host task starts, not
//! before — an unused heavy dependency slows every build in the meantime.
#![forbid(unsafe_code)]
