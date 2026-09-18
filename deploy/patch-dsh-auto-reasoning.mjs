#!/usr/bin/env node
/**
 * Patch the installed DSH discovery/editor seam so compatible gateways can
 * publish reasoningEfforts in /models and the Models page keeps them when a
 * discovered model is adopted. The patch is idempotent and fails loudly when
 * an installed DSH version changes its generated source shape.
 */
import { execFileSync } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

const npmRoot = execFileSync("npm", ["root", "-g"], { encoding: "utf8" }).trim();
const modules = join(npmRoot, "@deepseek-ai", "dsh", "node_modules");

async function patch(path, oldText, newText) {
  const source = await readFile(path, "utf8");
  if (source.includes(newText)) return false;
  if (!source.includes(oldText)) {
    throw new Error(`unsupported DSH source shape: ${path}`);
  }
  await writeFile(path, source.replace(oldText, newText));
  return true;
}

const piDiscovery = join(modules, "@deepseek-ai", "dsh-llm-pi-ai", "lib", "index.js");
const dshLlm = join(modules, "@deepseek-ai", "dsh-llm", "lib", "index.js");
const dshLlmHost = join(modules, "@deepseek-ai", "dsh-llm", "lib", "typert.host.js");
const dshLlmRemote = join(modules, "@deepseek-ai", "dsh-llm", "lib", "typert.remote-client.js");
const apiRemotes = join(modules, "@deepseek-ai", "dsh-api-remotes", "lib", "client.js");
const modelsUi = join(modules, "@deepseek-ai", "dsh-client-ui-settings-models", "lib", "client.js");

let changed = 0;
changed += await patch(
  piDiscovery,
  `\t\tconst contextWindow = capacity(entry?.contextWindow, entry?.context_window, entry?.context_length, entry?.max_input_tokens, entry?.limit?.context);\n\t\tconst maxTokens = capacity(entry?.maxOutputTokens, entry?.max_output_tokens, entry?.maxTokens, entry?.max_tokens, entry?.limit?.output, entry?.top_provider?.max_completion_tokens);\n\t\tmodels.push({\n\t\t\tid,\n\t\t\tname,\n\t\t\t...contextWindow === void 0 ? {} : { contextWindow },\n\t\t\t...maxTokens === void 0 ? {} : { maxTokens }\n\t\t});`,
  `\t\tconst contextWindow = capacity(entry?.contextWindow, entry?.context_window, entry?.context_length, entry?.max_input_tokens, entry?.limit?.context);\n\t\tconst maxTokens = capacity(entry?.maxOutputTokens, entry?.max_output_tokens, entry?.maxTokens, entry?.max_tokens, entry?.limit?.output, entry?.top_provider?.max_completion_tokens);\n\t\tconst declaredEfforts = entry?.reasoningEfforts ?? entry?.reasoning_efforts ?? entry?.thinkingLevels;\n\t\tconst reasoningEfforts = declaredEfforts !== null && typeof declaredEfforts === "object" && !Array.isArray(declaredEfforts) ? declaredEfforts : entry?.supportsReasoning === true || entry?.reasoning === true ? { off: null, low: "low", medium: "medium", high: "high" } : void 0;\n\t\tconst compat = entry?.compat ?? (reasoningEfforts !== void 0 ? { supportsReasoningEffort: true, thinkingFormat: "openai" } : void 0);\n\t\tmodels.push({\n\t\t\tid,\n\t\t\tname,\n\t\t\t...contextWindow === void 0 ? {} : { contextWindow },\n\t\t\t...maxTokens === void 0 ? {} : { maxTokens },\n\t\t\t...reasoningEfforts === void 0 ? {} : { reasoningEfforts },\n\t\t\t...compat === void 0 ? {} : { compat }\n\t\t});`,
);

changed += await patch(
  dshLlm,
  `...model.maxTokens === void 0 ? {} : { maxTokens: model.maxTokens }`,
  `...model.maxTokens === void 0 ? {} : { maxTokens: model.maxTokens },\n\t\t\t\t\t...model.reasoningEfforts === void 0 ? {} : { reasoningEfforts: model.reasoningEfforts },\n\t\t\t\t\t...model.compat === void 0 ? {} : { compat: model.compat }`,
);

const schemaFields = `\n  'reasoningEfforts': z.record(z.string(), z.unknown()).optional(),\n  'compat': z.record(z.string(), z.unknown()).optional(),`;
for (const path of [dshLlmHost, dshLlmRemote]) {
  changed += await patch(
    path,
    `  'maxTokens': z.number().optional(),\n}))`,
    `  'maxTokens': z.number().optional(),${schemaFields}\n}))`,
  );
}

changed += await patch(
  apiRemotes,
  `\t\t\t"maxTokens": number().optional()\n\t\t}));`,
  `\t\t\t"maxTokens": number().optional(),\n\t\t\t"reasoningEfforts": record(string(), unknown()).optional(),\n\t\t\t"compat": record(string(), unknown()).optional()\n\t\t}));`,
);

changed += await patch(
  modelsUi,
  `\t\t\t\t...candidate.maxTokens === void 0 ? {} : { maxTokens: candidate.maxTokens }\n\t\t\t};`,
  `\t\t\t\t...candidate.maxTokens === void 0 ? {} : { maxTokens: candidate.maxTokens },\n\t\t\t\t...candidate.reasoningEfforts === void 0 ? {} : { reasoningEfforts: candidate.reasoningEfforts },\n\t\t\t\t...candidate.compat === void 0 ? {} : { compat: candidate.compat }\n\t\t\t};`,
);

console.log(changed ? "DSH reasoning discovery patch applied" : "DSH reasoning discovery patch already active");
