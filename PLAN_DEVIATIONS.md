# Plan deviations

Per `IMPLEMENTATION_PLAN.md` §0.1: when this plan and the spec disagree, the spec wins
and the plan has a bug. Logged here, in execution order.

---

## D1 — kernel/store dependency direction (task X1)

**What the plan's own task table implied.** `IMPLEMENTATION_PLAN.md` §2 listed
`K1 · Budget ledger` as depending on `S3 · Event append, hash chain`, and K2/K3/K5
similarly depend on S-tasks. Read naively, that puts `arbiter-store` upstream of
`arbiter-kernel` in the crate graph.

**What ARCHITECTURE.md §4.1 actually says.** *"kernel depends on core; everything else
depends on kernel."* And the crate table is explicit about the direction: `arbiter-kernel`
owns "event store" (i.e. the trait); `arbiter-store` is "the SQLite **implementation** of
the Store traits." Store depends on kernel, never the reverse — kernel must not know
SQLite exists, so a second `Store` implementation (or a test double) never has to touch
`arbiter-store` to exist.

**Caught by:** attempting the X1 scaffold with `arbiter-kernel` depending on
`arbiter-store`, which would have made X2's dependency-rule test fail on first write.

**Fix applied in X1:**
- `arbiter-kernel/Cargo.toml` depends on `arbiter-core` only.
- `arbiter-store/Cargo.toml` depends on `arbiter-core` **and** `arbiter-kernel`.
- The `RunStore` / `RunWriter` / `Tx` / `RunReader` trait signatures (INTERFACES §1) are
  defined **in `arbiter-kernel`**, not in `arbiter-store`. `arbiter-store` provides the
  SQLite-backed implementation.

**Consequence for the task table:** read every `K* depends on S*` edge in §2 as backwards.
The correct direction is: kernel tasks that need a store define the trait they need
in-crate (no external dependency); store tasks then implement that trait and therefore
depend on the kernel task that defined it. When executing K1/K2/K3/K5, do not add
`arbiter-store` as a dependency to satisfy them — write the trait, or a temporary
in-memory test double, inside `arbiter-kernel` itself, and let `arbiter-store` catch up.

---

## D2 — `wasmtime` not yet a workspace dependency

`ARCHITECTURE.md` §16 names `wasmtime` for the WASM plugin host. That host is explicitly
1.5 (`IMPLEMENTATION_PLAN.md` §9, "explicitly out of this plan"). Adding the dependency
now, unused, would slow every `cargo build`/`clippy` invocation for the rest of 1.0 for no
present benefit — it violates §0.2 rule 2 ("do not widen scope"). Add it when the WASM
host task actually starts, not before.
