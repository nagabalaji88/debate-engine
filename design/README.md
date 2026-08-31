# Arbiter — UX concepts

Five app shells for the engine in `ARCHITECTURE.md` v2.6. One file:
[`concepts.html`](concepts.html) — press `1`–`5` or `←`/`→` to switch.
Nothing here is implemented; these are pickable directions.

## What the research changed

**1. The output is a graph, not a document.** A decision here is a directed
argument graph with numbers attached — `standing`, `share`, defeat chains,
an attachment matrix. Every prose rendering of it destroys the structure the
engine spent $0.41 building. So none of the five concepts has a reading
column; four of them put the graph, the matrix or the arithmetic on screen as
the primary object.

**2. There are three jobs, not one, and they want different screens.**

| Job | When | Wants | Concept |
|---|---|---|---|
| Watch it work | during the 3–8 min run | stage progress, spend, a kill switch | **A** |
| Interrogate the answer | after | structure, provenance, counterfactuals | **B**, **C** |
| Carry it forward | later | accept / override / build / compare runs | **C**, **E** |
| Audit the process | when the answer is surprising | rounds, challenges, judge behaviour | **D** |

Trying to serve all four in one screen is what produced the earlier
essay-shaped layouts. Each concept below picks one and commits.

**3. Trust is calibrated by arithmetic, not by adjectives.** `0.84` means
nothing on its own. `explain --json` guarantees every number carries the
inputs it came from (`derived_from`, `steps`, `margin_before/after`) and that
contributions sum to the total within 1e-9. So the signature interaction is
**click a number, see its inputs** — concept C is built entirely around it.

**4. A run costs money, so the budget is UI, not telemetry.** Spend, the 5%
reserved headroom, live reservations and the per-stage breakdown are
first-class in A, not buried in a settings tab.

**5. It is a CLI product and the UI must not hide that.** Every pane in
every concept shows the command that produced it. Concept E goes further and
makes the command line the interface.

**6. Model votes are never shown as a score.** 4-vs-1 does not decide
anything in this engine, and a UI that renders vote counts would teach users
the opposite of how it works. Author badges appear only as provenance.

## The five

| # | Concept | Layout | Foregrounds | Pick it if |
|---|---|---|---|---|
| A | **Mission Control** | 3 columns: stage rail · claim feed + event log · budget ledger | a run in flight | the scary part is spending money on a black box |
| B | **Argument Map** | filter rail · graph canvas · inspector | the claim graph | you want to see *why*, structurally |
| C | **Confidence Ledger** | answer bar · waterfall · options + attachment matrix · change triggers | the arithmetic behind 0.84 | the product is auditability |
| D | **Debate Floor** | transport · round bands × 5 model lanes · judge panel | process and replay | you need to defend the process to someone |
| E | **Command Deck** | command palette · result blocks · pinned context | CLI parity, comparing runs | your users live in a terminal |

Not mutually exclusive. The most likely real product is **A during a run →
C when it lands**, with **B** behind a key from any claim id and **E** as the
power surface. D is the one that could ship later.

## Design system

Swiss/minimal, dashboard density (per `ui-ux-pro-max`: *Minimalism & Swiss
Style* — enterprise apps, dashboards, professional tools).

- **Warm stone, no `#FFFFFF` anywhere.** `#F2EEE7` ground, `#FAF7F2` panels,
  `#EAE4DA` sunk. Light without being a blank page.
- **Semantic colour only.** teal `#1C5A4E` = system/primary, green `#2A7259`
  = supports, terracotta `#AB412B` = contradicts, amber `#9A6A1C` =
  qualifies/unresolved, slate blue `#3F6486` = judge. Never decoration.
- **JetBrains Mono for every number and id**, IBM Plex Sans for chrome.
  Tabular figures throughout so columns of standings align.
- **Viewport-locked.** The page never scrolls; panes scroll independently.
  Verified at 1600×960: `scrollWidth == clientWidth`, `body.scrollHeight == 960`.
- **Density 9/10** — 13px base, 11px meta, 10px uppercase labels, 5px radii.
- Minimum contrast 4.5:1 on text; focus rings kept; colour never the only
  channel (every state also carries a text chip).

## Known gaps

Static mockups: the tab switcher, node selection and the flip toggles are
live, nothing else is. No responsive work below ~1280px — these are desktop
shells and the small-screen story (probably: A collapses to the ledger, C to
the waterfall) is unresolved. Motion is deliberately near-zero; the only
animation is the in-flight spinner and the cursor blink.
