"use strict";
const { makeOpenAiCompatible } = require("./openai-compatible");

module.exports = makeOpenAiCompatible({
  id: "grok",
  name: "Grok",
  desc: "Real-time awareness",
  color: "grok",
  baseUrl: "https://api.x.ai/v1/chat/completions",
  apiKeyEnv: "XAI_API_KEY",
  modelEnv: "XAI_MODEL",
  defaultModel: "grok-4",
  pricing: { inputPer1M: 3.0, outputPer1M: 15.0 }, // list price, edit as needed
});
