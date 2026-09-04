"use strict";

/**
 * Reads a fetch() Response body as Server-Sent Events and yields each
 * event's raw `data:` payload (as a string), in order. Every provider
 * Multiplex talks to (Anthropic, the three OpenAI-compatible ones, and
 * Gemini with alt=sse) frames its stream this way, so one parser covers
 * all five instead of five hand-rolled copies.
 */
async function* sseLines(response) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let idx;
    while ((idx = buffer.indexOf("\n")) !== -1) {
      const line = buffer.slice(0, idx).trim();
      buffer = buffer.slice(idx + 1);
      if (line.startsWith("data:")) {
        const payload = line.slice(5).trim();
        if (payload && payload !== "[DONE]") yield payload;
      }
    }
  }
}

/** input/output prices are USD per 1M tokens - published list prices,
 * approximate and meant to be edited as providers change them. */
function costUsd(pricing, inputTokens, outputTokens) {
  return (inputTokens / 1e6) * pricing.inputPer1M + (outputTokens / 1e6) * pricing.outputPer1M;
}

module.exports = { sseLines, costUsd };
