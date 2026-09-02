"""
arbiter-explore — a read-only Streamlit view over the Arbiter store.

It opens ~/.arbiter with SQLite in read-only URI mode and never writes, never
holds a key, and never spends money. Running debates stays in the CLI or in
`arbiter serve`; this is for looking at what already happened.

    streamlit run app.py
    streamlit run app.py -- --store /path/to/.arbiter
"""
from __future__ import annotations

import argparse
import sqlite3
import sys
from pathlib import Path

import altair as alt
import pandas as pd
import streamlit as st

# Validated with the dataviz palette checker against surface #FBF9F5:
# lightness band, chroma floor, CVD separation (deutan dE 10.2), normal-vision
# floor (dE 24.9) and contrast vs surface all pass.
POS = "#0F8060"
NEG = "#BF4318"
INK = "#231F1A"
MUTED = "#6B6357"
SURFACE = "#FBF9F5"


# ── store access ─────────────────────────────────────────────────────────────

def _args() -> argparse.Namespace:
    p = argparse.ArgumentParser()
    p.add_argument("--store", default=str(Path.home() / ".arbiter"))
    return p.parse_args(sys.argv[1:])


def connect_ro(path: Path) -> sqlite3.Connection:
    """Read-only, so a live run's writer is never blocked and never corrupted."""
    if not path.exists():
        raise FileNotFoundError(path)
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    return con


@st.cache_data(ttl=5)
def load_catalog(store: str) -> pd.DataFrame:
    with connect_ro(Path(store) / "history.db") as con:
        return pd.read_sql_query(
            "SELECT * FROM run_catalog ORDER BY started_at DESC", con
        )


@st.cache_data(ttl=5)
def load_run(store: str, run_id: str) -> dict:
    """Demo-store schema only, for now.

    `confidence_terms`/`options`/`claims` are this tool's own pre-implementation
    mockup of ARCHITECTURE §8.1's projection tables (see `seed_demo.py`'s
    docstring: "so app.py can be run before the engine exists"). The real
    `run.db` migration (`crates/arbiter-store/migrations/0001_initial.sql`)
    only creates `events`, `run` and `schema_metadata` so far — every other
    projection table, this trio included, is intentionally deferred until S4
    gives `run.db` a `decision` projection to read them back from
    (PLAN_DEVIATIONS.md D21). Point this at a real (non-seeded) store today and
    `page_run` will fail with "no such table: confidence_terms" — that is
    expected, not a bug in either the tool or the engine, until S4 lands.
    `page_history`/`page_trends` (`load_catalog`, above) already read the real
    `history.db` schema (S6) and work against a genuine store now.
    """
    db = Path(store) / "runs" / run_id / "run.db"
    with connect_ro(db) as con:
        conf = pd.read_sql_query(
            "SELECT term, kind, value, weight, contribution, derived_from "
            "FROM confidence_terms ORDER BY ord", con)
        opts = pd.read_sql_query(
            "SELECT option_id, label, share, supporting, opposing "
            "FROM options ORDER BY share DESC", con)
        claims = pd.read_sql_query(
            "SELECT claim_id, text, standing, state, kind, author "
            "FROM claims ORDER BY standing DESC", con)
        # events is the source of truth; every read is ordered by seq (ARCH 8.1).
        # NB: the real schema's column is `timestamp`, not `created_at` --
        # this query only works against seed_demo.py's mock schema regardless.
        events = pd.read_sql_query(
            "SELECT seq, event_type, stage, created_at FROM events ORDER BY seq", con)
    return {"confidence": conf, "options": opts, "claims": claims, "events": events}


# ── charts ───────────────────────────────────────────────────────────────────

def confidence_chart(df: pd.DataFrame) -> alt.Chart:
    """Signed contributions. Base and total are hero numbers, not bars —
    a subtotal drawn as a bar invites reading it as another contribution."""
    d = df[df["kind"].isin(["dimension", "penalty"])].copy()
    d["sign"] = d["contribution"].apply(lambda v: "adds" if v >= 0 else "subtracts")
    d["label"] = d["contribution"].apply(lambda v: f"{v:+.4f}")

    base = alt.Chart(d).encode(
        y=alt.Y("term:N", sort=None, title=None,
                axis=alt.Axis(labelColor=INK, labelFontSize=12, labelLimit=180,
                              labelPadding=8, domain=False, ticks=False)),
    )
    bars = base.mark_bar(height=13, cornerRadiusEnd=4).encode(
        x=alt.X("contribution:Q", title="contribution to confidence",
                axis=alt.Axis(gridColor="#E7E0D4", domainColor="#DBD4C7",
                              tickColor="#DBD4C7", labelColor=MUTED, titleColor=MUTED)),
        color=alt.Color("sign:N",
                        scale=alt.Scale(domain=["adds", "subtracts"], range=[POS, NEG]),
                        legend=alt.Legend(title=None, orient="top", labelColor=INK)),
        tooltip=[alt.Tooltip("term:N", title="term"),
                 alt.Tooltip("value:Q", title="value", format=".2f"),
                 alt.Tooltip("weight:Q", title="weight", format=".2f"),
                 alt.Tooltip("contribution:Q", title="contribution", format="+.4f"),
                 alt.Tooltip("derived_from:N", title="from")],
    )
    # Every bar is labelled: this is an audit view, and the numbers are the point.
    text = base.mark_text(align="left", dx=6, fontSize=11, color=MUTED).encode(
        x=alt.X("contribution:Q"), text="label:N",
    ).transform_filter(alt.datum.contribution >= 0)
    text_neg = base.mark_text(align="right", dx=-6, fontSize=11, color=MUTED).encode(
        x=alt.X("contribution:Q"), text="label:N",
    ).transform_filter(alt.datum.contribution < 0)

    # One band per term. A fixed pixel height silently drops labels once the
    # rows no longer fit; Step sizes the plot from the data instead.
    return (bars + text + text_neg).properties(
        height=alt.Step(32), padding={"right": 46},
    ).configure_view(strokeWidth=0).configure(background=SURFACE)


def trend_chart(df: pd.DataFrame, field: str, title: str, fmt: str) -> alt.Chart:
    """One measure per chart. Confidence and cost have different scales and a
    dual axis would invite a comparison that means nothing."""
    return alt.Chart(df).mark_line(
        point=alt.OverlayMarkDef(size=45, filled=True, color=POS, fill=POS),
        strokeWidth=2, color=POS
    ).encode(
        # Runs span days: without an explicit unit Altair picks hours and every
        # label renders "12 PM".
        x=alt.X("yearmonthdate(started_at):T", title=None,
                axis=alt.Axis(format="%-d %b", labelAngle=0, tickCount=6,
                              gridColor="#EDE7DC", domainColor="#DBD4C7",
                              tickColor="#DBD4C7", labelColor=MUTED)),
        y=alt.Y(f"{field}:Q", title=title,
                axis=alt.Axis(format=fmt, gridColor="#EDE7DC", domain=False,
                              ticks=False, labelColor=MUTED, titleColor=MUTED)),
        tooltip=[alt.Tooltip("question:N", title="question"),
                 alt.Tooltip("started_at:T", title="run"),
                 alt.Tooltip(f"{field}:Q", title=title, format=fmt),
                 alt.Tooltip("outcome:N", title="outcome")],
    ).properties(height=190).configure_view(strokeWidth=0).configure(background=SURFACE)


# ── pages ────────────────────────────────────────────────────────────────────

def page_history(cat: pd.DataFrame) -> None:
    st.subheader("History")
    c1, c2, c3 = st.columns([2, 1, 1])
    outcomes = sorted(cat["outcome"].dropna().unique())
    pick = c1.multiselect("Outcome", outcomes, default=outcomes)
    lo = c2.slider("Min confidence", 0.0, 1.0, 0.0, 0.05)
    pols = sorted(cat["policy_version"].unique())
    pol = c3.selectbox("Policy version", pols, index=0)

    view = cat[(cat["outcome"].isin(pick))
               & (cat["confidence"].fillna(0) >= lo)
               & (cat["policy_version"] == pol)]

    st.caption(f"{len(view)} of {len(cat)} runs · confidence is only comparable "
               f"within one policy version, so the filter is not optional")
    st.dataframe(
        view[["run_id", "question", "outcome", "confidence", "cost",
              "orphaned_cost", "model_count", "started_at"]],
        use_container_width=True, hide_index=True,
        column_config={
            "confidence": st.column_config.NumberColumn(format="%.2f"),
            "cost": st.column_config.NumberColumn(format="$%.3f"),
            "orphaned_cost": st.column_config.NumberColumn(
                "orphaned", format="$%.3f",
                help="Spend that may have been billed but could not be confirmed"),
        })


def page_run(store: str, cat: pd.DataFrame) -> None:
    ids = cat["run_id"].tolist()
    rid = st.selectbox("Run", ids, format_func=lambda r: (
        f"{r} — {cat.loc[cat.run_id == r, 'question'].iloc[0][:64]}"))
    row = cat[cat.run_id == rid].iloc[0]
    data = load_run(store, rid)

    st.subheader(row["question"])
    a, b, c, d = st.columns(4)
    a.metric("Outcome", str(row["outcome"]).replace("_", " ").title())
    b.metric("Confidence", f"{row['confidence']:.2f}")
    c.metric("Margin", f"{row['margin']:.2f}")
    d.metric("Cost", f"${row['cost']:.3f}")

    st.markdown("**Options**")
    st.dataframe(data["options"], use_container_width=True, hide_index=True,
                 column_config={"share": st.column_config.NumberColumn(format="%.2f")})

    st.markdown(f"**Why {row['confidence']:.2f}**")
    conf = data["confidence"]
    st.altair_chart(confidence_chart(conf), use_container_width=True)
    total = conf.loc[conf["kind"] == "total", "contribution"]
    basev = conf.loc[conf["kind"] == "base", "contribution"]
    if len(basev) and len(total):
        st.caption(f"base {basev.iloc[0]:.4f} minus penalties = "
                   f"**{total.iloc[0]:.4f}** — contributions sum to the total "
                   f"within 1e-9, which the engine asserts rather than the chart")
    with st.expander("The same numbers as a table"):
        st.dataframe(conf, use_container_width=True, hide_index=True)

    st.markdown("**Claims**")
    only = st.checkbox("Only disputed and unresolved", value=False)
    cl = data["claims"]
    if only:
        cl = cl[cl["state"].isin(["disputed", "unresolved"])]
    st.dataframe(cl, use_container_width=True, hide_index=True,
                 column_config={"standing": st.column_config.NumberColumn(format="%.2f")})


def page_trends(cat: pd.DataFrame) -> None:
    st.subheader("Trends")
    pols = sorted(cat["policy_version"].unique())
    pol = st.selectbox("Policy version", pols, index=0,
                       help="Runs under different policy versions are not comparable")
    view = cat[cat["policy_version"] == pol].sort_values("started_at").copy()
    view["started_at"] = pd.to_datetime(view["started_at"])

    left, right = st.columns(2)
    with left:
        st.markdown("**Confidence over time**")
        st.altair_chart(trend_chart(view, "confidence", "confidence", ".2f"),
                        use_container_width=True)
    with right:
        st.markdown("**Cost per run**")
        st.altair_chart(trend_chart(view, "cost", "cost (USD)", "$.2f"),
                        use_container_width=True)
    st.caption("Two charts rather than two axes — confidence and dollars share no "
               "scale, and putting them on one plot invites a comparison that means nothing.")

    st.markdown("**Outcome mix**")
    mix = view.groupby("outcome").size().reset_index(name="runs")
    st.dataframe(mix, use_container_width=True, hide_index=True)


# ── main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    args = _args()
    st.set_page_config(page_title="arbiter explore", layout="wide")
    st.markdown(
        f"<style>.stApp{{background:#F4F1EB}}"
        f"[data-testid='stMetricValue']{{font-size:26px;color:{INK}}}</style>",
        unsafe_allow_html=True)

    st.title("arbiter · explore")
    st.caption(f"Read-only view of `{args.store}`. Never writes, holds no key, "
               f"spends nothing. Start runs with the CLI or `arbiter serve`.")

    try:
        cat = load_catalog(args.store)
    except FileNotFoundError as exc:
        st.error(f"No store at `{exc}`. Run a debate first, or pass "
                 f"`-- --store /path/to/.arbiter`.")
        return
    if cat.empty:
        st.info("The store has no runs yet.")
        return

    page = st.sidebar.radio("View", ["History", "Run", "Trends"])
    st.sidebar.divider()
    st.sidebar.caption(f"{len(cat)} runs\n\n"
                       f"${cat['cost'].sum():.2f} total spend")
    if page == "History":
        page_history(cat)
    elif page == "Run":
        page_run(args.store, cat)
    else:
        page_trends(cat)


if __name__ == "__main__":
    main()
