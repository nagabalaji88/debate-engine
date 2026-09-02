---
variables = ["pairs"]
---
Below are pairs of claims from a debate. For each pair, classify how claim A
relates to claim B.

{{pairs}}

Return a JSON array. Each element judges one pair:

`{"pair": "#1", "kind": "contradicts", "from": "A", "to": "B", "confidence": 0.85}`

`kind` is exactly one of:
- `"supports"` — A provides evidence for B, or vice versa
- `"contradicts"` — A and B cannot both be true
- `"qualifies"` — A adds a condition or exception to B, weakening its
  unconditional force without flatly contradicting it
- `"unrelated"` — A and B bear on different points
- `"uncertain"` — you cannot confidently classify this pair

`from`/`to` say which claim is doing the acting: for `"contradicts"`,
`"from": "A", "to": "B"` means A contradicts B; for `"supports"`, A supports
B; for `"qualifies"`, A qualifies B. For `"unrelated"` and `"uncertain"`,
`from`/`to` do not matter — always write `"from": "A", "to": "B"` for those.
`confidence` (0 to 1) is your confidence in the classification. When you are
not sure, prefer `"uncertain"` over guessing — a wrong `"contradicts"` or
`"supports"` corrupts the record, while `"uncertain"` is recorded honestly
and carries no weight.

You do not need an entry for every pair — omit a pair you have nothing
useful to say about. Return only the JSON array, nothing else.
