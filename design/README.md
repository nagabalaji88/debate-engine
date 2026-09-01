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

## Diagrams

Three interactive maps, separate on purpose — one answers *what the system is*, one
*what the code is*, one *what happens in what order*. Mixing them is how architecture
diagrams become unreadable.

**[`system-architecture.html`](system-architecture.html)** — the whole system, four
views (keys `1`–`4`). *System context*: who touches it, what it talks to, what ships
inside it. *Runtime topology*: processes, sandboxes, egress, config precedence.
*Data at rest*: 14 artifacts — writer, format, integrity, derived-or-not, lifecycle,
size. *Data in motion*: 14 flows — transport, payload, trust, what persists, with the
two that cross the machine boundary marked.

The shape it makes visible: there is no server, no database and no account; the whole
system is one binary the operator runs, and **exactly one flow leaves the machine**.

**[`architecture-map.html`](architecture-map.html)** — 8 crates, 8 plugin planes,
2 mechanisms, 2 external resources. Click any block for its contract and its edges;
hover to trace them and dim the rest. Toggles: dependency edges, data-flow edges,
the engine boundary, and whether 1.5 items are shown. The footer states the
dependency rule the map is checked against — `core → nothing internal · kernel →
core · everything else → kernel · nothing → cli`.

**[`workflow-map.html`](workflow-map.html)** — the 15 stages, the one controlled
loop, and the gated Build Studio. Press **run** (or space) to play the pipeline:
stages light in order, the budget gauge fills, calls and tokens accumulate.
Switch **standard ↔ deep** to see the loop engage and the totals change. Overlays:
LLM-only, cost heat. Click a stage for its contract, the events it emits, and what
happens when it fails.

The playback is not decorative — it is driven by the pre-flight table in
`ARCHITECTURE.md` §11, and a standard run reproduces it exactly: **$0.480, 28 calls,
74.5k tokens**. Deep lands at $0.900 against a $1.20 target.

## The minimal UI

**[`minimal-ui.html`](minimal-ui.html)** — what `arbiter serve` renders. Four screens,
keys `1`–`4`: new run, running, result, history. Deliberately plain — system font, one
column, one accent, no dense rails. This is not concept 1–5 with the chrome removed; it
is a different brief. The five concepts are for someone who lives in the tool. This is
for someone who wants to ask a question and read an answer.

The panel picker is key-aware (screen 1) and there is a keys screen (screen 5): a model
with no working key stays **listed and disabled** rather than hidden, because an empty
panel looks like a broken install and hides why the confidence will be lower. Fewer usable
providers means fewer independence groups, and the picker says so before the run instead
of letting it surface as an unexplained number afterwards. Spec: ARCHITECTURE §11.1,
INTERFACES §25.

Three things it does that a form usually doesn't: it shows the **cost estimate before
you commit**, it says **"closing this page does not stop the run"**, and the result leads
with the one live objection rather than burying it under the winner. Spec: ARCHITECTURE
§17.1, INTERFACES §24.

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
