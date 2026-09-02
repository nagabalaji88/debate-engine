---
variables = ["position_text", "failed_claims"]
---
The claims below could not be automatically grounded in the position text.
For each one, find the exact substring of the position text that supports it,
or mark it as an inference and name the premises that support it (which may
be other claims listed below, by their index).

Position:
{{position_text}}

Claims needing repair:
{{failed_claims}}

Return a JSON array, one element per claim above, in the same order, each
shaped exactly like this:

- If you can find a supporting quote:
  `{"index": "#3", "kind": "fact", "grounding": {"quote": "<exact substring of the position text>"}}`

- If it is genuinely an inference:
  `{"index": "#3", "kind": "inference", "grounding": {"derived_from": ["#1"], "confidence": 0.8}}`

If a note below says two or more claims cite each other as premises (a
cycle), break it: name which one is the real base premise (supported
directly by the text, or by some other claim not in the cycle), and mark
the rest as depending on it rather than on each other. If none of them can
be shown to be a genuine base premise, mark them independent (no
`derived_from`) instead of leaving the cycle in place.

`quote` must be copied verbatim from the position text. Return only the
JSON array, nothing else.
