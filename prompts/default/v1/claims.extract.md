---
variables = ["position_text"]
---
Extract the individual factual and inferential claims from the position below.
For each claim, say exactly how it is grounded in the text.

Position:
{{position_text}}

Return a JSON array. Each element has exactly this shape:

- A directly stated fact:
  `{"text": "<claim in your own words>", "kind": "fact", "grounding": {"quote": "<exact substring of the position text that supports it>"}}`

- An inference the position draws from other claims in this same array:
  `{"text": "<claim in your own words>", "kind": "inference", "grounding": {"derived_from": ["#1", "#4"], "confidence": 0.8}}`

  `derived_from` refers to other claims by their 1-based position in this
  same array (`"#1"` is the first claim you return, `"#4"` the fourth).
  `confidence` is your own confidence (0 to 1) that this inference is
  actually supported by those premises.

Rules:
- `quote` must be copied verbatim from the position text above — do not
  paraphrase it.
- Every claim you return must be one atomic, checkable statement — split
  compound sentences into separate claims rather than combining them.
- Do not invent premises: `derived_from` may only reference claims that
  actually appear elsewhere in the array you return.
- Return only the JSON array, nothing else.
