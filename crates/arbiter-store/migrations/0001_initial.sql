-- run.db initial schema. ARCHITECTURE §8.1, §8.5, §8.7.
--
-- Scope note (PLAN_DEVIATIONS.md D21, updated by D29): §8.1's table lists ~15
-- projection tables by name only -- neither spec file gives most of them a
-- column list. `events` (ARCHITECTURE §9's JSON envelope), `run` (INTERFACES
-- §1's literal INSERT/UPDATE statements), and `schema_metadata` (ARCHITECTURE
-- §8.7's explicit column list) had a complete, spec-given shape from the start.
-- `budget`, `provider_calls`, `cache_entries` and `artifacts` (S4, D29) add the
-- four whose write paths INTERFACES §5's crash-recovery sequence and §8.3's SQL
-- examples pin precisely enough to implement. `stages` and the ten claim-graph/
-- decision projections (`positions`, `claims`, `claim_relations`, `disputes`,
-- `challenges`, `rebuttals`, `judge_evaluations`, `decision`,
-- `decision_triggers`, `provenance`) stay deferred to G2-G9, whose own stage
-- implementations are what will pin their real payload shapes -- inventing
-- those columns now, before any stage exists to need them, risks designing
-- against a shape the real stage code then has to work around.

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

-- The budget ledger's persisted half (§8.3, §7). Exactly one row, seeded below
-- -- `run_id` is a constant within a run.db (§8.1's own reasoning for omitting
-- it from `events`), so a PRIMARY KEY column would only ever hold one value.
-- `reserved`/`committed` are updated transactionally alongside the
-- `provider_calls` row and event(s) each movement fires with (§8.3's own SQL:
-- "UPDATE budget SET committed = committed + ?, reserved = reserved - ?").
CREATE TABLE budget (
    reserved  REAL NOT NULL DEFAULT 0,
    committed REAL NOT NULL DEFAULT 0
);
INSERT INTO budget (reserved, committed) VALUES (0, 0);

-- One row per call attempt (INTERFACES §5's crash-recovery write order).
-- `call_id` is the primary key (`arbiter_kernel::ids::CallId`'s own doc
-- comment: "the key ... the provider_calls table [is] keyed on"); `reservation_id`
-- is a column, not the key, because a retry against an idempotent provider
-- shares one reservation across more than one call_id (INTERFACES §5: "the
-- reservation stays HELD across the retry"). `state` stores `CallState`'s own
-- SCREAMING_SNAKE_CASE JSON form, matching `events.event_type`'s convention.
-- `actual_cost`/`request_id` are NULL until COMPLETED/ACKNOWLEDGED respectively.
CREATE TABLE provider_calls (
    call_id         TEXT PRIMARY KEY,
    reservation_id  TEXT NOT NULL,
    state           TEXT NOT NULL,
    reserved_amount REAL NOT NULL,
    actual_cost     REAL,
    request_id      TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX ix_provider_calls_reservation ON provider_calls(reservation_id);

-- `(provider, model, params, prompt_hash) -> response`, INTERFACES §5: "never
-- prompt_hash alone: the same prompt sent to two models has one prompt_hash and
-- two different answers." `inline` is NULL exactly when the payload moved to the
-- blob store above `blob_threshold` (§8.2) -- `arbiter_kernel::store::CachedResponse`'s
-- own field, carried straight through.
CREATE TABLE cache_entries (
    provider      TEXT NOT NULL,
    model         TEXT NOT NULL,
    params        TEXT NOT NULL,
    prompt_hash   TEXT NOT NULL,
    response_hash TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL,
    inline        TEXT,
    PRIMARY KEY (provider, model, params, prompt_hash)
);

-- Content-addressed stage output (INTERFACES §6: "Artifacts are content-addressed,
-- serde-typed, and versioned"). `artifact_id` is `arbiter_kernel::store::Artifact::content_hash()`;
-- `payload` is its `to_json()` form, canonical JSON text. Idempotent on
-- `put_artifact` -- identical content hashes identically, so a re-put of the
-- same artifact is a no-op, never a conflict (blob.rs's own write_blob takes the
-- same stance for the same reason).
CREATE TABLE artifacts (
    artifact_id   TEXT PRIMARY KEY,
    artifact_type TEXT NOT NULL,
    payload       TEXT NOT NULL,
    created_at    TEXT NOT NULL
);
