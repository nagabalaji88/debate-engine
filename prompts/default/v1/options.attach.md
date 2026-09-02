---
variables = ["claims", "options"]
---
Below are claims from a debate, and the candidate recommendations
("options") under consideration. For each claim, say how it bears on each
option: does the claim support that option being chosen, oppose it, or is
it neutral (irrelevant, or cuts both ways)?

Options:
{{options}}

Claims:
{{claims}}

Return a JSON array. Each element judges one (claim, option) pair:

`{"claim": "#2", "option": "#1", "polarity": "supports", "confidence": 0.8}`

`polarity` is exactly one of `"supports"`, `"opposes"`, `"neutral"`.
`confidence` (0 to 1) is your confidence in that judgment. You do not need
to return an entry for every (claim, option) pair — omit a pair you judge
`"neutral"` with no useful confidence to report; only return entries you
have something to say about. Return only the JSON array, nothing else.
