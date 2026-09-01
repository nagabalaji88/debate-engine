# Arbiter — Interface Definitions

**Companion to** `ARCHITECTURE.md` v2.7. Where the spec says *what*, this says *how*.
Every item here closes a numbered finding from the v2.0 review.

---

## 1. Storage concurrency model  *(v2.7 — was "filesystem concurrency model")*

**The model: SQLite owns exclusion; the traits never mention a file.**

The v2.0 trait leaked its implementation — it spoke of creating directories, acquiring
lock files and tolerating torn tails. A second backend would have had to emulate
filesystem semantics it does not have, which is how a "stable, public" plane turns out
to describe one implementation rather than abstract over any. Reshaping it is a
prerequisite for the SQLite store, not a follow-up.

| Path | Writers | Protocol |
|---|---|---|
| `runs/<id>/run.db` | one, for the life of the run | SQLite write lease; no lock file |
| `runs/<id>/blobs/*` | the same single owner | write blob → fsync → commit row |
| `history.db` | any process, twice per run | WAL + `busy_timeout`; two short transactions |

```rust
pub trait RunStore: Send + Sync {
    /// Opens a new run for writing. `AlreadyOpen` if another process holds the lease.
    fn create(&self, run_id: &RunId, manifest: &Manifest) -> Result<Box<dyn RunWriter>, StoreError>;
    /// Re-opens an existing run for writing, for `resume`.
    fn reopen(&self, run_id: &RunId) -> Result<Box<dyn RunWriter>, StoreError>;
    /// Concurrent reader. Never blocks the writer; never observes a partial commit.
    fn reader(&self, run_id: &RunId) -> Result<Box<dyn RunReader>, StoreError>;
}

pub trait RunWriter: Send {
    /// Everything inside the closure commits, or none of it does.
    fn transact<T>(
        &mut self,
        f: &mut dyn FnMut(&mut dyn Tx) -> Result<T, StoreError>,
    ) -> Result<T, StoreError>;
}

pub trait Tx {
    fn append_event(&mut self, e: &Event) -> Result<Sequence, StoreError>;
    fn put_artifact(&mut self, a: &Artifact) -> Result<ArtifactId, StoreError>;
    fn put_cache(&mut self, k: &CacheKey, r: &CachedResponse) -> Result<(), StoreError>;
    fn commit_budget(&mut self, r: &ReservationId, actual: Cost) -> Result<(), StoreError>;
    fn set_call_state(&mut self, c: &CallId, s: CallState) -> Result<(), StoreError>;
}

pub trait RunReader: Send {
    /// Always ordered by sequence. SQL has no inherent row order and a
    /// byte-for-byte DecisionRecord cannot depend on one that happens to hold.
    fn events(&self) -> Result<Box<dyn Iterator<Item = Event>>, StoreError>;
    fn verify_chain(&self) -> Result<ChainStatus, StoreError>;
}
```

Nothing in those signatures names a directory, a lock, a flush or a torn tail. That is
the test: if a signature could not be implemented over Postgres or an object store
without lying, it is still leaking.

**Two writers to one run.** `create` and `reopen` fail with `AlreadyOpen` rather than
blocking, so a second `arbiter run` on the same id reports it instead of hanging behind
`busy_timeout`. Owner metadata — pid, boot_id, hostname, started_at, engine_version —
lives in the `run` table and is updated on open, so `doctor` can still name the holder.
A stale lease is one whose recorded pid is dead; there is no mtime heuristic any more
because there is no lock file to age.

**Readers.** A reader opens read-only in WAL mode: it never blocks the writer, never
blocks on the writer, and by construction never sees a partially committed stage. The
"tolerate a torn tail" clause is gone — the condition it defended against cannot occur.
`verify_chain` remains, because a committed row can still have been altered afterwards
by something that is not the engine, and that is what the chain detects.

**`history.db`.** One insert at run start (status `running`), one update at completion.
Both are single-statement transactions; WAL keeps readers unblocked. Contention exists
and is measured in microseconds, but it exists — unlike the v2.0 design, concurrent runs
are no longer fully independent, and §8.5 says so. A row left in `running` is the signal
`arbiter resume` and `arbiter doctor` both read.

**Rebuilding `history.db`.** `arbiter reindex` scans run directories, opens each `run.db`
read-only, and upserts one row. No watermark, no delta pass, no lock choreography —
those existed because an append-only text index could not be updated in place. It is now
a scan and an upsert, and losing `history.db` costs a rebuild, never a run.

**Migrations.** `migrations/NNNN_*.sql` applied in order and recorded in
`schema_metadata`. Opening a store whose `db_schema_version` exceeds the binary's is
refused, not guessed at. A migration that cannot be expressed as a projection rebuild
from `events` is a design smell: the log is the truth and the tables are derived, so
almost every schema change should be *drop the projection and replay*.

---

## 2. Claim grounding and the repair protocol  *(review #2)*

The extractor returns, per claim, a `grounding_hint`:

```jsonc
{ "text": "…", "kind": "fact", "grounding": { "quote": "exact substring from my position" } }
{ "text": "…", "kind": "inference", "grounding": { "derived_from": ["#1", "#4"] } }
```

**Validation is mechanical, in this order:**

1. **Exact match** — whitespace- and case-normalised substring search in the position
   text. Hit ⇒ `Grounding::DirectQuote{span}`.
2. **Fuzzy match** — trigram Jaccard ≥ 0.85 over a sliding window the length of the
   quote. Hit ⇒ `DirectQuote` with the matched span.
3. **Derived** — every premise must resolve to an already-accepted claim in the same
   position, and the premise graph must be acyclic. Hit ⇒ `Grounding::Derived`.

   **Cycle enforcement is explicit, not assumed.** After extraction and before
   `relations.analyze`, the premise graph of each position is topologically sorted
   (Kahn). A model can emit *A derived from B, B derived from A* — adversarially or by
   accident — and nothing downstream would catch it.

   **Untangle before degrading.** Collapsing the whole strongly-connected component to
   0.15 would punish a verifiable fact for a bogus derivation edge, and a fact dropping
   from 1.00 to 0.15 can collapse an otherwise sound option in scoring. So degradation
   is the last step, not the first:

   ```
   cycle detected in SCC
     1. repair    the position's repair call (step 4) carries the cycle:
                  "these claims cite each other as premises — name the base premise,
                   or mark them independent"                    ≤1 call, already budgeted
     2. cut       still cyclic → remove the minimum set of derivation edges that
                  restores acyclicity. Exact for |SCC| ≤ 12, otherwise greedy by
                  ascending extractor confidence
     3. re-check  re-run grounding on every affected claim
                    still has a verified DirectQuote → keeps its EvidenceKind
                    sole grounding was a cut edge   → Grounding::Unsupported (0.15)
   ```

   `PREMISE_CYCLE_DETECTED { component, edges_cut, degraded, retained }` records exactly
   what happened, so a reader can see that C-011 kept `Fact` while C-019 fell to
   `Unverified`. Fixture: `premise_cycle_grounded_fact`.

   Note the distinction: **premise cycles are malformed extraction; relation cycles are
   not.** `A contradicts B contradicts A` is ordinary dialectic and is handled by the
   fixpoint's damping (§12), not rejected.
4. **Repair** — one extra call *per position* (never per claim), carrying only the
   failed claims plus the position text: *"return the exact substring supporting each,
   or mark it as an inference and name its premises."*
5. **Admit** — still failing ⇒ `Grounding::Unsupported`, `EvidenceKind::Unverified`
   (weight 0.15), event `CLAIM_UNGROUNDED`.

**Repair runs on `repair_model`, not on the claim's author.** The task is *"find the
substring of this text that supports this claim"* — extraction over fixed text, where the
position is the source of truth and the author's identity is irrelevant. Paying the
author's rate for it is pure waste, and the spread is wide enough to matter:

```
15 repair calls (5 models × deep) ≈ 45k in / 30k out
  cheapest tier   ≈ $0.20    10% of the $2.00 cap
  mid tier        ≈ $0.40    20%
  frontier tier   ≈ $0.99    50%
```

`repair_model` therefore defaults to the cheapest model the configured providers expose,
and `repair_budget_fraction` (default 0.15) caps cumulative repair spend independently of
the call count — whichever binds first stops repairs, and remaining failures are admitted
as `Unsupported`.

**Bounds.** `max_repair_calls_per_position = 1`, `max_repair_output_tokens = 2048`,
`max_repair_calls_per_run = panel_size`, `repair_budget_fraction = 0.15` (config). The repair call is budget-reserved
like any other, so it cannot be a hidden cost sink: worst case is one extra small call
per model per round.

This is also what distinguishes *"our team has 8 developers"* from *"therefore a
modular monolith is safer"* — the first survives step 1 or 2, the second must declare
premises at step 3 or it is admitted at Unverified weight.

---

## 3. Similarity: lexical in phase 1, no embeddings  *(review #3)*

The review is right that the spec contradicted itself. Resolution: **phase 1 ships no
embedding model and no vector store.** The cheap stage is purely lexical:

Pure lexical blocking has a real recall hole, and the review named it precisely:
*"Kubernetes deployment overhead"* and *"container orchestration maintenance workload"*
share almost no tokens and no useful character n-grams either, yet they are the same
claim. Missing the pair produces two canonical claims for one point, which then dilutes
`independence` and `corroboration` — the failure is silent and it corrupts the decision
arithmetic, not just the display.

**Candidate generation is therefore a union of three tiers, not one filter:**

```
T1  lexical      normalise → trigrams → 64-bit SimHash blocking
                 → IDF-weighted cosine → top-K per claim (K scales)     always on, free
T2  polarity     every cross-model pair attached to opposing options    always on, free
T3  batch-LLM    ONE call over all claims on a cheap model:             default on
                 "group claims that state the same underlying point"    ~3k in / 1k out
                                                                        ≈ $0.01
candidates = T1 ∪ T2 ∪ T3  →  LLM pair classification
```

T3 is what closes the synonymy hole. It is **one call for the whole claim set**, not
O(n²) — the original objection to "use an LLM for similarity" was about pair-wise cost,
which a single batched grouping call does not incur. On a 32-claim debate it costs about
a cent and catches paraphrase far better than a small local embedding model would.

**Why not a local ONNX embedding sidecar in phase 1** (the review's suggestion): it adds
a native build dependency (`ort`), a 23–90 MB model to distribute and version, and
float-level output drift across runtime versions — and MiniLM-class embeddings are
weaker at *"is this the same claim"* than a Haiku-class model reading the claims. It is
supported as a plugin for users who want offline or zero-LLM-cost blocking, not as the
default.

```rust
pub trait Similarity: Send + Sync {          // internal plane
    fn candidates(&self, claims: &[CanonicalClaim], k: usize) -> Vec<CandidatePair>;
}

pub struct SimilarityStack {                 // union of enabled tiers
    lexical: LexicalSimilarity,              // always
    polarity: PolaritySweep,                 // always
    batch_llm: Option<BatchGrouping>,        // default Some
    plugin: Option<Box<dyn Similarity>>,     // e.g. arbiter-similarity-embed
}
```

**T3 partitions above 60 claims.** A single prompt carrying 150+ claims dilutes
attention and risks a truncated structured response — the failure would be silent claim
loss, which is exactly what T3 exists to prevent. Two-level batching:

```
n ≤ t3_max_claims_per_batch (60)      one call, as before
n >  t3_max_claims_per_batch
   1. partition   union-find over the T1/T2 candidate graph → connected components
   2. pack        first-fit-decreasing components into batches ≤ 60 claims
                  and ≤ t3_max_input_tokens (8k, estimated at ~40 tok/claim)
   3. group       one call per batch
   4. stitch      one final call over the group representatives (one canonical text
                  per group, typically ≤ n/3) → restores cross-batch synonymy
```

**The stitch pass recurses when it has to.** At n = 300 the representatives alone are
~100, over the batch limit, so a single stitch call would overflow exactly as a single
grouping call would:

```
stitch(reps):
    reps ≤ 60            → one call, done
    reps >  60           → partition reps by T1/T2 components, stitch each,
                           then stitch the resulting representatives   (depth ≤ 2)
    still > 60 at depth 2 → T1/T2 only for the remainder,
                            emit CANDIDATES_SELECTED { tier: "stitch_depth_exceeded" }
```

Depth 2 covers ~3,600 claims, far past any realistic debate; the fallback exists so the
algorithm is total rather than because the branch is expected. The intermediate case is
the ordinary one and needs no fallback: 200 claims yielding 80 components produces 80
representatives, over the 60 limit, so the stitch simply recurses one level and completes.

**The distribution is measured, not assumed.** `CANDIDATES_SELECTED` carries
`{ components, sizes_p50, sizes_p95, batches, stitch_depth }`, and `stitch_depth_exceeded`
is a tracked rate. If it fires on more than 5% of runs the partition constant is wrong,
not the algorithm — that is a tuning signal with a threshold, rather than a hope.

Calls stay `O(n/60 + 1)`. A batch whose structured response truncates falls back to
T1/T2 for that batch and emits `CANDIDATES_SELECTED { tier: "t1t2_fallback", batch }` —
degraded recall, never a dropped claim. Fixture: `t3_batch_partition` (180 claims).

**Grouping is biased toward splitting.** T3's two failure modes are not symmetric: a
*wrong merge* fuses two distinct claims into one canonical claim, inflating its
`independence` and `corroboration` and corrupting the decision arithmetic silently. A
*missed merge* leaves two claims that each carry a share of the evidence — dilution,
visible in the record, recoverable. So groups carry a confidence, groups below
`t3_merge_threshold` (0.75) are split rather than merged, and the grouping prompt is
written to prefer "not sure — keep separate". `paraphrase_corpus` measures what that costs in recall against a hand-labelled set; the
threshold is tuned against it, not guessed — and 0.75 is a **starting point, not a
discovered constant**:

| Property | Requirement |
|---|---|
| Size | ≥ 300 claim pairs, ≥ 100 of them true paraphrases with low lexical overlap |
| Labelling | two independent labellers; disagreements adjudicated, not dropped |
| Objective | maximise F1 **subject to merge-precision ≥ 0.95** — the asymmetry above, expressed as a constraint |
| Domains | ≥ 3, since paraphrase density differs sharply between e.g. architecture and policy text |
| Status | living dataset, versioned with the table; threshold stays config-overridable |

A corpus below that size cannot distinguish 0.75 from 0.80, and a threshold tuned on one
domain will not transfer.

**K scales with the claim count** rather than being a hard-coded quality threshold:

```
K = clamp( ceil(k_factor · log2(n + 1)), k_min, k_max )
k_factor = 3.0 · k_min = 8 · k_max = 24 · global cap max_candidate_pairs = 2000

n =  12 → 11      n =  32 → 16      n = 100 → 20      n = 300 → 24 (capped)
```

A fixed K=12 is a recall bottleneck at 100 claims and wasteful at 12. All four values
are config, so a policy plugin can retune them without touching a stage.

**Replay determinism is protected by recording, not by purity.** The selected pair set
is written to the log as `CANDIDATES_SELECTED { pairs, tier }`, and exact replay reads
those pairs rather than recomputing them. Neither LLM non-determinism in T3 nor float
drift in a future embedding plugin can change a replayed decision.

---

## 4. What the judge actually sees  *(review #4)*

The review found a real hole: Counterargument Handling cannot be scored from final
text alone. The judge receives a **dossier per position**, not a position:

```
POSITION C  (pseudonym, stable across the dossier)
  recommendation + reasoning        (surface-normalised)
  claims           C-011 fact · C-018 inference · …
  exchanges        challenge received → verbatim rebuttal → lifecycle outcome
                   (Defended / Modified v2 / Withdrawn)
```

So Counterargument Handling is scored from observed exchanges, and the claim lifecycle
gives a mechanical cross-check on the judge's score.

**Surface normalisation before judging:** markdown tables → plain rows, headings
stripped, bullet glyphs unified, length not truncated. This reduces style fingerprints;
it does not eliminate them.

**Residual risk, stated plainly.** A judge model may still infer authorship from style,
and normalisation cannot prevent that. Three things bound the damage rather than deny
it: the judge scores exchanges rather than picking a winner; its weighted score is one
term at 0.35 of confidence, never the decision; and `judge_count > 1` across vendors is
supported when the risk matters. The `judge_identity_leakage` fixture measures it —
same debate, model names swapped, scores compared.

---

## 5. Crash recovery and in-flight calls  *(review #5)*

**Write order around every provider call:**

```
1. TX: CALL_STARTED{call_id, prompt_hash, reservation_id, estimate}   commit  → SENT
       + provider_calls row, state SENT
2. …provider call; response headers arrive…
3. TX: CALL_REQUEST_ID{call_id, request_id}                           commit  → ACKNOWLEDGED
4. …body received into memory (or the blob store above blob_threshold, fsynced)…
5. TX: CALL_COMPLETED{call_id, response_hash, actual_cost}
       + cache_entries row
       + budget: reserved → committed
       + provider_calls state COMPLETED                               commit  → COMPLETED
```

Steps 1 and 3 are **separate committed transactions**, not an optimisation to collapse.
A crash between them is the whole reason `CALL_ORPHANED` exists: the request left the
machine and may have been billed, and only a committed `request_id` makes that charge
reconcilable against a usage export. Folding step 3 into step 1 would mean recording an
id the provider has not issued yet; folding it into step 5 would mean losing it in
exactly the crash it was written for.

Step 5 is where SQLite earns the migration. The response, the money and the events land
in **one transaction** — the interleavings the v2.0 design had to reason about no longer
exist. What it cannot do is reach outside the machine: a crash after step 2 and before
step 3 leaves a call that may have been billed with no id to reconcile it by, which is
`ORPHANED` and must never be reopened as `FAILED`.

**On `resume`, for every `CALL_STARTED` with no `CALL_COMPLETED`:**

```
cache hit on prompt_hash  → emit CALL_RECOVERED, use the response, commit actual cost
cache miss                → release the reservation, emit CALL_ORPHANED, retry
                            (bounded by max_retries)
```

Under v2.7 the cached response and the completion event commit **together**, so the
v2.0 rule that the cache must be written first no longer applies — there is no window
between them to recover across.

**Narrowing the exposure window.** Three mechanisms, in order of how much they actually
buy:

1. **Commit the response with the event.** A crash before the commit leaves no trace
   of the call beyond its `SENT`/`ACKNOWLEDGED` rows, which is the honest state: the
   money may be spent and the response is gone. A crash after it leaves a complete,
   usable cache entry. There is no third outcome any more — which is the single largest
   simplification the SQLite store buys, since the `.part`-rename window it replaces had
   four distinct recoverable states and one that was merely reportable.
2. **Record the provider request id as its own event.** Response headers carry a
   request identifier (`request_id` on Anthropic); `CALL_REQUEST_ID` is appended the
   moment they arrive, before the body finishes, so an orphaned call is reconcilable
   against the provider's usage export afterwards.
3. **Send an idempotency key where the provider supports one** —
   `blake3(prompt_hash ‖ reservation_id)`, stable across retries of the same logical
   call and distinct between calls. This is **capability-gated, not assumed**:

```rust
pub struct ProviderCapabilities {
    pub structured_output: bool,
    pub streaming: bool,
    pub idempotency: Option<IdempotencyStyle>,   // None = unsupported
}
pub enum IdempotencyStyle { Header(&'static str) }
```

Each adapter declares its own support from that provider's documentation; the kernel
sends the key only when `Some`. At the time of writing, the Anthropic Messages API
reference bundled with this project documents no idempotency header, so the Anthropic
adapter ships `idempotency: None` until that changes. Several OpenAI-compatible
gateways do accept `Idempotency-Key`, which is exactly why this is a per-adapter
capability rather than a global assumption.

**Honest limit, unchanged.** If a provider served and billed a call whose response never
reached disk and that provider offers no idempotency key, the money is spent and Arbiter
cannot recover it. What the engine guarantees is no *duplicated work* on a cache hit, a
recorded `request_id` for after-the-fact reconciliation, and an honest ledger:
`orphaned_reservations` are reported separately rather than quietly absorbed.

**Idempotency key** for every stage:

```
blake3(stage_name ‖ engine_version ‖ config_hash ‖ round ‖ input_artifact_hashes)
```

Pure stages (`claims.normalize`, `relations.analyze` given fixed inputs,
`disputes.rank`, `decision.synthesize`) re-run freely; provider stages consult the
cache first.

---

## 6. StageGraph semantics  *(review #7)*

```rust
pub trait Stage: Send + Sync {
    type In: Artifact;
    type Out: Artifact;

    fn name(&self) -> StageName;
    fn parallelism(&self) -> Parallelism;             // Serial | PerItem { max: usize }
    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key;
    fn cost_estimate(&self, input: &Self::In) -> CostEstimate;

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError>;
}

pub struct StageContext<'a> {
    pub providers: &'a ProviderRegistry,
    pub budget:    &'a BudgetLedger,      // reserve() returns a guard
    pub events:    &'a EventSink,
    pub cache:     &'a ResponseCache,
    pub deadline:  Instant,
    pub cancel:    CancellationToken,
    pub round:     u32,
    pub rng:       DeterministicRng,      // seeded from the manifest
}
```

**Artifacts** are content-addressed, `serde`-typed, and versioned; a stage's output is
persisted before its checkpoint event.

**The graph is a DAG per round, plus exactly one controlled loop.** There are no
arbitrary cycles. `controller.decide` returns

```rust
enum ControlFlow { Continue { round: u32, focus: Vec<ClaimId> }, Stop(StopReason) }
```

and the executor re-instantiates the round subgraph
(`challenge.plan → challenge.run → rebuttal.run → controller.decide`) with `round` in
the idempotency key, so a resumed run re-enters the right iteration.

**Concurrency** lives inside stages, not across them: the pipeline is inherently
sequential, while `positions.generate`, `claims.extract` and `challenge.run` fan out
per item under a bounded join set and a per-provider semaphore.

**Failure policy** is declared per stage: `Fatal | DegradeWithEvent | SkipItem`. A
single model timing out in `positions.generate` is `SkipItem` — the debate continues
with four positions and the record says so.

---

## 7. Build Studio provenance, made mechanical  *(review #6)*

Free-form markdown cannot be validated, so Build Studio stages do not emit markdown.
They emit assertions, and the document is *rendered from* them:

```rust
pub struct Assertion {
    pub id: AssertionId,
    pub text: String,
    pub provenance: ProvenanceKind,
    pub section: SectionPath,
}

pub fn validate_provenance(doc: &BuildDoc, cfg: &BuildConfig)
    -> Result<(), Vec<ProvenanceViolation>>;
```

Two mechanical gates, both failing the stage:

| Gate | Rule | Default |
|---|---|---|
| `Unattributed` | any assertion without a `ProvenanceKind` | zero tolerated |
| `TooMuchInvention` | `ArchitectInference` share of substantive assertions | ≤ 0.40 |

The second gate is the review's real point: "zero unattributed" is trivially satisfied
by labelling everything `ArchitectInference`. Capping that share is what keeps the
provenance meaningful, and the ratio is printed in the build report either way.

---

## 8. Adversarial fixtures  *(review #8)*

The mock provider is **scripted per call**, not canned per stage:

```jsonc
{ "call": 3, "kind": "extract", "response": "{ malformed json",  "latency_ms": 20 }
{ "call": 4, "kind": "extract", "error": "timeout" }
{ "call": 7, "kind": "judge",   "response": { "…": "missing required metric" } }
```

**The fixture list itself lives in `ARCHITECTURE.md` §18 and is authoritative there.**
It is not repeated here: the two lists had already drifted to 21 entries against 13,
which is exactly the failure this split prevents.

What belongs here is the scripting contract every fixture is built from — a mock
response is addressed by call index and stage kind, so a fixture can inject a malformed
body, a timeout, a missing rubric metric or a slow response at a precise point in the
run, and assert what the engine *did* rather than only what it decided.


---

## 9. Truncated runs are a first-class outcome  *(review #10)*

```rust
pub enum StopReason {
    Converged, RoundLimit, NoNewInformation,
    BudgetExhausted, TokenLimit, Deadline, Cancelled, ProviderFailure,
}

// The two judged-looking reasons are computed from the round's own artifacts.
// No extra call, no model opinion, both thresholds config.
fn no_new_information(r: &RoundDelta, cfg: &PolicyConfig) -> bool {
    r.new_canonical_claims < cfg.min_new_claims           // default 2
        && r.max_standing_delta < cfg.min_standing_delta  // default 0.05
}

fn converged(g: &ResolvedGraph, o: &[OptionScore], cfg: &PolicyConfig) -> bool {
    !g.has_live_attacker_against(o[0].id, cfg.thresholds.dissent)
        && (o[0].share - o[1].share) >= cfg.thresholds.min_margin * cfg.converged_margin_factor
        && g.unresolved_triggers().is_empty()
}

pub enum Completeness {
    Complete,
    Truncated { reason: StopReason, missing_stages: Vec<StageName> },
}
```

`Completeness` is a field on `DecisionRecord`, and truncation feeds the classifier
rather than bypassing it:

- the evidence-mass floor is raised by `truncation_factor` (default ×1.2), so a
  half-finished debate lands in `INSUFFICIENT_EVIDENCE` more readily
- a sixth confidence term, `truncation_penalty`, is subtracted and reported
- a truncated run may still be `MAJORITY_WITH_DISSENT` when the evidence gathered is
  genuinely strong — being cut short is not automatically being wrong

---

## 10. Plugin discovery and loading  *(review #11)*

```
./.arbiter/plugins/<name>/      project-local   (highest precedence)
~/.arbiter/plugins/<name>/      user
$ARBITER_PLUGIN_PATH            colon-separated extra roots
builtin                         compiled in     (lowest)
```

```toml
# plugin.toml
name        = "arbiter-provider-bedrock"
version     = "0.2.0"
kind        = "provider"              # provider | judge | store | exporter
abi         = "jsonrpc-1"             # jsonrpc-1 | wasm-1
entrypoint  = "./bin/bedrock"         # or ./plugin.wasm
config_schema = "./schema.json"

[permissions]
network = ["bedrock.*.amazonaws.com"]
filesystem = []
env = ["AWS_REGION", "AWS_PROFILE"]
```

`arbiter plugins list` prints name, kind, version, ABI, source root and permissions.
A plugin whose name collides with a builtin is refused unless `--allow-override` is
passed. Permissions are enforced by the host: a WASM plugin gets exactly the declared
network hosts; a subprocess plugin gets a scrubbed environment containing only the
declared variables.

---

## 11. Rounds and what "adaptive" means  *(review #9)*

Partly conceded. `max_rounds` counts deliberation rounds (challenge + rebuttal), and
the default was too tight to adapt within. Revised:

| Profile | Rounds | Notes |
|---|---|---|
| `--depth standard` | 1 | MVP default, matches the v1.0 scope |
| `--depth deep` | 3 | controller has real room to stop early |
| hard ceiling | 6 | config-capped; the controller can never exceed it |

But the naming stands, because the controller decides two things, not one:

```rust
ControlFlow::Continue { round, focus: Vec<ClaimId> }
```

**Which disputes to spend the next round on** is adaptation even when only one round
remains — a controller that must choose 6 challenge pairs out of 40 candidate disputes
is doing the work regardless of how many rounds are left.


---

## 12. Fixpoint robustness  *(review #3, second half)*

The argumentation fixpoint must be **total**: it returns a deterministic answer for
every input, including adversarial graphs.

```
support = min( Σ w·standing(s), support_cap )      support_cap = 2.0
attack  = min( Σ w·standing(a), attack_cap  )      attack_cap  = 1.5
qualify = min( Σ w·standing(q), attack_cap  )

standing_{k+1} = clamp01( (1-λ)·standing_k + λ·(E + α·support − β·attack − γ·qualify) )
λ = 0.5   α = 0.25   β = 0.60   γ = 0.15   ε = 1e-9   max_iterations = 64
```

**Saturation is a correctness fix, not tuning.** The raw sums are unbounded, so in a
dense graph ten weak attackers accumulate more defeat than one decisive refutation —
wrong dialectically and arithmetically. Capping the aggregate before weighting means a
well-evidenced fact cannot be buried under a pile of weak objections, while a single
strong refutation still defeats it. Fixture: `attack_saturation`.

- **Jacobi, not Gauss-Seidel** — every claim reads the previous iterate, so the result
  does not depend on iteration order, by construction rather than by discipline.
- **Damping plus clamping** keeps the update bounded on `[0,1]`; oscillation between
  mutually attacking claims decays instead of ringing.
- **Relation cycles are legitimate** and are *not* rejected: mutual contradiction is a
  normal dialectic shape and the damped iteration settles it.
- **Non-convergence is handled, not assumed away.** If `max_iterations` is reached with
  `Δ > ε`, the engine emits `FIXPOINT_NOT_CONVERGED { max_delta, iterations }`, keeps
  the last iterate, and applies a `convergence_penalty` to confidence. The decision is
  still produced and still deterministic — a debate does not fail because its argument
  graph is pathological, but the record says the graph was.

Fixtures pin this: `premise_cycle` and `premise_cycle_grounded_fact` (extraction-time
untangling), `fixpoint_nonconvergence` (a hand-built oscillating graph that hits the
iteration cap and still yields a byte-identical record on replay), and
`attack_saturation`.

### Parameters are versioned, not settled

`λ, α, β, γ` and both caps are tuning constants. β = 0.60 is a judgement call that dense
cyclic graphs will test, and the right response is measurement, not confidence:

```rust
pub struct PolicyVersion(pub &'static str);   // "argument-v1"
```

The active set is recorded in every `DecisionRecord` as `policy_version`. Re-tuning mints
a new version rather than silently changing what past decisions meant — **decisions are
comparable only within a policy version**, `arbiter history` groups by it, and
`arbiter replay` refuses to replay a run under a different one without `--repolicy`,
which produces a new run id rather than overwriting the original record.

A `tuning/` fixture set supports parameter sweeps under `cargo test --features tuning`.
That is a parameter study, not a unit test, and it is how β earns its default — but only
if the corpus is powered enough to distinguish candidate values:

```
≥ 20 graphs, covering every topology the fixpoint behaves differently on:
  sparse trees · dense cliques · cyclic triangles · long attack chains
  saturation edge cases (many weak attackers vs one strong)
  mutual-attack pairs · disconnected components
≥ 1 hand-verified known-outcome graph per topology
```

Under twenty graphs the sweep cannot separate β = 0.55 from β = 0.60, and pinning
`argument-v1` on it would be theatre.


---

## 13. Event taxonomy  *(review #3, #4)*

The authoritative enum. Seven families; `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]`.

```rust
pub enum EventType {
    // Lifecycle
    RunStarted, RunCompleted, RunFailed,

    // Stage
    StageStarted, StageCompleted, StageFailed, StageCheckpoint,

    // Provider — the crash-recovery protocol (§5) depends on all six
    CallStarted, CallRequestId, CallCompleted,
    CallRetrying, CallRecovered, CallOrphaned,

    // Budget — the reservation protocol
    BudgetReserved, BudgetCommitted, BudgetReleased, BudgetExhausted,

    // Debate
    PanelResolved, PositionStarted, PositionCompleted,
    ClaimExtracted, ClaimUngrounded, ClaimNormalised,
    CandidatesSelected, RelationshipFound, DisputePrioritised,
    ChallengeIssued, RebuttalReceived,
    RoundStarted, RoundCompleted, ControllerDecided,

    // Decision
    JudgeScored, DecisionSynthesized, DecisionAccepted, DecisionOverridden,

    // Integrity
    PremiseCycleDetected, FixpointNotConverged, LogRepaired,
}
```

**Forward compatibility.** Adding a variant is additive: readers skip unknown
`event_type` values but still include the line in the hash chain, so an older consumer
never breaks the integrity check on a newer log. Removing or renaming a variant requires
a `schema_version` bump.

---

## 14. The confidence formula  *(review #1, #2)*

Three evidence dimensions, five penalties. The implementation evaluates this; it does
not invent it.

```rust
pub struct ConfidenceBreakdown {
    pub evidence_mass: f64,       // mean standing of claims decisive for the winner
    pub decision_margin: f64,     // share(top1) − share(top2)
    pub judge_score: f64,         // weighted 9-metric rubric
    pub base: f64,                // 0.35·mass + 0.30·margin + 0.35·judge

    pub unresolved_penalty: f64,  // 0.25 × unresolved_critical_ratio
    pub assumption_penalty: f64,  // 0.15 × assumption_dependency_ratio
    pub truncation_penalty: f64,  // 0.10 when Completeness::Truncated
    pub convergence_penalty: f64, // 0.05 when FixpointNotConverged
    pub dispersion_penalty: f64,  // 0.20 × max(0, judge_dispersion − 0.15); 0 if judge_count == 1

    pub total: f64,               // clamp01(base − Σ penalties)
}
```

Invariants asserted in code and pinned by the `confidence_arithmetic` fixture:

```
dimension weights sum to 1.0 ±1e-9   0.35+0.30+0.35 is 0.9999999999999999 in f64;
                                     assert with an epsilon, never with ==
base ∈ [0,1]                         each dimension is clamped before weighting
total == clamp01(base − Σ penalties) recomputed, never stored independently
every field serialised                so `arbiter explain` can print the derivation
```

`judge_dispersion` is the **population** standard deviation (÷n, not ÷n−1) of the judges'
weighted scores over the same anonymised dossier. For two judges that is exactly half
their gap, which makes the penalty deliberately permissive rather than aggressive:

| scores | gap | dispersion | penalty |
|---|---|---|---|
| 0.85 / 0.75 | 0.10 | 0.050 | 0 |
| 0.80 / 0.50 | 0.30 | 0.150 | 0 — the threshold, not past it |
| 0.90 / 0.50 | 0.40 | 0.200 | 0.010 |
| 1.00 / 0.00 | 1.00 | 0.500 | **0.070 — the maximum for two judges** |

Two judges must differ by more than 0.30 on a weighted rubric score before anything is
subtracted, and total disagreement costs 0.07 of confidence. This is a nudge, not a
veto: it is a signal that the judge term is shaky, and the term itself is only 0.35 of
`base`. It measures **instability, not bias**: two judges that
independently infer the same authorship agree, dispersion is low, and nothing here sees
it. Cross-vendor selection is what addresses correlated leakage; this penalty only stops
a visibly unreliable judge signal carrying full weight.

The v2.0 worked example was internally inconsistent — 0.8695 − 0.07 − 0.04 = 0.76, not
the 0.84 printed beside it. The fixture exists so that error cannot recur.

---

## 15. Correlation groups  *(review #7)*

`independence` is computed from an explicit partition, not inferred from counts.

```rust
pub struct ClaimMember {
    pub model: ModelId,
    pub provider: ProviderId,
    pub correlation_group: GroupId,   // defaults to provider; config-overridable
    // …
}

fn independence(members: &[ClaimMember], lambda: f64) -> f64 {
    let n = members.len();
    if n == 0 { return 0.0; }
    let groups = distinct(members.iter().map(|m| &m.correlation_group));
    ((groups as f64) + lambda * ((n - groups) as f64)) / n as f64
}
```

```
{OpenAI, OpenAI, Anthropic, Google}  n=4 groups=3  →  (3 + 0.25)/4   = 0.8125
{Anthropic}                          n=1 groups=1  →  (1 + 0)/1      = 1.0
{OpenAI, OpenAI, OpenAI}             n=3 groups=1  →  (1 + 0.5)/3    = 0.50
```

The override exists because provider identity is a proxy, not the truth: two vendors
serving the same base weights are correlated and should share a group. Because that
landscape moves faster than a release cycle, the table is data, not code:

```rust
pub trait CorrelationSource: Send + Sync {          // internal plane
    fn group_for(&self, model: &ModelId, provider: &ProviderId) -> GroupId;
}
```

```
crates/arbiter-core/data/correlation.toml     shipped seed, updated in patch releases
correlation_table_path = "…"                  config override
ARBITER_CORRELATION_TABLE=/path/…             environment override
CorrelationSource plugin                      computed, for operators with better data
```

The seed is versioned with its own `table_version` and recorded in the manifest, so a run
can be explained against the grouping that was actually in force. An operator can
replace it. Absent a better table, provider-as-group **overstates** independence — the
error is in the optimistic direction and the docs say so rather than implying the
default is neutral.

---

## 16. Monotonicity, scoped  *(review #8)*

Global monotonicity over the argument graph is **false**, and the specification no
longer claims it. Counterexample: `c` supports `d`, `d` attacks `c`. Raising `E(c)`
raises `standing(d)`, which raises the attack on `c`.

What is claimed and tested:

| Property | Scope |
|---|---|
| **Local monotonicity** — raising `E(c)`, all else fixed, never lowers `standing(c)` | acyclic graphs only |
| **Attack monotonicity** — raising an attacker's evidence never raises its target's standing | acyclic graphs only |
| **Determinism** — identical inputs give identical output, any iteration order | all graphs |
| **Totality** — a result is produced within `max_iterations` for every graph | all graphs |
| **Independence** — correlated members never outscore independent ones | all graphs |

Cyclic behaviour is pinned by golden fixtures rather than asserted as a law.

---

## 17. Decision acceptance and override  *(review #14)*

Build Studio consumes an **accepted** decision. A debate concludes; a human decides
whether to act on it, and may act on a modified version.

```rust
pub struct DecisionAcceptance {
    pub accepted_by: String,
    pub accepted_at: Timestamp,
    pub overrides: Vec<DecisionOverride>,
}

pub struct DecisionOverride {
    pub id: OverrideId,
    pub path: FieldPath,     // e.g. "technical.cloud_provider"
    pub from: JsonValue,
    pub to: JsonValue,
    pub reason: String,      // required — an unexplained override is rejected
}
```

`arbiter accept <run> [--override path=value --reason "…"]` emits `DECISION_ACCEPTED`
and one `DECISION_OVERRIDDEN` per change. Build stages refuse to run without an
acceptance record, and every overridden value enters the generated spec as
`Provenance::UserOverride(override_id)` — so a reader can tell what the debate decided
from what a human changed afterwards.

---

## 18. Provenance chains  *(review #12)*

A label is not provenance. *"Redis is required because the system needs distributed
caching"* passes a label check while smuggling an unsourced premise, so inferences must
point at what they derive from and terminate at a real root.

```rust
pub struct Provenance {
    pub kind: ProvenanceKind,
    pub source_id: Option<SourceId>,
    pub source_path: Option<FieldPath>,
    pub derivation_reason: Option<String>,
    pub parent: Option<AssertionId>,      // only ArchitectInference may set this
}

pub enum ProvenanceKind {
    DebateClaim(ClaimId), DecisionField(FieldPath), UserRequirement(RequirementId),
    UserOverride(OverrideId), ExternalSource(Uri),   // roots
    ArchitectInference(String),                      // must chain to a root
}
```

```
"Use Redis"                      ArchitectInference
  └─ parent → "distributed cache required"    DecisionField(technical.caching)
       └─ parent → "30 concurrent workers"    DebateClaim(C-042)
            └─ root                           UserRequirement(req-7)
```

Gates: `Unattributed` (zero), `OrphanInference` (zero), `ChainTooDeep` (> 4),
`CitesDefeatedClaim` (zero), `TooMuchInvention` (`ArchitectInference` share of
**substantive** assertions > 0.40, config).

```rust
fn is_substantive(a: &Assertion) -> bool {
    matches!(a.section.kind(),
        SectionKind::Constraint | SectionKind::Requirement
      | SectionKind::Deliverable | SectionKind::Contract)
}
```

Both gate the two ways the ratio is gameable: **padding** the denominator with
trivially-attributed prose (only substantive assertions count) and **spurious
attribution** to a claim the debate demolished (`CitesDefeatedClaim` resolves every cited
id against the record and rejects `Defeated` standing).

`Defeated` is terminal for a *version*, and there is no `Revived` lifecycle state by
design: a claim that answers its refutation does so as `Modified{v+1}`, a new version
carrying its own standing. The gate therefore resolves ids against **final** standing at
`decision.synthesize`, so citing `C-024@v2` — which survived — is legal while citing
`C-024@v1` — which did not — is not. Chain-depth distribution is
reported alongside the ratio, since one four-hop chain is a different risk from twelve
one-hop ones.

---

## 19. Plugin trust model  *(review #13)*

| Mechanism | Isolation | Permissions |
|---|---|---|
| **WASM** (`wasm-1`) | sandboxed by the runtime | enforced — declared hosts only, no ambient filesystem |
| **JSON-RPC subprocess** (`jsonrpc-1`) | separate process, scrubbed environment | **declared and displayed, not enforced** |

A subprocess plugin is a **trusted local executable**. Scrubbing the environment removes
credentials it was not granted; it does not stop the process opening a socket or reading
a file. `arbiter plugins list` labels every plugin `SANDBOXED` or `TRUSTED`, and
installing a `TRUSTED` plugin from a non-builtin root prints that label at load time.

**Optional confinement**, because "the operator's problem" is not a good answer when the
CLI runs in CI:

```toml
[runtime]
confinement = "bwrap"        # none | bwrap | sandbox-exec | container
```

| Value | Mechanism | Applies |
|---|---|---|
| `bwrap` | `bubblewrap`: read-only rootfs, private `/tmp`, `--unshare-net` unless hosts are declared | Linux |
| `sandbox-exec` | seatbelt profile generated from the declared permissions | macOS |
| `container` | the operator's own runtime; Arbiter only passes the manifest | any |
| `none` | bare subprocess with a scrubbed environment | default for builtin |

```
ARBITER_PLUGIN_CONFINEMENT=required
```

refuses to load a `TRUSTED` plugin that cannot be confined — the recommended setting for
CI and any multi-tenant use. This is **best-effort hardening, not parity with WASM**: a
seatbelt profile or a bwrap namespace is weaker than a runtime that cannot express an
un-declared syscall in the first place, and the labels stay honest about which one you
are getting.


---

## 20. `options.cluster` — option clustering contract  *(v2.3 — the last algorithmic gap)*

`decision.synthesize` scores options. Nothing defined where options came from, so this
does.

```rust
pub trait OptionClusterer: Send + Sync {            // internal plane
    fn cluster(&self, positions: &[Position], ctx: &StageContext<'_>)
        -> Result<Vec<DecisionOption>, StageError>;

    fn attach(&self, claims: &[CanonicalClaim], options: &[DecisionOption],
              ctx: &StageContext<'_>) -> Result<AttachmentMatrix, StageError>;
}

pub struct AttachmentMatrix {                       // recorded as an artifact
    pub cells: BTreeMap<(ClaimId, OptionId), Attachment>,
}
pub struct Attachment { pub polarity: Polarity, pub confidence: f64, pub source: AttachSource }
pub enum Polarity   { Supports, Opposes, Neutral }
pub enum AttachSource { Authored, Classified, Propagated }
```

**Step 1 — cluster the recommendations.** Each position carries a structured
`recommendation`. Cluster them with the same machinery as claims (lexical + one batched
LLM grouping call), yielding 2–4 options for a typical 5-model panel. An option's id is
the **cluster's identity**, not a hash of its text — see Step 3b. Text goes into
`option_version`; the id survives rewording.

**Step 2 — attach claims.** One batched call produces the (claim × option) polarity
matrix. `|C| × |O|` pairwise calls — 32 × 3 = 96 — is exactly the cost mistake T3 was
introduced to avoid, so it is one call for the whole matrix, chunked by the same
partition rule as T3 when `|C|` is large. Claims from a position that recommended `O`
start as `AttachSource::Authored` toward `O` and may be revised by the classifier.

**Step 3 — propagate deterministically.** No LLM:

```
c contradicts s ∧ s supports O   →  c opposes O      (strength × relation confidence)
c supports    s ∧ s supports O   →  c supports O
c qualifies   s ∧ s supports O   →  c opposes O at γ weight
```

propagated to depth `attachment_propagation_depth` (default 2), tagged `Propagated`.
This is why the classifier only has to see direct attachment: the graph does the rest,
and it does it identically on replay.

**Step 3b — option lineage.** `OptionId` is the cluster's identity and does **not** move
when wording changes; `option_version` is `blake3` of the current canonical text.

```rust
pub struct DecisionOption {
    pub id: OptionId,                    // stable cluster identity
    pub version: OptionVersion,          // blake3 of canonical text
    pub supersedes: Option<(OptionId, OptionVersion)>,
    pub retired: bool,                   // superseded versions are kept, not scored
    // …
}
```

| Round-2 event | Effect |
|---|---|
| recommendation **reworded** ("modular monolith" → "…with enforced boundaries") | same `id`, new `version`; attachment cells carry over untouched |
| recommendation **materially changed** (different course of action) | new `id`, `supersedes` set, cells re-derived by the next matrix pass |
| option **abandoned** (no live claim supports it) | `retired = true`; falls below `option_floor` naturally, never deleted |

Scoring always runs over lineage heads. Hashing the text into the id — the v2.3
formulation — would have minted a new option on every refinement and orphaned its
attachment mid-debate. Fixture: `option_supersede`.

**Step 4 — round-to-round membership.** Deterministic, no re-classification:

| Event | Effect on attachment |
|---|---|
| `Modified{v}` | inherits its predecessor's cells; standing changes, membership does not |
| `Withdrawn` / `Rejected` | cells dropped |
| new claim from a rebuttal | attached in the next round's matrix pass |
| new recommendation in a rebuttal | **new option**, clustered like any other, starting with no evidence |

**What the engine never does:** invent an option nobody argued for. If no model proposed
the status quo, there is no status-quo option — the debate answers the question that was
asked, not one the engine improvised.

Fixtures: `option_clustering`, `option_emerges_midround`.

---

## 21. `disputes.rank` / `challenge.plan` — focus selection  *(v2.3)*

The controller returns `Continue { round, focus }`. The contents of `focus` decide what
the next round's money buys, so the ranking is a formula, not a heuristic.

```rust
pub fn dispute_priority(c: &CanonicalClaim, g: &ResolvedGraph, cfg: &PolicyConfig) -> f64 {
      cfg.w_contested * contested_mass(c, g)
    + cfg.w_leverage  * decision_leverage(c, g)
    + cfg.w_gap       * evidence_gap(c)
    - cfg.w_cost      * resolution_cost(c)
}
// defaults: 0.35 · 0.35 · 0.20 · 0.10
```

| Term | Definition | Why it earns its weight |
|---|---|---|
| `contested_mass` | `Σ standing(attackers) + Σ standing(defenders)` around `c`, normalised | a claim nobody contests is not a dispute |
| `decision_leverage` | flip `c`, re-run the fixpoint, take `\|Δ margin(top1, top2)\|` | **the only term that asks "could this change the answer?"** |
| `evidence_gap` | `1 − E(c)` | challenge the unevidenced, not the well-evidenced |
| `resolution_cost` | estimated tokens for the exchange ÷ remaining budget | a cheap dispute beats an equally useful expensive one |

`decision_leverage` reuses the counterfactual pass already built for change triggers:
one fixpoint per candidate claim, ~32 runs of a 64-iteration loop over ≤100 nodes —
microseconds, and entirely deterministic.

**Pair selection**, after ranking:

```
for each dispute, top-down until challenge_budget is spent:
    defender   = the claim's author
    challenger = the model whose claim most strongly contradicts it
                 (relation confidence × attacker standing)
    skip if    challenger == defender
    skip if    that model already has max_challenges_per_model this round (default 2)
```

The per-model cap exists for two reasons: it keeps fan-out parallel across providers,
and it stops one prolific model from monopolising a round's budget by having generated
the most contradictions.

Fixture: `focus_selection` — a graph where the loudest dispute has near-zero leverage
and a quiet one flips the outcome; the ranking must choose the quiet one.


---

## 22. `arbiter explain --json` schema  *(v2.4)*

Human output is a rendering of this structure, never a separate code path — otherwise the
future UI reimplements the explanation and the two drift.

```jsonc
{
  "schema_version": 1,
  "run_id": "run_01J…",
  "policy_version": "argument-v1",
  "subject": { "kind": "decision" },          // or { "kind": "claim", "id": "C-024" }

  "confidence": {
    "total": 0.84,
    "base": 0.8695,
    "dimensions": [
      { "name": "evidence_mass",   "value": 0.88, "weight": 0.35, "contribution":  0.3080,
        "derived_from": ["C-002", "C-011", "C-018"] },
      { "name": "decision_margin", "value": 0.81, "weight": 0.30, "contribution":  0.2430 },
      { "name": "judge_score",     "value": 0.91, "weight": 0.35, "contribution":  0.3185 }
    ],
    "penalties": [
      { "name": "unresolved",  "input": 0.08, "rate": 0.25, "contribution": -0.0200,
        "derived_from": ["C-031", "C-014"] },
      { "name": "assumption",  "input": 0.07, "rate": 0.15, "contribution": -0.0105 },
      { "name": "truncation",  "input": 0.0,  "rate": 0.10, "contribution":  0.0 },
      { "name": "convergence", "input": 0.0,  "rate": 0.05, "contribution":  0.0 }
    ]
  },

  "defeat_chains": [                           // why a claim stands where it does
    { "claim_id": "C-024", "standing": 0.38, "evidence": 0.50,
      "steps": [
        { "by": "C-011", "relation": "contradicts", "attacker_standing": 0.84,
          "weight": 0.60, "delta": -0.29 },
        { "by": "C-027", "relation": "qualifies",   "attacker_standing": 0.62,
          "weight": 0.15, "delta": -0.06 }
      ],
      "saturated": false }
  ],

  "change_triggers": [
    { "claim_id": "C-031", "direction": "if_true", "new_winner": "opt_microservices",
      "margin_before": 0.19, "margin_after": -0.04 }
  ],

  "options": [
    { "id": "opt_monolith", "share": 0.62,
      "supported_by": ["C-002", "C-011", "C-018"], "opposed_by": ["C-006"] }
  ]
}
```

Two properties the renderer depends on and the schema guarantees: every number carries
the inputs it was computed from (`derived_from`, `steps`, `margin_before/after`), and
`contribution` fields sum to `total` within 1e-9 — so *"why 84?"* is answered by
arithmetic present in the payload rather than by prose generated beside it.


---

## 23. Prompt packs  *(v2.5)*

Replay reproduces a decision only if the prompts are as pinned as the code. Templates are
assets, not string literals in stage code.

```
prompts/<pack>/<version>/<stage>.md + manifest.toml
```

```rust
pub struct PromptPack { pub name: String, pub version: String, pub hash: PackHash }

pub struct PromptTemplate {
    pub stage: StageName,
    pub body: String,
    pub variables: VariableSchema,      // declared, validated before render
}

// recorded on every CALL_STARTED
pub fn prompt_hash(t: &PromptTemplate, rendered: &str) -> Hash;   // blake3(rendered ‖ schema)
```

| Rule | Reason |
|---|---|
| `init` snapshots `pack_hash` into the manifest | the run states which prompts produced it |
| every `CALL_STARTED` carries `prompt_hash` | a single template change is visible per call, not just per run |
| variables validated against the declared schema before render | a missing variable is a stage error, never a silently malformed prompt |
| `prompt_hash` covers the **schema as well as** the rendered text | two prompts that render identically but declare different variables are different prompts, and must not share a cache entry |
| exact replay **refuses** a differing `pack_hash` | replaying under new prompts is a re-run wearing a replay's clothes |
| `--repack <version>` mints a new run id | the same escape hatch as `--repolicy`, and just as explicit |

Prompts, scoring constants (`policy_version`) and the correlation table
(`table_version`) are the three inputs that are neither code nor user data. All three are
versioned, recorded in the manifest, and pinned on replay for exactly the same reason:
without them, "the same inputs" is not a well-defined phrase.

Fixture: `prompt_pack_mismatch`.
