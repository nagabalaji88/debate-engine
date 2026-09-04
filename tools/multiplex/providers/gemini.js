"use strict";
const { sseLines } = require("./util");

module.exports = {
  id: "gemini",
  name: "Gemini",
  desc: "Multimodal strength",
  color: "gemini",
  model: process.env.GEMINI_MODEL || "gemini-2.0-flash",
  pricing: { inputPer1M: 0.1, outputPer1M: 0.4 }, // list price, edit as needed
  hasKey() {
    return !!process.env.GEMINI_API_KEY;
  },

  async run({ prompt, onDelta }) {
    const url =
      `https://generativelanguage.googleapis.com/v1beta/models/${this.model}:streamGenerateContent` +
      `?alt=sse&key=${encodeURIComponent(process.env.GEMINI_API_KEY)}`;
    const res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ contents: [{ parts: [{ text: prompt }] }] }),
    });
    if (!res.ok) {
      throw new Error(`Gemini ${res.status}: ${(await res.text()).slice(0, 300)}`);
    }

    let inputTokens = 0;
    let outputTokens = 0;
    for await (const payload of sseLines(res)) {
      let chunk;
      try {
        chunk = JSON.parse(payload);
      } catch {
        continue;
      }
      const parts = chunk.candidates?.[0]?.content?.parts;
      if (parts) {
        for (const p of parts) if (p.text) onDelta(p.text);
      }
      if (chunk.usageMetadata) {
        inputTokens = chunk.usageMetadata.promptTokenCount || inputTokens;
        outputTokens = chunk.usageMetadata.candidatesTokenCount || outputTokens;
      }
    }
    return { inputTokens, outputTokens };
  },
};
