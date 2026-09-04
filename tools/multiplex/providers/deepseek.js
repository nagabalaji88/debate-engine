"use strict";
const { makeOpenAiCompatible } = require("./openai-compatible");

module.exports = makeOpenAiCompatible({
  id: "deepseek",
  name: "DeepSeek",
  desc: "Technical and coding focus",
  color: "deepseek",
  baseUrl: "https://api.deepseek.com/chat/completions",
  apiKeyEnv: "DEEPSEEK_API_KEY",
  modelEnv: "DEEPSEEK_MODEL",
  defaultModel: "deepseek-chat",
  pricing: { inputPer1M: 0.27, outputPer1M: 1.1 }, // list price, edit as needed
});
