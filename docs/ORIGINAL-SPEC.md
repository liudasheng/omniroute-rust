# Implementation spec derived from the original OmniRoute (TypeScript)

> Source: `/tmp/omniroute-src` (shallow clone of v3.8.x). The gateway engine
> lives in `open-sse/`; the HTTP layer is the Next.js App Router
> (`src/app/api/v1/**`) behind the authz proxy (`src/proxy.ts`); the CLI is
> `bin/`. This spec drove the Rust rewrite; 中文版见
> [docs/zh/ORIGINAL-SPEC.md](zh/ORIGINAL-SPEC.md)。

## 1. HTTP API surface

All endpoints support CORS (`CORS_HEADERS` in `shared/utils/cors`; OPTIONS
preflight short-circuits). API keys use the CLIENT_API policy (Bearer token);
the management plane (MANAGEMENT) uses dashboard JWT/session.

| Endpoint | Method | Purpose |
|---|---|---|
| `/v1` | GET | OpenAI-compatible model list (same catalog as `/v1/models`) |
| `/v1/chat/completions` | POST | Core chat (`src/app/api/v1/chat/completions/route.ts` → `handleChat` → `open-sse/handlers/chatCore.ts`); streams SSE |
| `/v1/messages`, `/v1/messages/count_tokens` | POST | Anthropic-native format |
| `/v1/responses` | POST | OpenAI Responses format (+ `[...path]` variants) |
| `/v1/completions` | POST | Legacy completion |
| `/v1/models` | GET | Catalog: `{object:"list", data:[{id,name,provider,contextLength,maxOutputTokens,supportsReasoning,supportsVision,...}]}` |
| `/v1/combos`, `/v1/combos/test` | CRUD/POST | Combo configuration and replay tests |
| `/v1/embeddings`, `/v1/rerank`, `/v1/moderations`, `/v1/search` | POST | Routed via `EMBEDDING/RERANK/MODERATION_PROVIDERS` registries |
| `/v1/images/{generations,edits,upscale}` | POST | Images (IMAGE_PROVIDERS) |
| `/v1/audio/*`, `/v1/speech-to-text`, `/v1/text-to-speech` | POST | Audio |
| `/v1/videos`, `/v1/music`, `/v1/voices`, `/v1/ocr`, `/v1/multimodal-embeddings`, `/v1/batches`, `/v1/files` | POST/GET | Multimodal & batches |
| `/v1/accounts`, `/v1/providers`, `/v1/quotas`, `/v1/compression`, `/v1/webhooks`, `/v1/ws` | various | Management / accounts / quotas / compression / WebSocket |
| `/api/v1/...` | | Mirrors `/v1/...` |
| `/healthz` | GET/HEAD | text `ok\n|starting\n|stopping\n`, 200/503 (`src/app/healthz/route.ts`) |
| `/readyz` | same | `/healthz` alias |
| `/livez` | GET/HEAD | process liveness, always 200 `ok\n` (no DB check) |
| `/api/health` | GET | unauthenticated liveness; `/api/health/ping` = DB readiness (503 when down); `/api/health/degradation` |
| `/api/v1beta/models` | GET | v1beta alias |
| catch-all `route.ts` | | unknown `/api/*`, `/v1/*` → JSON 404 `{error:{message,type:"not_found",code:"unknown_route",path}}` (never HTML) |

Request/response shapes: inbound format is auto-detected
(`open-sse/services/provider.ts#detectFormatFromEndpoint/detectFormat`):
`/responses`→`openai-responses`, `/messages`→`claude`,
`/chat/completions`→`openai` (body-field fallback). Errors are always OpenAI
shaped: `{error:{message,type,code}}`; the status→type/code map lives in
`open-sse/config/errorConfig.ts#ERROR_TYPES` (400 invalid_request_error,
401 authentication_error, 402 payment_required, 403 insufficient_quota, 404
model_not_found, 429 rate_limit_error, 499 client_disconnected, 5xx
server_error). SSE chat emits `data: {chunk}\ndata: [DONE]` (anthropic inbound
uses anthropic event frames; third-party formats are translated by
`open-sse/translator/response/*-to-openai.ts`). Early keepalive constants:
`OPENAI_KEEPALIVE_FRAME`, `OPENAI_STARTUP_FRAME`, `OPENAI_CHAT_ERROR_FRAME`
(`open-sse/utils/earlyStreamKeepalive.ts`); `DEFAULT_SSE_HEARTBEAT_INTERVAL_MS`
= 15,000 ms (`src/shared/utils/runtimeTimeouts.ts`).

## 2. Provider system

Single source of truth `open-sse/config/providerRegistry.ts` + per-provider
`config/providers/registry/<id>/index.ts` (~240 providers). The
`RegistryEntry` schema is in `config/providers/shared.ts:133`. Key fields:

- `id / alias / format / executor / baseUrl / baseUrls (multi-URL failover) /
  authType ("apikey"|"oauth") / authHeader / models[]`
- `authHeader` variants: bearer / x-api-key / Key / x-goog-api-key
- `urlBuilder(base, model, stream)` (e.g. gemini builds
  `<base>/{model}:streamGenerateContent?alt=sse`); `urlSuffix` (anthropic =
  `?beta=true`); `modelIdPrefix` / `acceptedModelIdPrefixes` (fireworks:
  `accounts/fireworks/models/`)
- `forceStream` (kimi forces upstream SSE, gateway folds to JSON);
  `passthroughModels` (404 bans the model only, not the connection)
- `modelsUrl` (catalog sync), `testKeyModelsUrl` (key validation URL, e.g.
  openrouter `/api/v1/auth/key`)
- `timeoutMs / requestDefaults / defaultContextLength / unsupportedParams /
  defaultSupportedThinkingEfforts`
- `oauth` → `{clientIdEnv, clientIdDefault, clientSecretEnv,
  clientSecretDefault, tokenUrl, refreshUrl, authUrl}`; refresh in
  `open-sse/services/tokenRefresh.ts`

Dynamic families (`open-sse/services/provider.ts`), no registration needed:

- `openai-compatible-<name>` → baseUrl default `https://api.openai.com/v1`
  (overridable via providerSpecificData.baseUrl); format dispatched by
  apiType: `openai / openai-responses / embeddings / audio-transcriptions /
  audio-speech / images-generations`
- `anthropic-compatible-<name>` → baseUrl default `https://api.anthropic.com/v1`,
  format `claude`
- `anthropic-compatible-cc-<name>` (Claude Code compatible) additionally
  simulates Claude Code wire headers (`open-sse/services/claudeCodeCompatible.ts`;
  `CLAUDE_CODE_COMPATIBLE_DEFAULT_CHAT_PATH = "/chat_completion"` + `?beta=true`)

Key provider quick table:

| Provider | Target URL | Auth | Format |
|---|---|---|---|
| `anthropic` | `https://api.anthropic.com/v1/messages?beta=true` | `x-api-key` + `Anthropic-Version` (`open-sse/config/anthropicHeaders.ts`) | claude |
| `openai` | `https://chatgpt.com/backend-api/codex/responses` + `https://api.openai.com/v1/*` | OAuth (`chatgpt-web-codex`) | openai |
| `gemini` | `https://generativelanguage.googleapis.com/v1beta/models/<m>:streamGenerateContent?alt=sse` | `x-goog-api-key` | gemini |
| `zai` | `https://api.z.ai/api/anthropic/v1/messages?beta=true` | `x-api-key` | claude |
| `glm` | `https://api.z.ai/api/coding/paas/v4/chat/completions` | Bearer | openai (`GLM_REQUEST_DEFAULTS` in `providers/shared.ts`) |
| `kimi` | `https://api.moonshot.ai/v1/chat/completions` | Bearer | openai, `forceStream: true` |
| `openrouter` | `https://openrouter.ai/api/v1/chat/completions` | Bearer + `HTTP-Referer/X-Title` | openai, `passthroughModels: true` |
| `groq` | `https://api.groq.com/openai/v1/chat/completions` | Bearer; strict max_tokens 16384 (`PROVIDER_MAX_TOKENS.groq`) | openai |
| `ollama-cloud` | `https://ollama.com/v1/chat/completions`; `modelsUrl=https://ollama.com/api/tags` | Bearer | openai |
| local (`ollama`/`lmstudio`/`omlx`) | built-in provider-nodes or custom baseUrl | none | openai; `isLocalProvider()` recognizes localhost/127.x/private/docker hosts (`LOCAL_HOSTNAMES`) |

Executor dispatch: `open-sse/executors/index.ts#getExecutor`; specialized
executors implement real web/protocol providers (antigravity, grok-web,
kimi/web, deepseek/web, bedrock, vertex, kiro, cursor, uc, ...).

## 3. Core request flow

```
Route entry (src/app/api/v1/*/route.ts)
  → authz pipeline (src/proxy.ts) → CSRF/origin/IPFilter/bodySizeGuard
  → handler (src/sse/handlers/chat.ts: body ingest, alias resolution)
  → chatCore (open-sse/handlers/chatCore.ts, ~6,142 lines) — main orchestration
     ├ requestSetup / sanitization / injectionGuard / idempotencyCache / semanticCache
     ├ detectFormat (path → body fallback)
     ├ comboContextCache (getCombosCached)
     ├ combo.ts (open-sse/services/combo.ts: dispatchChaos / providerExecutionPipeline)
     └ streaming / non-streaming Response
```

Model-string parsing (`open-sse/services/model.ts#parseModel`):
`stripContextWindowSuffix` removes `[1m]` (marks `extendedContext:true`),
`normalizeCrossProxyModelId` normalizes cross-proxy dialects. Shapes:
1. `provider/model` (split at the first `/`, `resolveProviderAlias` via
   `PROVIDER_ID_TO_ALIAS`)
2. `alias/model` (same)
3. bare `model` → global MODEL ALIAS map (`resolveBareModelToConnectionDefault`,
   `isAlias: true`)

`getUnsupportedParams(provider, modelId)`: model-level overrides → global
precomputed map → prefix-stripped fallback → entry-level fallback.
`hasThinkingEfforts`: `getRegistryThinkingEfforts` (model-level → provider
default → heuristic).

Translation hub (`open-sse/translator/`): FORMATS enum `OPENAI /
OPENAI_RESPONSES / CLAUDE / GEMINI / CODEX / CURSOR / CLOVA / KIRO /
ANTIGRAVITY`. Hub-and-spoke: any source → openai (capability language) →
target; `translateRequest(targetFormat, sourceFormat, request, context)` /
`translateResponse(targetFormat, sourceFormat, chunk, state)`
(`optimizer/index.ts:307/803`). SSE is translated frame-by-frame, statefully;
`needsTranslation` decides passthrough. Key translators: `openai-to-claude.ts`,
`claude-to-openai.ts`, `openai-to-gemini.ts`, `openai-responses/` (both ways),
`*-to-cursor/clova/kiro/antigravity`, `openai-to-gemini-sse.ts`, response-side
`claude-to-openai.ts`.

Streaming pipeline (`open-sse/handlers/chatCore/streamingPipeline.ts`):
`pipeWithDisconnect` (client-disconnect supervision) → PII SSE transform
(`src/lib/streamingPiiTransform`) → compression echo → keepalive → SSE
transform with logger (`open-sse/utils/stream.ts`). `forceStream` providers
(kimi): the client asked for JSON but the upstream still streams; the gateway
accumulates into a complete JSON response. The reverse: `jsonBodyToSse.ts` /
`responsesJsonToSse.ts` synthesize SSE when the provider returns JSON but the
client asked to stream.

Timeouts/keepalive (`src/shared/utils/runtimeTimeouts.ts`): CONNECT 30s,
FETCH/REQUEST 600s (`DEFAULT_FETCH_TIMEOUT_MS`=600,000), FIRST_BYTE_READINESS
80s (extendable to 180s: `STREAM_READINESS_MAX_TIMEOUT_MS`), STREAM_IDLE 600s,
SSE_HEARTBEAT 15s, TLS first-byte watchdog 10s, Responses first-idle 15s,
client-disconnect grace 10s.

Error handling: `open-sse/utils/error.ts#formatProviderError` → per-status
ERROR_TYPES + DEFAULT_ERROR_MESSAGES; 401/403/429/5xx feed
`checkFallbackError` (`open-sse/services/accountFallback.ts`) which decides
connection bans / cooldown escalation / account demotion. Quota description:
`quotaPreflight.ts` (`getQuotaFetcher` per provider in
`services/*QuotaFetcher.ts`). RateLimitReason enum: auth_error /
quota_exhausted / rate_limit_exceeded / model_capacity / server_error / unknown.

## 4. Combo / routing strategies

Full list (`src/shared/constants/routingStrategies.ts#ROUTING_STRATEGY_VALUES`):
```
priority, weighted, round-robin, context-relay, fill-first, p2c, random,
least-used, cost-optimized, reset-aware, reset-window, headroom, quota-weighted,
strict-random, auto, lkgp, context-optimized, cache-optimized, fusion, pipeline
```
plus internal `quota-share`. Aliases: `usage`→least-used,
`context`→context-optimized, `weekly-reset`/`reset-window-order`→reset-window;
unknown → priority. `auto` sub-strategies (`AUTO_ROUTING_STRATEGY_VALUES`):
rules, score, cost, eco, latency, fast, sla-aware, sla, lkgp.
Account-level fallback subset (`ACCOUNT_FALLBACK_STRATEGY_VALUES`): priority /
weighted / fill-first / round-robin / p2c / random / least-used /
cost-optimized / strict-random.

Semantics (`open-sse/services/combo.ts` + `combo/*`):
- `priority`: sequential, fail down the list; `fill-first`: fill the first
  until unavailable; `round-robin`: even rotation (nativeTurnPin can pin a
  session, `combo/nativeCodexTurnPin.ts`); `p2c`: power-of-two-choices with a
  latency predictor (`comboPredicates.ts#PREDICTIVE_TTFT_MIN_SAMPLES=5`);
  `weighted`: weighted random; `least-used`: pick the lowest current usage;
  `cost-optimized`: sort by unit price; `reset-aware`/`reset-window`
  (`RESET_WINDOW_NAMES`): order by quota reset windows; `headroom`/
  `quota-weighted`: weight by remaining quota percentage (`comboPredicates.ts`,
  `comboPreflight`); `lkgp`: last-known-good provider; `cache-optimized`
  (`combo/promptCacheAffinity.ts`): prompt-cache global affinity;
  `fusion`/`pipeline` (`combo.ts#tryFusionDispatch/tryPipelineDispatch`);
  `auto` has its own `autoStrategy.ts` (task complexity classification
  code/reasoning/simple/medium, tiering via `autoCombo/scoring.ts#projectAccountTier`).

Retry/failure predicates (`open-sse/services/combo/comboPredicates.ts`):
- `MAX_COMBO_DEPTH = 3` (default; hard cap 10)
- `MAX_GLOBAL_ATTEMPTS = 30` (default; hard cap 200)
- `MAX_FALLBACK_WAIT_MS = 5000` (max wait to free one cooldown slot)
- `COMBO_LOOP_SAFETY_TIMEOUT_MS = 10*60*1000` (per whole combo)
- `COMBO_SAFETY_DRAIN_MS = 2000`
- `UNAVAILABLE_LABEL_GRACE_MS = 60*1000`
- Distinct predicates for context-overflow 400 / param-validation 400 /
  model-scoped 400 decide "jump to next provider" vs "user-fixable params" vs
  "ban this model permanently".
- Predictive TTFT fast-skip (`shouldSkipForPredictedTtft`).

Cooldowns / circuit breaking (`open-sse/config/constants.ts`):

Per-connection (account):
- `BACKOFF_CONFIG = {base:1000, max:2*60*1000, maxLevel:15}`
  (exponential, `calculateBackoffCooldown`)
- `BACKOFF_STEPS_MS = [60s, 120s, 300s, 600s, 1200s]` (per-model ban escalation)
- `COOLDOWN_MS = {unauthorized:2min, paymentRequired:2min, notFound:2min,
  notFoundLocal:5s, transientInitial:5s, transientMax:60s, transient:5s,
  requestNotAllowed:5s, rateLimit:2min, serviceUnavailable:2s, authExpired:2min}`

Circuit breaker (`PROVIDER_PROFILES`, `constants.ts:252`):

| profile | transient cooldown | rate-limit cooldown | threshold | reset | provider breaker |
|---|---|---|---|---|---|
| oauth | 5s | 60s | 8 | 60s | >10 fails in 15min → 5min cooldown |
| apikey | 3s | 0 (use retry-after) | 12 | 30s | >15 fails in 30min → 10min cooldown |
| local | 2s | 5s | 2 | 15s | >2 fails in 5min → 1min cooldown |

Adaptive backoff: `degradationThreshold` (oauth 5 / apikey 7) → DEGRADED,
`maxBackoffMultiplier` (8x / 4x), `backoffEscalationCount` (2 / 3). All
overridable via `OMNIROUTE_CIRCUIT_BREAKER_*` / `OMNIROUTE_PROVIDER_BREAKER_*`.

`DEFAULT_API_LIMITS = {requestsPerMinute:60, minTimeBetweenRequests:350,
concurrentRequests:6}` (Bottleneck-style queue).

Failure triangle (`services/combo.ts` top-level flow):
1. quotaPreflight (`evaluateQuotaCutoff`); 2. low-quota hold; 3. execution
candidates → providerExecutionPipeline; 4. on failure:
`checkFallbackError` → `recordComboFailure` → `applyNativeCodexTurnPin`
(chatPredicates) → try the next candidate until MAX_GLOBAL_ATTEMPTS /
COMBO_LOOP_SAFETY timeout → on total failure
`errorResponseWithComboDiagnostics` includes combo name/model/diagnostics.

The `quota-share` strategy books concurrency slots via
`combo/quotaShareConcurrency.ts`.

## 5. Configuration

Credential/config layers:
1. Database: `storage.sqlite` (`bin/cli/data-dir.mjs#resolveStoragePath`),
   dataDir default `~/.omniroute` (legacy); controls provider-nodes
   (`getCachedProviderConnections`), model aliases, combos. Encrypted columns
   use `STORAGE_ENCRYPTION_KEY`.
2. File: `$DATA_DIR/provider-credentials.json`
   (`open-sse/config/credentialLoader.ts` — note: only OAuth fields
   clientId/clientSecret/tokenUrl/authUrl/refreshUrl are merged; api-key
   connections live in the DB), merged over db provider-nodes.
3. Environment: api keys (`*_API_KEY`), OAuth ids/secrets (e.g.
   `GEMINI_OAUTH_CLIENT_ID`, `QODER_OAUTH_TOKEN_URL`), budgets (`OMNIROUTE_*`),
   `LOCAL_HOSTNAMES`, `.env` auto-loaded.

Default port: CLI `omniroute serve`: `--port <p>` || `process.env.PORT` ||
`20128` (`bin/cli/commands/serve.mjs:115`). Auxiliary: `API_PORT` /
`DASHBOARD_PORT` default to the main port; `8317` is only the built-in
upstream-mitm subsystem default (`src/shared/constants/providers/upstream-proxy.ts`).

Main env vars: `BASE_URL`, `API_PORT`, `DASHBOARD_PORT`, `DATA_DIR`,
`XDG_CONFIG_HOME`, `APPDATA`, `OMNIROUTE_LANG`, `OMNIROUTE_API_KEY`,
`OMNIROUTE_BASE_URL`, `OMNIROUTE_READY_TIMEOUT_MS=60000`,
`OMNIROUTE_TLS_CERT/KEY`, `OMNIROUTE_CLI_SKIP_REPO_ENV=1`, `CI`,
`OMNIROUTE_NO_UPDATE_NOTIFIER`. Timeouts: `REQUEST_TIMEOUT_MS/
FETCH_TIMEOUT_MS=600000`, `STREAM_IDLE_TIMEOUT_MS=600000`,
`STREAM_READINESS_TIMEOUT_MS=80000`, `STREAM_READINESS_MAX_TIMEOUT_MS=180000`,
`FETCH_CONNECT_TIMEOUT_MS=30000`, `FETCH_KEEPALIVE_TIMEOUT_MS=4000`,
`SSE_HEARTBEAT_INTERVAL_MS=15000`, `STREAM_DISCONNECT_GRACE_PERIOD_MS=10000`,
`TLS_FIRST_BYTE_WATCHDOG_MS=10000`, `RESPONSES_FIRST_BYTE_TIMEOUT_MS=15000`.
Circuit: `OMNIROUTE_CIRCUIT_BREAKER_OAUTH/API_KEY/LOCAL_*`,
`OMNIROUTE_PROVIDER_BREAKER_*`, `DEGRADATION_THRESHOLD`,
`MAX_BACKOFF_MULTIPLIER`, `BACKOFF_ESCALATION_COUNT`. Rate queue:
`RATE_LIMIT_MAX_WAIT_MS=30000` (default), `RATE_LIMIT_EXECUTION_MAX_WAIT_MS=600000`,
`RATE_LIMIT_MAX_QUEUE_DEPTH=0` (opt-in). Other: `CREDENTIAL_HEALTH_CHECK_INTERVAL=300000`
(min 10,000), `CREDENTIAL_HEALTH_CACHE_TTL=300000`, `STREAM_RECOVERY_ENABLED`,
`STORAGE_ENCRYPTION_KEY`, `DATA_DIR`.

Disk layout `$DATA_DIR/`: `storage.sqlite`, `provider-credentials.json`,
`.env`, `server.env` (legacy Electron), logs.

## 6. CLI (`bin/omniroute.mjs`)

Entry flow: (1) fast paths before Commander: `--version/-V`, `--mcp` (stdio),
`reset-encrypted-columns`, `reset-password`; (2) Node runtime support check;
tsx/esm + polyfill + `@/` alias resolver (`bin/aliasResolver.mjs`); (3)
Electron `server.env` → `.env` one-way migration; (4) loadEnvFile:
`$DATA_DIR/.env` → `~/.omniroute/.env` → `cwd/.env` → `<pkg>/.env`, first-wins
(#6194); (5) first run: auto-generate `STORAGE_ENCRYPTION_KEY`
(randomBytes(32).hex) when absent and no DB exists (#1622 guard); (6) `--lang`
→ i18n; (7) update-notifier (24 h); (8) `createProgram()`
(`bin/cli/program.mjs`) with global flags `--output table|json|jsonl|csv`,
`-q/--quiet`, `--no-color`, `--timeout <ms>` (default 30000), `--api-key`
(env `OMNIROUTE_API_KEY`), `--base-url` (env `OMNIROUTE_BASE_URL`),
`--context <name>` (env `OMNIROUTE_CONTEXT`), `--lang <code>`; (9) ~88
subcommands (`bin/cli/commands/registry.mjs`).

`serve` (default, `bin/cli/commands/serve.mjs`): flags `--port <p>` (fallback
`PORT`), `--no-open`, `--daemon`, `--log`, `--no-recovery`, `--max-restarts <n>`
(default 2), `--tray/--no-tray`, `--tls-cert/key`; spawns the Next.js
standalone (`dist/server.js`), pidfile `~/.omniroute/pid`, opens a browser
unless `--daemon`/`--no-open`; `waitForServer` polls `/healthz`; crash
restarts via `ServerSupervisor` (`bin/cli/runtime/processSupervisor.mjs`).
Other common commands: `status`, `stop` (pidfile), `update`, `providers`,
`combos`, `models`, `logs`, `doctor`, `backup/restore`, `setup-<client>`
(claude/codex/cursor/aider/goose, ...).

Management API auth: loopback CLI requests carry a machine-derived token
(`x-omniroute-cli-token` = HMAC-SHA256(machine-id, salt), salt
`OMNIROUTE_CLI_SALT` default `omniroute-cli-auth-v1`; `bin/cli/utils/cliToken.mjs`,
`src/lib/machineToken.ts`).

## 7. Optional features (one-liners)

- **RTK / Caveman compression** (`src/shared/validation/compressionConfigSchemas.ts`,
  `src/app/api/compression`): outbound/inbound token compression; RTK has
  minimal/standard/aggressive + raw-output retention; Caveman has
  lite/full/ultra; injected per-request via headers
  (`chatCore/headers.ts#resolveCompressionHeader`) and per-connection defaults.
- Other gateway enhancements: thinking-signature recovery
  (`thinkingSignatureRecovery.ts`), semantic/idempotency caches, prompt
  injection guard, policy/guardrails (`src/sse/`), server-owned tool-loop
  (`serverOwnedToolLoopWire.ts`), responseSanitizer, videoCombo/speechCombo/
  imageCombo, SSE streamRecovery (holdback 750 ms, 64 KiB buffer, 4 retries,
  min resume overlap 8 chars — `constants.ts#STREAM_RECOVERY`), throughput
  watchdog (30 s warmup + 30 s window, ≥4 B/s effective).

## Appendix

- `open-sse` vs `src/sse` overlap slightly: the frontend `src/sse/handlers/*`
  builds the HTTP boundary (admission/auth/requestBody/dispatch); the real
  translation and upstream execution live in `open-sse/**`. The Rust version
  merges both.
- The legacy `PROVIDERS` shape (`config/constants.ts#PROVIDERS`) is a lazy
  `Proxy` (`generateLegacyProviders` + `loadProviderCredentials`) for
  `services/provider.ts#getProviderConfig` and executor lookups.
- Logging: Pino + `forwardDashboardEventToLiveWs` routing events
  (`open-sse/utils/routing`) → dashboard WS stream.
