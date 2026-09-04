"use strict";
require("dotenv").config();
const express = require("express");
const path = require("path");

const PROVIDERS = [
  require("./providers/claude"),
  require("./providers/openai"),
  require("./providers/gemini"),
  require("./providers/grok"),
  require("./providers/deepseek"),
];

const app = express();
app.use(express.json());
app.use(express.static(path.join(__dirname, "public")));

// The frontend's own model row/cards read from here on load, so a model
// with no key configured shows up honestly instead of silently missing.
app.get("/api/providers", (req, res) => {
  res.json({
    providers: PROVIDERS.map((p) => ({
      id: p.id,
      name: p.name,
      desc: p.desc,
      color: p.color,
      model: p.model,
      usable: p.hasKey(),
    })),
  });
});

function sseWrite(res, event, data) {
  res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

app.post("/api/run", async (req, res) => {
  const prompt = (req.body && req.body.prompt) || "";
  if (!prompt.trim()) {
    res.status(400).json({ error: "prompt is required" });
    return;
  }

  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });

  let settled = 0;
  const usable = PROVIDERS.filter((p) => p.hasKey());
  const skipped = PROVIDERS.filter((p) => !p.hasKey());

  skipped.forEach((p) => {
    sseWrite(res, "model-skipped", { model: p.id, reason: "no key configured" });
  });

  const maybeFinish = () => {
    settled += 1;
    if (settled === usable.length) {
      sseWrite(res, "run-done", {});
      res.end();
    }
  };

  if (usable.length === 0) {
    sseWrite(res, "run-done", {});
    res.end();
    return;
  }

  usable.forEach((p) => {
    const startedAt = Date.now();
    sseWrite(res, "model-start", { model: p.id });
    p.run({
      prompt,
      onDelta: (text) => sseWrite(res, "model-delta", { model: p.id, text }),
    })
      .then(({ inputTokens, outputTokens }) => {
        const elapsedMs = Date.now() - startedAt;
        sseWrite(res, "model-done", {
          model: p.id,
          inputTokens,
          outputTokens,
          totalTokens: inputTokens + outputTokens,
          costUsd: (inputTokens / 1e6) * p.pricing.inputPer1M + (outputTokens / 1e6) * p.pricing.outputPer1M,
          elapsedMs,
        });
      })
      .catch((err) => {
        sseWrite(res, "model-error", { model: p.id, error: String(err.message || err) });
      })
      .finally(maybeFinish);
  });

  req.on("close", () => {
    // Client navigated away or cancelled - the vendor calls already in
    // flight finish server-side (no vendor API offers cheap mid-stream
    // cancellation worth the complexity here); this just stops writing
    // to a closed response.
    res.write = () => {};
  });
});

const PORT = process.env.PORT || 8787;
app.listen(PORT, () => {
  console.log(`Multiplex running at http://localhost:${PORT}`);
  const missing = PROVIDERS.filter((p) => !p.hasKey()).map((p) => p.name);
  if (missing.length) {
    console.log(`No key configured for: ${missing.join(", ")} (they'll show as unavailable)`);
  }
});
