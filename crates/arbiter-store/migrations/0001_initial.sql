-- run.db initial schema. ARCHITECTURE §8.1, §8.5, §8.7.
--
-- Scope note (PLAN_DEVIATIONS.md D21): §8.1's table lists ~15 more projection
-- tables (`stages`, `provider_calls`, `budget`, `positions`, `claims`,
-- `claim_relations`, `disputes`, `challenges`, `rebuttals`, `judge_evaluations`,
-- `decision`, `decision_triggers`, `provenance`, `cache_entries`, `artifacts`) by
-- name only -- neither spec file gives any of them a column list. Only the three
-- tables below have a complete, spec-given shape: `events` (ARCHITECTURE §9's JSON
-- envelope), `run` (INTERFACES §1's literal INSERT/UPDATE statements), and
-- `schema_metadata` (ARCHITECTURE §8.7's explicit column list). The rest are
-- deferred to the tasks that actually read/write them (K1 budget ledger, K2
-- provider-call state machine, K5 response cache, S4 projections + rebuild), where
-- real code using each table is what should pin its exact columns -- not this one.

-- The source of truth. Append-only, hash-chained, never rebuilt (§8.1).
-- `seq INTEGER PRIMARY KEY` is a SQLite rowid alias: rows are physically stored in
-- `seq` order, so `ORDER BY seq` is a sequential scan, not a sort (§8.1, §8.7).
-- No compound (run_id, seq) index: run_id is constant within a run.db and such an
-- index would only duplicate the primary key (explicit instruction, §8.1).
CREATE TABLE events (
    seq                 INTEGER PRIMARY KEY,
    run_id              TEXT NOT NULL,
    schema_version      INTEGER NOT NULL,
    event_id            TEXT NOT NULL UNIQUE,
    timestamp           TEXT NOT NULL,
    stage               TEXT NOT NULL,
    event_type          TEXT NOT NULL,
    durable             INTEGER NOT NULL,
    payload             TEXT NOT NULL,
    content_hash        TEXT NOT NULL,
    previous_event_hash TEXT
);

-- One row: the run's lease and owner metadata (INTERFACES §1). `lease_epoch`
-- starts at 1 and is the compare-and-swap target on `reopen` -- liveness only
-- decides whether a steal is *permitted*; the epoch CAS decides who *wins*.
CREATE TABLE run (
    run_id         TEXT PRIMARY KEY,
    owner_pid      INTEGER NOT NULL,
    boot_id        TEXT NOT NULL,
    hostname       TEXT NOT NULL,
    started_at     TEXT NOT NULL,
    engine_version TEXT NOT NULL,
    lease_epoch    INTEGER NOT NULL
);

-- Store metadata, updated by migrations, not a projection of `events` (§8.1).
-- `db_schema_version` -- never `schema_version`, which already means the event
-- envelope's own version (§8.7, §9); reusing the name across two axes is how a
-- migration ends up gated on an event version.
CREATE TABLE schema_metadata (
    db_schema_version INTEGER NOT NULL,
    engine_version    TEXT NOT NULL,
    created_at        TEXT NOT NULL
);
