# Multiplex

Send one prompt to Claude, GPT, Gemini, Grok, and DeepSeek at once, and
compare the real responses side by side — cost, tokens, latency, all
measured from the actual API calls, not simulated.

## Relationship to Arbiter

None, mechanically — this is a standalone Node.js app, not a mode of
`arbiter` or `arbiter serve`, and nothing here reads or writes
`~/.arbiter`. It lives in this repo (`tools/multiplex`, alongside
`tools/arbiter-explore`) purely for convenience, so one `git clone`
gets you both. Where Arbiter runs a single question through a
multi-round adversarial debate to one shared decision, Multiplex asks
the same prompt to five independent models and shows you all five
answers side by side — a different tool for a different question
("which model handles this best?" vs. "what should we do?").

## Setup

Requires [Node.js](https://nodejs.org) 18 or later (for native `fetch`).

```bash
npm install
cp .env.example .env
```

Open `.env` and paste in whichever API keys you have. **You don't need
all five** — any model with no key configured shows up in the app
marked "no key configured" and is simply skipped; the rest still run.

| Provider | Get a key at |
|---|---|
| Claude | console.anthropic.com |
| GPT | platform.openai.com |
| Gemini | aistudio.google.com |
| Grok | console.x.ai |
| DeepSeek | platform.deepseek.com |

Then:

```bash
npm start
```

and open **http://localhost:8787**.

Or use the launcher for your OS — `./install_and_run.sh` (macOS/Linux)
or double-click `install_and_run.bat` (Windows) — which installs
dependencies and starts the server for you.

## How it works

- `server.js` is a small Express server. It never sends your API keys
  to the browser — the browser only ever talks to `localhost`, and this
  server does the actual calls to each provider.
- `providers/*.js` — one file per provider (GPT, Grok, and DeepSeek
  share `openai-compatible.js` since they all speak the same Chat
  Completions API shape). Each one streams the real response and
  reports real token usage back to the server.
- `POST /api/run` fans a prompt out to every model with a key
  configured, concurrently, and streams the results back to the
  browser over Server-Sent Events as they arrive — that's what drives
  the live Flow tab.
- Cost is computed from each provider's reported token counts times a
  list price per 1M tokens (`pricing` in each `providers/*.js` file) —
  edit those numbers if a provider changes its pricing.

## Model ids drift

Every model id (`ANTHROPIC_MODEL`, `OPENAI_MODEL`, etc. in `.env`) is
configurable because providers retire and rename models over time. If
a call 404s or comes back "model not found," check that provider's own
docs for the current id and update `.env` — nothing else needs to
change.

## What's real vs. what isn't

Everything with a number attached is real: the response text, token
counts, cost, and latency all come straight from each provider's own
API response. There is no automatic "quality" or "correctness" score —
that would need a human, or another model acting as judge, to actually
read and evaluate five answers. The Overview tab only ever shows what's
directly measurable (fastest, cheapest, most detailed by token count);
judging *which answer is actually right* is left to you, on the
Compare tab, on purpose.
