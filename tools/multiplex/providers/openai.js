"use strict";
const { makeOpenAiCompatible } = require("./openai-compatible");

module.exports = makeOpenAiCompatible({
  id: "gpt",
  name: "GPT",
  desc: "Strong general reasoning",
  color: "gpt",
  baseUrl: "https://api.openai.com/v1/chat/completions",
  apiKeyEnv: "OPENAI_API_KEY",
  modelEnv: "OPENAI_MODEL",
  defaultModel: "gpt-4o",
  pricing: { inputPer1M: 2.5, outputPer1M: 10.0 }, // list price, edit as needed
});
