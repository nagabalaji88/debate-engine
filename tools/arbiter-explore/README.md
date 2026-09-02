# arbiter-explore

A read-only Streamlit view over the Arbiter store. It opens `~/.arbiter` with
SQLite in **read-only URI mode**, so it never blocks a live run's writer and
cannot corrupt a store. It holds no API key and spends nothing.

Starting debates stays in the CLI or in `arbiter serve`. This is for looking at
what already happened — which is the job Streamlit is actually good at.

```bash
uv venv && uv pip install streamlit pandas altair
streamlit run app.py                              # reads ~/.arbiter
streamlit run app.py -- --store /path/to/.arbiter
```

No engine yet? Seed a demo store:

```bash
python seed_demo.py /tmp/demo-arbiter
streamlit run app.py -- --store /tmp/demo-arbiter
```

## Current schema status — read before pointing this at a real store

This tool was built during design work, before `arbiter-store`'s real schema
existed, so it currently has **two different relationships to the real store**
depending on the page:

- **History and Trends** (`history.db`'s `run_catalog`) read the schema the
  engine actually implements today (`arbiter-store` task S6) and work against
  a genuine store.
- **Run detail** (`run.db`'s `confidence_terms`/`options`/`claims`/`events`)
  is still `seed_demo.py`'s own pre-implementation mockup of ARCHITECTURE
  §8.1's projection tables. The real `run.db` migration only creates `events`,
  `run` and `schema_metadata` so far — everything else, including these three
  tables, is intentionally deferred to task S4 (`PLAN_DEVIATIONS.md` D21).
  Selecting a run from a genuine (non-seeded) store will error with "no such
  table" until S4 lands; that is expected, not a bug. Use `seed_demo.py`'s
  output to exercise this page until then.

## Why this is read-only, and stays that way

`POST /api/runs` in `arbiter serve` spends real money, and ARCHITECTURE §17.1
puts five security requirements around that: loopback bind, a per-process token,
`Host` validation, `Origin` checks, no CORS. Streamlit gives you none of them,
has no auth in the open-source build, and its own websocket to worry about.

Re-implementing that surface in Python to launch runs would be a second place
where credentials and spend authority live. So this app has neither. The split
is the point:

| | `arbiter serve` | `arbiter-explore` |
|---|---|---|
| Starts runs | yes | no |
| Holds a key | yes | no |
| Can spend | yes | no |
| Reads the store | via the engine | directly, read-only |
| Ships | inside the binary | separate, optional, Python |

It needs no API at all — it reads `history.db` and `run.db` with `sqlite3`. That
is only possible because of the v2.7 SQLite migration; against the old NDJSON
store this app would have had to reimplement the reader, the hash chain and the
projection logic in Python, and would have drifted from the engine the first
time either changed.

## Charts

Two rules that shaped them, both from the dataviz guidance:

- **No dual axis.** Confidence (0–1) and cost (dollars) get two charts. One plot
  with two scales invites a comparison that means nothing.
- **The palette is validated, not eyeballed.** `#0F8060` / `#BF4318` passes the
  lightness band, chroma floor, CVD separation (deutan ΔE 10.2), normal-vision
  floor (ΔE 24.9) and contrast checks against the `#FBF9F5` surface.

The confidence view draws the eight signed contributions as bars and leaves
`base` and `total` as numbers — a subtotal drawn as a bar reads as another
contribution. Every bar is labelled because this is an audit view where the
numbers are the whole point.

## Limits

Projection tables (`confidence_terms`, `options`, `claims`) are derived from
`events` per ARCHITECTURE §8.1. This app reads the projections, so a store whose
projections are stale relative to its log will show stale numbers. `arbiter
replay` rebuilds them; the `projection_rebuild` fixture is what guarantees they
match.
