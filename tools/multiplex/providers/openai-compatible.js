"use strict";
const { sseLines } = require("./util");

/**
 * GPT, Grok, and DeepSeek all speak the same Chat Completions shape
 * (Grok and DeepSeek both publish themselves as OpenAI-compatible), so
 * one factory builds all three adapters instead of three near-duplicate
 * files that would drift out of sync with each other.
 */
function makeOpenAiCompatible({ id, name, desc, color, baseUrl, apiKeyEnv, modelEnv, defaultModel, pricing }) {
  return {
    id,
    name,
    desc,
    color,
    model: process.env[modelEnv] || defaultModel,
    pricing,
    hasKey() {
      return !!process.env[apiKeyEnv];
    },

    async run({ prompt, onDelta }) {
      const res = await fetch(baseUrl, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${process.env[apiKeyEnv]}`,
        },
        body: JSON.stringify({
          model: this.model,
          stream: true,
          stream_options: { include_usage: true },
          messages: [{ role: "user", content: prompt }],
        }),
      });
      if (!res.ok) {
        throw new Error(`${name} ${res.status}: ${(await res.text()).slice(0, 300)}`);
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
        const delta = chunk.choices?.[0]?.delta?.content;
        if (delta) onDelta(delta);
        if (chunk.usage) {
          inputTokens = chunk.usage.prompt_tokens || inputTokens;
          outputTokens = chunk.usage.completion_tokens || outputTokens;
        }
      }
      return { inputTokens, outputTokens };
    },
  };
}

module.exports = { makeOpenAiCompatible };
