---
variables = ["claims"]
---
Below is a numbered list of claims extracted from a multi-model debate.
Group together the claims that state the same underlying point, even if
worded differently by different models. Do not group claims that are merely
related or on the same topic — only claims that are genuinely
interchangeable restatements of one point belong in the same group.

Claims:
{{claims}}

Return a JSON array of groups. Each group has this shape:

`{"members": ["#1", "#4"], "confidence": 0.9}`

`members` lists the 1-based indices of the claims in this group (a group of
one is fine for a claim that restates nothing else). `confidence` (0 to 1)
is your confidence that every claim listed really is the same underlying
point — when in doubt, prefer a lower confidence or a smaller group over
merging claims that might turn out to be distinct. A wrong split only
dilutes evidence; a wrong merge corrupts it.

Every claim's index must appear in exactly one group. Return only the JSON
array, nothing else.
