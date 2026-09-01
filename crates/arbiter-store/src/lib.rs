//! Persistence: `run.db` per run, a `history.db` catalogue, and a filesystem blob
//! store for payloads above the threshold (ARCHITECTURE.md §8).
//!
//! `events` is the source of truth; every other table is a projection rebuilt from
//! it on replay (§8.1). This crate implements the `Store` trait seam `arbiter-kernel`
//! defines — dependency direction is store -> kernel -> core, never the reverse,
//! so the orchestration engine never has to know SQLite exists.
#![forbid(unsafe_code)]
