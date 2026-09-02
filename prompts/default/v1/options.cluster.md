---
variables = ["positions"]
---
Below are the full positions taken by each panelist in a debate. Identify
each position's core recommendation — the course of action it argues for —
and group the positions whose recommendations are genuinely the same course
of action, even if worded differently.

Positions:
{{positions}}

Return a JSON array of groups. Each group has this shape:

`{"members": ["#1", "#3"], "label": "Adopt a modular monolith architecture", "confidence": 0.9}`

`members` lists the 1-based indices of the positions in this group (a group
of one is fine — not every position need share a recommendation with
another). `label` is a concise, single-sentence statement of the shared
recommendation, written as an actionable course of action, not a summary of
the debate. `confidence` (0 to 1) is your confidence that every position
listed truly argues for that same course of action — prefer a smaller group
or lower confidence over merging positions that argue for different things.

Every position's index must appear in exactly one group. Do not invent a
recommendation no position actually argued for. Return only the JSON array,
nothing else.
