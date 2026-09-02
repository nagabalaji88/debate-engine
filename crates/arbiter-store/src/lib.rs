//! Persistence: `run.db` per run, a `history.db` catalogue, and a filesystem blob
//! store for payloads above the threshold (ARCHITECTURE.md §8).
//!
//! `events` is the source of truth; every other table is a projection rebuilt from
//! it on replay (§8.1). This crate implements the `Store` trait seam `arbiter-kernel`
//! defines — dependency direction is store -> kernel -> core, never the reverse,
//! so the orchestration engine never has to know SQLite exists.
#![forbid(unsafe_code)]

pub mod blob;
pub mod catalog;
pub mod events;
pub mod lease;
pub mod project;
pub mod schema;
pub mod sqlite_store;

/// The current instant, RFC3339 (`2026-08-31T12:04:11.221Z`) — the format
/// ARCHITECTURE §9's `Event` envelope and every `run`/`schema_metadata` timestamp
/// column use.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting of the current time cannot fail")
}
