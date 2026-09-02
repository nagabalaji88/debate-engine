---
variables = ["claim", "challenge"]
---
You previously asserted the following claim:

CLAIM: {{claim}}

Another participant has challenged it:

CHALLENGE: {{challenge}}

Respond to the challenge. You have exactly three options:

- **Defend** — the challenge does not hold; explain why your claim stands as
  originally stated.
- **Modify** — the challenge has a point, but a revised version of your claim
  still holds; state the revision.
- **Withdraw** — the challenge is correct and your claim does not survive it.

Return JSON only, in this exact shape:

`{"outcome": "defend", "rebuttal_text": "..."}`

`outcome` is exactly one of `"defend"`, `"modify"`, or `"withdraw"`.
`rebuttal_text` is your response to the challenge, in your own words. If
`outcome` is `"modify"`, also include `"revised_text"`: the claim's new
wording. Do not include `revised_text` for `"defend"` or `"withdraw"`.

Return only the JSON object, nothing else.
