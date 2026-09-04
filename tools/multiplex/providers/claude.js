"use strict";
const { sseLines } = require("./util");

module.exports = {
  id: "claude",
  name: "Claude",
  desc: "Balanced reasoning and writing",
  color: "claude",
  model: process.env.ANTHROPIC_MODEL || "claude-sonnet-5",
  pricing: { inputPer1M: 3.0, outputPer1M: 15.0 }, // list price, edit as needed
  hasKey() {
    return !!process.env.ANTHROPIC_API_KEY;
  },

  /** onDelta(textChunk) as it streams; must resolve {inputTokens, outputTokens}. */
  async run({ prompt, onDelta }) {
    const res = await fetch("https://api.anthropic.com/v1/messages", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": process.env.ANTHROPIC_API_KEY,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: this.model,
        max_tokens: 1024,
        stream: true,
        messages: [{ role: "user", content: prompt }],
      }),
    });
    if (!res.ok) {
      throw new Error(`Anthropic ${res.status}: ${(await res.text()).slice(0, 300)}`);
    }

    let inputTokens = 0;
    let outputTokens = 0;
    for await (const payload of sseLines(res)) {
      let evt;
      try {
        evt = JSON.parse(payload);
      } catch {
        continue;
      }
      if (evt.type === "message_start") {
        inputTokens = evt.message?.usage?.input_tokens || 0;
        outputTokens = evt.message?.usage?.output_tokens || 0;
      } else if (evt.type === "content_block_delta" && evt.delta?.type === "text_delta") {
        onDelta(evt.delta.text);
      } else if (evt.type === "message_delta") {
        if (evt.usage?.output_tokens) outputTokens = evt.usage.output_tokens;
      }
    }
    return { inputTokens, outputTokens };
  },
};
