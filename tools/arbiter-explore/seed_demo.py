"""Create a demo store so `app.py` can be run before the engine exists.

    python seed_demo.py /tmp/demo-arbiter

Schema mirrors ARCHITECTURE.md §8.5 (run_catalog) and the §8.1 projection
tables. Numbers are the worked example from the spec: base 0.8695, penalties
-0.0200 and -0.0105, total 0.8390.
"""
import random
import sqlite3
import sys
from datetime import datetime, timedelta
from pathlib import Path

CATALOG = """
CREATE TABLE run_catalog (
  run_id TEXT PRIMARY KEY, status TEXT NOT NULL, question TEXT NOT NULL,
  outcome TEXT, confidence REAL, margin REAL,
  cost REAL NOT NULL DEFAULT 0, orphaned_cost REAL NOT NULL DEFAULT 0,
  duration_ms INTEGER, model_count INTEGER, depth TEXT,
  policy_version TEXT NOT NULL, started_at TEXT NOT NULL,
  completed_at TEXT, run_path TEXT NOT NULL);
CREATE INDEX ix_catalog_time ON run_catalog(started_at DESC);
CREATE INDEX ix_catalog_outcome ON run_catalog(policy_version, outcome, confidence);
"""

RUN = """
CREATE TABLE events (seq INTEGER PRIMARY KEY, event_type TEXT, stage TEXT,
  created_at TEXT, content_hash TEXT, previous_event_hash TEXT);
CREATE TABLE confidence_terms (ord INTEGER PRIMARY KEY, term TEXT, kind TEXT,
  value REAL, weight REAL, contribution REAL, derived_from TEXT);
CREATE TABLE options (option_id TEXT PRIMARY KEY, label TEXT, share REAL,
  supporting INTEGER, opposing INTEGER);
CREATE TABLE claims (claim_id TEXT PRIMARY KEY, text TEXT, standing REAL,
  state TEXT, kind TEXT, author TEXT);
"""

QUESTIONS = [
    ("Should we split billing out of the monolith?", "MAJORITY_WITH_DISSENT", 0.84, 0.31, 0.412),
    ("Postgres or DynamoDB for the ledger?", "CONSENSUS", 0.91, 0.44, 0.463),
    ("Build or buy the fraud scorer?", "SPLIT_DECISION", 0.72, 0.09, 1.088),
    ("Move the scheduler to a queue?", "MAJORITY_WITH_DISSENT", 0.88, 0.36, 0.441),
    ("Adopt a monorepo?", "CONSENSUS", 0.79, 0.28, 0.395),
    ("Rewrite the importer in Rust?", "INSUFFICIENT_EVIDENCE", 0.41, 0.04, 0.298),
    ("Self-host the vector index?", "MAJORITY_WITH_DISSENT", 0.83, 0.30, 0.474),
    ("Drop IE-era browser support?", "CONSENSUS", 0.94, 0.52, 0.361),
]

TERMS = [
    (1, "evidence_mass",   "dimension", 0.88, 0.35,  0.3080, "C-002 · C-011 · C-018"),
    (2, "decision_margin", "dimension", 0.81, 0.30,  0.2430, ""),
    (3, "judge_score",     "dimension", 0.91, 0.35,  0.3185, "9-metric rubric"),
    (4, "base",            "base",      None, None,  0.8695, ""),
    (5, "unresolved",      "penalty",   0.08, 0.25, -0.0200, "C-031 · C-014"),
    (6, "assumption",      "penalty",   0.07, 0.15, -0.0105, ""),
    (7, "truncation",      "penalty",   0.00, 0.10,  0.0,    ""),
    (8, "convergence",     "penalty",   0.00, 0.05,  0.0,    ""),
    (9, "dispersion",      "penalty",   None, 0.20,  0.0,    "inactive — judge_count == 1"),
    (10, "total",          "total",     None, None,  0.8390, ""),
]

OPTIONS = [("opt_modular", "Modular monolith", 0.58, 7, 2),
           ("opt_extract", "Extract billing only", 0.27, 4, 3),
           ("opt_micro", "Full microservices", 0.15, 2, 6)]

CLAIMS = [
    ("C-011", "Billing p99 is dominated by the payment provider, not in-process calls.", 0.84, "agreed", "Fact", "gemini"),
    ("C-002", "Deploy frequency is bounded by a 47-minute test suite, not by coupling.", 0.79, "agreed", "Fact", "claude"),
    ("C-006", "Two teams edit billing concurrently about four times a week.", 0.71, "disputed", "Fact", "llama"),
    ("C-018", "Module boundaries can be enforced in-process via build rules.", 0.66, "agreed", "Inference", "claude"),
    ("C-027", "Blast-radius gains assume independent failure domains.", 0.62, "agreed", "Inference", "gpt"),
    ("C-014", "Three services cost about $1.4k/month.", 0.44, "unresolved", "Assumption", "mistral"),
    ("C-024", "Service extraction reduces deploy blast radius.", 0.38, "disputed", "Inference", "gpt"),
    ("C-031", "No on-call rotation exists for three new services.", 0.30, "unresolved", "Inference", "llama"),
    ("C-009", "The monolith cannot scale past current traffic.", 0.09, "defeated", "Opinion", "mistral"),
]


def main() -> None:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/demo-arbiter")
    (root / "runs").mkdir(parents=True, exist_ok=True)
    hist = root / "history.db"
    hist.unlink(missing_ok=True)

    con = sqlite3.connect(hist)
    con.executescript(CATALOG)
    rng = random.Random(7)
    now = datetime(2026, 9, 1, 14, 0)

    for i, (q, outcome, conf, margin, cost) in enumerate(QUESTIONS):
        rid = f"run_01J8{chr(65 + i)}{rng.randint(100, 999)}"
        started = now - timedelta(days=len(QUESTIONS) - i, hours=rng.randint(0, 9))
        dur = rng.randint(190_000, 700_000)
        con.execute(
            "INSERT INTO run_catalog VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (rid, "completed", q, outcome, conf, margin, cost,
             0.0 if i else 0.021, dur, 5, "deep" if cost > 0.6 else "standard",
             "argument-v1", started.isoformat(timespec="seconds"),
             (started + timedelta(milliseconds=dur)).isoformat(timespec="seconds"),
             str(root / "runs" / rid)))

        rd = root / "runs" / rid
        rd.mkdir(parents=True, exist_ok=True)
        (rd / "run.db").unlink(missing_ok=True)
        r = sqlite3.connect(rd / "run.db")
        r.executescript(RUN)
        scale = conf / 0.8390
        r.executemany("INSERT INTO confidence_terms VALUES (?,?,?,?,?,?,?)",
                      [(o, t, k, v, w, round(c * scale, 4), d)
                       for o, t, k, v, w, c, d in TERMS])
        r.executemany("INSERT INTO options VALUES (?,?,?,?,?)", OPTIONS)
        r.executemany("INSERT INTO claims VALUES (?,?,?,?,?,?)", CLAIMS)
        r.executemany("INSERT INTO events VALUES (?,?,?,?,?,?)",
                      [(s, "CLAIM_EXTRACTED", "claims.extract",
                        started.isoformat(timespec="seconds"), "blake3:…", "blake3:…")
                       for s in range(1, 40)])
        r.commit(); r.close()

    con.commit(); con.close()
    print(f"demo store at {root} — {len(QUESTIONS)} runs")


if __name__ == "__main__":
    main()
