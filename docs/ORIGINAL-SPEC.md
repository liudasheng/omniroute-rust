# OmniRoute (TypeScript) → Rust Core Gateway 实现规格

来源：`/tmp/omniroute-src`。核心引擎在 `open-sse/`，HTTP 层为 Next.js App Router 路由 `src/app/api/v1/**`，鉴权网关 `src/proxy.ts`（matcher 覆盖 `/v1`(大小写不敏感)、`/v1beta`、`/chat`、`/responses`、`/codex`、`/models`、`/api/*`）→ `src/server/authz/pipeline.ts`（routeClass: PUBLIC / CLIENT_API / MANAGEMENT 三种策略；浏览器写请求加 CSRF 校验）。

---

## 1. HTTP API 端点清单

所有端点支持 CORS（`shared/utils/cors` 的 CORS_HEADERS，OPTIONS 预检直达返回）。API key 走 CLIENT_API 策略（Bearer token，管理面 MANAGEMENT 用 dashboard JWT/session）。

| 端点 | 方法 | 用途 |
|---|---|---|
| `/v1` | GET | OpenAI 兼容模型列表（与 `/v1/models` 同一 catalog 构建） |
| `/v1/chat/completions` | POST | 核心 chat（`src/app/api/v1/chat/completions/route.ts` → `handleChat` → `open-sse/handlers/chatCore.ts`）。流式返回 SSE |
| `/v1/messages`, `/v1/messages/count_tokens` | POST | Anthropic 原生格式（`src/app/api/v1/messages/route.ts`） |
| `/v1/responses` (`src/app/api/v1/responses/route.ts`) | POST | OpenAI Responses 格式 + `[...path]` 变体 |
| `/v1/completions` | POST | legacy completion |
| `/v1/models` (`route.ts` + `catalog*.ts`) | GET | 模型目录：`{object:"list", data:[{id,name,provider,contextLength,maxOutputTokens,supportsReasoning,supportsVision,...}]}` |
| `/v1/combos`, `/v1/combos/test` | CRUD/POST | combo 配置与回放测试 |
| `/v1/embeddings`, `/v1/rerank`, `/v1/moderations`, `/v1/search` | POST | 各按 registry 中的 `EMBEDDING/RERANK/MODERATION_PROVIDERS`（`open-sse/config/*Registry.ts`） |
| `/v1/images/{generations,edits,upscale}` | POST | 图像（IMAGE_PROVIDERS） |
| `/v1/audio/*`（transcriptions/speech）、`/v1/speech-to-text`、`/v1/text-to-speech` | POST | 音频 |
| `/v1/videos`, `/v1/music`, `/v1/voices`, `/v1/ocr`, `/v1/multimodal-embeddings`, `/v1/batches`, `/v1/files` | POST/GET | 多模态与批处理 |
| `/v1/accounts`, `/v1/providers`, `/v1/quotas`, `/v1/compression`, `/v1/webhooks`, `/v1/ws` | 各 | 管理/账户/配额/压缩/WebSocket |
| `/api/v1/...` | | 与 `/v1/...` 同构镜像（`src/app/api/v1/**`） |
| `/healthz` | GET/HEAD | 防 C：‑/api/health/ping（json）；文本 `ok\n|starting\n|stopping\n`，200/503（`src/app/healthz/route.ts`） |
| `/readyz` | 同 | `/healthz` 别名 |
| `/livez` | GET/HEAD | 进程存活探针，恒 200 `ok\n`（不查 DB） |
| `/api/health` | GET | 无鉴权 liveness；`/api/health/ping` = DB 就绪（503 when down）；`/api/health/degradation` |
| `/api/v1beta/models` | GET | v1beta 模型别名 |
| catch-all `src/app/api/[...omnirouteApiCatchAll]/route.ts` | | 未知 /api/*、/v1/* → JSON 404 `{error:{message,type:"not_found",code:"unknown_route",path}}`（绝不返回 HTML） |

### 请求/响应 JSON 形状

- 入站请求三形态自动识别（`open-sse/services/provider.ts#detectFormatFromEndpoint/detectFormat`）：按路径 `/responses`→`openai-responses`，`/messages`→`claude`，`/chat/completions`→`openai`（再按 body 字段兜底 `max_tokens` vs `max_tokens`/`messages[0].content`）。
- 错误统一 OpenAI 形状：`{error:{message, type, code}}`；`ERROR_TYPES`（`open-sse/config/errorConfig.ts`）映射 status→type/code：400 invalid_request_error/bad_request、401 authentication_error/invalid_api_key、402 payment_required、403 insufficient_quota、404 model_not_found、429 rate_limit_error、499 client_disconnected、5xx server_error（502/503/504）。
- SSE 流式响应：Chat 走 `data: {chunk}\ndata: [DONE]`；Anthropic 入站则用 anthropic 事件帧；第三方格式（gemini/cursor/clova/kiro）由 `open-sse/translator/response/*-to-openai.ts` 转为 openai chunk。提前 keepalive：`OPENAI_KEEPALIVE_FRAME`、`OPENAI_STARTUP_FRAME`、`OPENAI_CHAT_ERROR_FRAME`（`open-sse/utils/earlyStreamKeepalive.ts`），DEFAULT_SSE_HEARTBEAT_INTERVAL_MS = 15000ms（`src/shared/utils/runtimeTimeouts.ts`）。

---

## 2. Provider 体系

### Registry（单一事实源）

`open-sse/config/providerRegistry.ts` ≈ 240 个 provider 目录，全部在 `open-sse/config/providers/registry/<id>/index.ts`，每个导出 `RegistryEntry`（schema 在 `open-sse/config/providers/shared.ts:133`）。

关键字段：
- `id / alias / format / executor / baseUrl / baseUrls(多URL容错) / authType("apikey"|"oauth") / authHeader / models[]`
- `urlBuilder(base,model,stream)`（如 gemini 拼到 `<base>/{model}:streamGenerateContent?alt=sse`）
- `urlSuffix`（anthropic = `?beta=true`）
- `modelIdPrefix / acceptedModelIdPrefixes`（fireworks: `accounts/fireworks/models/`）
- `modelsUrl`（目录同步）、`testKeyModelsUrl`（密钥校验专用 URL，如 openrouter `/api/v1/auth/key`）
- `forceStream`（kimi 等强制上游 SSE，网关转回 JSON）、`passthroughModels`（404 仅模型级，不冷却连接）
- `timeoutMs / requestDefaults / defaultContextLength / unsupportedParams / defaultSupportedThinkingEfforts`
- `oauth` → `{clientIdEnv,clientIdDefault,clientSecretEnv,clientSecretDefault,tokenUrl,refreshUrl,authUrl}`；token 刷新在 `open-sse/services/tokenRefresh.ts`

### 兼容族前缀（动态 provider，无需注册）

`open-sse/services/provider.ts`：
- `openai-compatible-<name>` → baseUrl 默认 `https://api.openai.com/v1`（可由 providerSpecificData.baseUrl 覆盖），format 按 apiType 分 `openai / openai-responses / embeddings / audio-transcriptions / audio-speech / images-generations`
- `anthropic-compatible-<name>` → baseUrl 默认 `https://api.anthropic.com/v1`，format=`claude`
- `anthropic-compatible-cc-<name>`（Claude Code 兼容）→ 附加模拟 Claude Code 的 wire headers（`open-sse/services/claudeCodeCompatible.ts`，HINTs: `CLAUDE_CODE_COMPATIBLE_DEFAULT_CHAT_PATH = "/chat_completion"` + `?beta=true`）

### 重点 provider 速查表

| Provider id | 目标 URL | 认证头 | 格式 |
|---|---|---|---|
| `anthropic` | `https://api.anthropic.com/v1/messages?beta=true` | `x-api-key` + `Anthropic-Version`（header 生成在 `open-sse/config/anthropicHeaders.ts` / `providers/shared.ts#ANTHROPIC_BETA_API_KEY`） | claude |
| `openai` | `https://chatgpt.com/backend-api/codex/responses` + `https://api.openai.com/v1/*`（多 URL） | `(Bearer)` OAuth（`chatgpt-web-codex`） | openai |
| `gemini` | `https://generativelanguage.googleapis.com/v1beta/models/<m>:streamGenerateContent?alt=sse` | `x-goog-api-key` | gemini |
| `openai`（兼容 openai-compatible-xxx） | base 可配置 | `Bearer` | openai |
| `zai` | `https://api.z.ai/api/anthropic/v1/messages?beta=true` | `x-api-key` | claude |
| `glm` | `https://api.z.ai/api/coding/paas/v4/chat/completions` | Bearer | openai（自定义 executor，详见 `open-sse/config/providers/shared.ts#GLM_REQUEST_DEFAULTS`） |
| `kimi` | `https://api.moonshot.ai/v1/chat/completions` | Bearer | openai，`forceStream:true` |
| `openrouter` | `https://openrouter.ai/api/v1/chat/completions` | Bearer + `HTTP-Referer/X-Title` | openai；`passthroughModels:true`（404 只封模型） |
| `groq` | `https://api.groq.com/openai/v1/chat/completions` | Bearer；strict max_tokens 16384（`config/constants.ts#PROVIDER_MAX_TOKENS.groq`） | openai |
| `ollama-cloud` | `https://ollama.com/v1/chat/completions`；`modelsUrl=https://ollama.com/api/tags` | Bearer | openai |
| 本地主机（`ollama`/`lmstudio`/`omlx`） | src/lib 内置 provider-nodes 或自定义 baseUrl | 无 | openai；`isLocalProvider()`（`providerRegistry.ts`）识别 localhost/127.0.0.1/私网/Docker 主机 LOCAL_HOSTNAMES |
| anthropic-compatible-* | 覆盖 base | x-api-key | claude |

executor 分派：`open-sse/executors/index.ts#getExecutor`；specialized executors 用于真实 web/协议级 provider（`antigravity`, `grok-web`, `kimi/web`, `deepseek/web`, `bedrock`, `vertex`, `kiro`, `cursor`, `uc` 等，文件在 `open-sse/executors/*.ts`）。

---

## 3. 请求处理核心流程（open-sse 侧）

```
Route 入口 (src/app/api/v1/*/route.ts)
  → authz pipeline (src/proxy.ts) → CSRF/origin/IPFilter/bodySizeGuard
  → 各具体 handler (src/sse/handlers/chat.ts 摄入 body、alias 解析)
  → chatCore (open-sse/handlers/chatCore.ts, 约6142行) ── 主编排
     ├ requestSetup / sanitization / injectionGuard / idempotencyCache / semanticCache
     ├ detectFormat (path → body fallback)
     ├ comboContextCache (getCombosCached)
     ├ combo.ts (open-sse/services/combo.ts, dispatchChaos / providerExecutionPipeline)
     └ 返回流式/非流式 Response
```

**模型字符串解析**（`open-sse/services/model.ts#parseModel`）：
- 输入模型串 → stripContextWindowSuffix 移除 `[1m]` 等后缀（`extendedContext:true` 标记 1M 上下文），`normalizeCrossProxyModelId` 归一跨代理方言。
- 三种形态：
  1. `provider/model`（首个 `/` 分割）→ resolveProviderAlias(tokens[0]) 用 `PROVIDER_ID_TO_ALIAS` 表把别名→canonical id，剩下部分代表 provider 侧模型。
  2. `alias/model`（alias 也是 provider 别名）→ 同上，isAlias 处理见 `providerRegistry.alias`。
  3. 裸模型 `model`（不含 `/`）→ 从全局 MODEL ALIAS 表 (`resolveModelAliasFromMap`) → подсист模型→provider 映射（`resolveBareModelToConnectionDefault`）。`isAlias:true`。
- `getUnsupportedParams(provider, modelId)`：模型级 overrides → 全局 _unsupportedParamsMap（预计算 map）→ prefix-stripped fallback → entry 级 fallback。
- `hasThinkingEfforts`：`getRegistryThinkingEfforts(provider, modelId)`（模型级 → provider 默认 → openvocabulary heuristic）。

**翻译层**（`open-sse/translator/`）：
- `FORMATS` 枚举：`OPENAI / OPENAI_RESPONSES / CLAUDE / GEMINI / CODEX / CURSOR / CLOVA / KIRO / ANTIGRAVITY`。
- hub-and-spoke：任意 source→先翻到 openai(capability-语言)→再翻到 target；`translateRequest(targetFormat, sourceFormat, request, context)` / `translateResponse(targetFormat, sourceFormat, chunk, state)`（`optimizer/index.ts:307/803`）。SSE 帧级（stateful）转换：流中 chunk 逐个翻译（`needsTranslation` 决定 passthrough）。
- 关键翻译器：`openai-to-claude.ts`（将 openai 消息内容拆分回 system/messages，behavior `claudeSystemRole` 处理 system 拆分）、`claude-to-openai.ts`、`openai-to-gemini.ts`、`openai-responses/`（双向）、`*-to-cursor/clova/kiro/antigravity`、`openai-to-gemini-sse.ts`（gemini SSE 输出转换）、response 侧 `claude-to-openai.ts`。

**流式管道**（`open-sse/handlers/chatCore/streamingPipeline.ts`）：
`pipeWithDisconnect`（客户端断开监督）→ PII SSE transform（`src/lib/streamingPiiTransform`）→ compression echo → keepalive → `SSE transform with logger`（`open-sse/utils/stream.ts`）。`forceStream` provider（kimi 等）：客户端请求非流式时上游仍发 SSE，网关积累成完整 JSON 形状响应。
反方向 `jsonBodyToSse.ts` / `responsesJsonToSse.ts`：客户端接受流而 provider 只给 JSON 时合成 SSE。

**超时/保持**：
- CONNECT_TIMEOUT = 30s、FETCH_TIMEOUT/REQUEST_TIMEOUT = 600s（`DEFAULT_FETCH_TIMEOUT_MS`=600_000，`src/shared/utils/runtimeTimeouts.ts`）
- FIRST_BYTE_READYNESS = 80s（可扩展到 180s：`STREAM_READINESS_MAX_TIMEOUT_MS`）
- STREAM_IDLE = 600s，SSE_HEARTBEAT = 15s，BODY 后续读取 ≤ FETCH_TIMEOUT
- TLS 第一字节 watchdog 10s，Responses 首 idle 15s，客户端断开 grace 10s

**错误处理**：
- `open-sse/utils/error.ts#formatProviderError` → per-status map ERROR_TYPES + DEFAULT_ERROR_MESSAGES；401/403/429/5xx 分类入 `checkFallbackError`（`open-sse/services/accountFallback.ts`）决定"封禁连接/武器化冷却/账号降级"。
- 配额描述：`quotaPreflight.ts`（`getQuotaFetcher` per provider 的 `services/*QuotaFetcher.ts`）。
- RateLimitReason 枚举：auth_error / quota_exhausted / rate_limit_exceeded / model_capacity / server_error / unknown。

---

## 4. Combo/路由策略

### 全量策略（`src/shared/constants/routingStrategies.ts`）

`ROUTING_STRATEGY_VALUES`：
```
priority, weighted, round-robin, context-relay, fill-first, p2c, random,
least-used, cost-optimized, reset-aware, reset-window, headroom, quota-weighted,
strict-random, auto, lkgp, context-optimized, cache-optimized, fusion, pipeline
```
内部专用：`quota-share`（不外露）。aliases：`usage`→least-used, `context`→context-optimized, `weekly-reset`/`reset-window-order`→reset-window；未知→priority。

auto 细分（`AUTO_ROUTING_STRATEGY_VALUES`）：rules, score, cost, eco, latency, fast, sla-aware, sla, lkgp。

账户级 fallback 单独子集：`ACCOUNT_FALLBACK_STRATEGY_VALUES` = priority / weighted / fill-first / round-robin / p2c / random / least-used / cost-optimized / strict-random。

各策略语义（`open-sse/services/combo.ts` + `combo/*`）：
- `priority`：按序列出，失败顺延；`fill-first`：优先填满第一个直到不可用；`round-robin`：循环年均分（session 可以被 nativeTurnPin 绑住 `combo/nativeCodexTurnPin.ts`）；`p2c`：power-of-2-choices，靠 latency predictor（`comboPredicates.ts#PREDICTIVE_TTFT_MIN_SAMPLES=5`）；`weighted`：权重随机；`least-used`/`usage` 选当前使用量最小者；`cost-optimized` 按模型单价；`reset-aware`/`reset-window`（`RESET_WINDOW_NAMES`）按配额重置周期排序；`headroom`/`quota-weighted` 按剩余配额百分比加权（`comboPredicates.ts; comboPreflight`）；`lkgp` last-known-good provider；`session-affinity`（`services/sessionAffinityPin.ts`）、`explicitInactiveProbe` 主动探活；`cache-optimized` (`combo/promptCacheAffinity.ts`) 用 prompt cache 全局亲和；`fusion`/`pipeline`（复杂管道，见 `combo.ts#tryFusionDispatch/tryPipelineDispatch`）；`auto` 有独立的 `autoStrategy.ts`（任务复杂度分类 code/reasoning/simple/medium，由 `autoCombo/scoring.ts#projectAccountTier` 排序）。

### 失败重试/致命切分

`open-sse/services/combo/comboPredicates.ts`：
- `MAX_COMBO_DEPTH = 3`（默认；硬上限 10）
- `MAX_GLOBAL_ATTEMPTS = 30`（默认；硬上限 200）
- `MAX_FALLBACK_WAIT_MS = 5000`（等待 cooldown 解一个 slot 的最大等待）
- `COMBO_LOOP_SAFETY_TIMEOUT_MS = 10*60*1000`（每 whole combo 最多 10 分钟）
- `COMBO_SAFETY_DRAIN_MS = 2000`
- `UNAVAILABLE_LABEL_GRACE_MS = 60*1000`
- Context-overflow 400 / param-validation 400 / model-scoped 400 有独立谓词，决定"跳到下一个 provider" vs "换用户可修参数" vs "永久封禁该 model"。
- 可预测 TTFT fast-skip（`shouldSkipForPredictedTtft`）。

### 熔断/冷却（`open-sse/config/constants.ts`）

Per-connection（账户）：
- `BACKOFF_CONFIG = {base:1000, max:2*60*1000, maxLevel:15}`（指数退避，`calculateBackoffCooldown`）
- `BACKOFF_STEPS_MS = [60s, 120s, 300s, 600s, 1200s]`（每模型封禁递进）
- `COOLDOWN_MS = {unauthorized:2min, paymentRequired:2min, notFound:2min, notFoundLocal:5s, transientInitial:5s, transientMax:60s, transient:5s, requestNotAllowed:5s, rateLimit:2min, serviceUnavailable:2s, authExpired:2min}`

Circuit breaker（`PROVIDER_PROFILES`，`constants.ts:252`）：
| profile | transient冷却 | rate limit冷却 | threshold | reset | provider breaker |
|---|---|---|---|---|---|
| oauth | 5s | 60s | 8 | 60s | fail>10 in 15min → 5min cooldown |
| apikey | 3s | 0 (用 retry-after) | 12 | 30s | fail>15 in 30min → 10min cooldown |
| local | 2s | 5s | 2 | 15s | fail>2 in 5min → 1min cooldown |

自适应 backoff：`degradationThreshold`（oauth 5 / apikey 7）进 DEGRADED，`maxBackoffMultiplier`（8x / 4x），`backoffEscalationCount`（2 / 3）。全部可通过 `OMNIROUTE_CIRCUIT_BREAKER_*` / `OMNIROUTE_PROVIDER_BREAKER_*` 环境变量覆盖。

`DEFAULT_API_LIMITS = {requestsPerMinute:60, minTimeBetweenRequests:350, concurrentRequests:6}`（Bottleneck 式限流）。

失败三角形（`services/combo.ts` 顶端流程）：
1. quotaPreflight (`evaluateQuotaCutoff`)；2. 持贝过低3S；3. execution candidates → providerExecutionPipeline；4. 失败：`checkFallbackError` → `recordComboFailure` → `applyNativeCodexTurnPin` / 见 sse/chatDispatch `chatPredicates.ts` → 尝试下个 candidate 直到 MAX_GLOBAL_ATTEMPTS / COMBO_LOOP_SAFETY 超时 → 全败时 `errorResponseWithComboDiagnostics` 带 combo 名/model/diagnostics。

quota-share 策略特殊：并发 slot 由 `combo/quotaShareConcurrency.ts` 记账。

---

## 5. 配置

### 凭据/配置来源层级

1. 数据库：`storage.sqlite`（`bin/cli/data-dir.mjs#resolveStoragePath`），dataDir 默认 `~/.omniroute`（legacy），控制 `provider-nodes` (`getCachedProviderConnections`)、模型别名、combo。加密列用 `STORAGE_ENCRYPTION_KEY`。
2. 文件：`$DATA_DIR/provider-credentials.json`（`open-sse/config/credentialLoader.ts`），与 db provider-nodes 合并。
3. 环境变量：API keys (`*_API_KEY`)、OAuth IDs/secrets（如 `GEMINI_OAUTH_CLIENT_ID`、`QODER_OAUTH_TOKEN_URL`）、预算 (`OMNIROUTE_*`)、`LOCAL_HOSTNAMES`、`.env` 文件自动加载。

### 默认端口

CLI `omniroute serve`：`--port <p>` || `process.env.PORT` || `20128`（`bin/cli/commands/serve.mjs:115/146`）。
辅助端口：`API_PORT`（默认=port）、`DASHBOARD_PORT`（默认=port）、`OMNIROUTE_PORT`。
`src/shared/constants/providers/upstream-proxy.ts` 里 defaultPort: 8317 — 仅指内置 mitm/upstream 例程，非主网关。

### 主要环境变量

基础：`BASE_URL`、`API_PORT`、`DASHBOARD_PORT`、`DATA_DIR`、`XDG_CONFIG_HOME`、`APPDATA`、`OMNIROUTE_LANG`、`OMNIROUTE_API_KEY`、`OMNIROUTE_BASE_URL`（CLI），`OMNIROUTE_READY_TIMEOUT_MS=60000`、`OMNIROUTE_TLS_CERT/KEY`、`OMNIROUTE_CLI_SKIP_REPO_ENV=1`、`CI`、`OMNIROUTE_NO_UPDATE_NOTIFIER`。
超时：`REQUEST_TIMEOUT_MS/FETCH_TIMEOUT_MS=600000`、`STREAM_IDLE_TIMEOUT_MS=600000`、`STREAM_READINESS_TIMEOUT_MS=80000`、`STREAM_READINESS_MAX_TIMEOUT_MS=180000`、`FETCH_CONNECT_TIMEOUT_MS=30000`、`FETCH_KEEPALIVE_TIMEOUT_MS=4000`、`FETCH_BODY_TIMEOUT_MS`、`FETCH_HEADERS_TIMEOUT_MS`、`SSE_HEARTBEAT_INTERVAL_MS=15000`、`STREAM_DISCONNECT_GRACE_PERIOD_MS=10000`、`TLS_FIRST_BYTE_WATCHDOG_MS=10000`、`RESPONSES_FIRST_BYTE_TIMEOUT_MS=15000`。
熔断：`OMNIROUTE_CIRCUIT_BREAKER_OAUTH_THRESHOLD/RESET_MS`、`OMNIROUTE_CIRCUIT_BREAKER_API_KEY_*`、`OMNIROUTE_CIRCUIT_BREAKER_LOCAL_*`、`OMNIROUTE_PROVIDER_BREAKER_*`（_FAILURE_THRESHOLD/_FAILURE_WINDOW_MS/_COOLDOWN_MS）、`DEGRADATION_THRESHOLD/MAX_BACKOFF_MULTIPLIER/BACKOFF_ESCALATION_COUNT`。
其他：`CREDENTIAL_HEALTH_CHECK_INTERVAL=300000`（min 10000）、`CREDENTIAL_HEALTH_CACHE_TTL=300000`、`STREAM_RECOVERY_ENABLED`、`STORAGE_ENCRYPTION_KEY`、`CI`、`OMNIROUTE_NO_UPDATE_NOTIFIER`、`DATA_DIR/.ENV`；provider key/oAuth per-provider vars 见 `oauth.clientIdEnv`。

### 数据/Disk 布局

`$DATA_DIR/`：`storage.sqlite`（DB, `src/lib/db/**`），`provider-credentials.json`，`.env`，`server.env`（Electron 旧版），logs。

---

## 6. CLI（bin/omniroute.mjs）

入口流程：
1. 特殊路径（先于 Commander）：`--version/-V` 快速全局路径（打印 pkg.version 后退出，跳过 tsx/polyfill/命令注册），`--mcp`（stdio MCP），`reset-encrypted-columns`，`reset-password`。
2. Node 支持检测（`getNodeRuntimeSupport`）；register tsx/esm + polyfill + `@/` alias resolver（`bin/aliasResolver.mjs`，支持全局安装时裸导入到 `<ROOT>/src/`）。
3. Electron `server.env` → `.env` 单向迁移（若 `.env` 不存在）。
4. loadEnvFile：`$DATA_DIR/.env` → `~/.omniroute/.env` → `cwd/.env` → `<pkg>/.env`；先到先得，被遮蔽变量以 warning 打出（#6194）。
5. 首次运行若无 `STORAGE_ENCRYPTION_KEY` 且无 `storage.sqlite`，自动 randomBytes(32).hex 生成并写回 `$DATA_DIR/.env`（有问题时不动，见 #1622 guard）。
6. `--lang` → `bin/cli/i18n.mjs#setLocale`（先用伪直通）。
7. update-notifier（24h一次）。
8. 主入口：`createProgram()`（`bin/cli/program.mjs`），Commander；全局 flag：`--output table|json|jsonl|csv`、`-q/--quiet`、`--no-color`、`--timeout <ms>`(default 30000)、`--api-key`（env `OMNIROUTE_API_KEY`）、`--base-url`（env `OMNIROUTE_BASE_URL`）、`--context <name>`（env `OMNIROUTE_CONTEXT`）、`--lang <code>`。
9. 88 个子命令（`bin/cli/commands/*`）：serve(models/completion/combos/providers...) 具体见 `bin/cli/commands/registry.mjs`。注意：CLI 用 `createProgram` 与 `parseAsync`。

### serve 子命令（默认，`bin/cli/commands/serve.mjs`）

flag：`--port <p>`（fallback `PORT` env）、`--no-open`、`--daemon`、`--log`、`--no-recovery`、`--max-restarts <n>`（默认 2）、`--tray/--no-tray`、`--ready-timeout <ms>`（env `OMNIROUTE_READY_TIMEOUT_MS` 默认 60000）、`--tls-cert <path>`、`--tls-key <path>`（env `OMNIROUTE_TLS_CERT/KEY`）、`--tray-worker`/`--tray-ready-port`（hidden）。
行为：spawn() 子进程跑 Next.js standalone（`dist/server.js` 或 `app/`），pidfile `~/.omniroute/pid`，浏览器自动打开（除非 `--daemon` 或 `--no-open`）；`waitForServer` 轮询 `/healthz` 直到 ready 或 timeout。崩溃重启由 `ServerSupervisor`（`bin/cli/runtime/processSupervisor.mjs`）with `detectMitmCrash`。

其他常用：`status`、`stop`（用 pidfile）、`update`（npm 升级 `--apply`）、`providers`、`combos`、`models`、`logs`、`doctor`、`backup/restore`、`setup-<client>`（claude/codex/cursor/aider/goose 等 IDE/agent 配置写入）。

---

## 7. 可选特性一句话

- **RTK / Caveman 压缩**（`src/shared/validation/compressionConfigSchemas.ts`、`src/app/api/compression`）：出/入站 token 压缩框架，RTK 有 minimal/standard/aggressive 三档 + raw-output 保留策略；Caveman 有 lite/full/ultra 三档强度；在请求 headers（`chatCore/headers.ts#resolveCompressionHeader`）与流处理（`streamingPipeline`）注入，可 per-request via header echo，per-connection via default settings。
- 其他可选网关增强：思维签名恢复（`thinkingSignatureRecovery.ts`）、语义缓存（`idempotencyCache`,`semanticCache`）、prompt injection guard、policy/guardrail 注册（`src/sse/`）、tool-loop 服务端编排（`serverOwnedToolLoopWire.ts`）、`responseSanitizer`、`videoCombo`、`speechCombo`、`imageCombo`、SSE streamRecovery（holdback 750ms，buffer 64KiB，4 次重试，最小 resume-overlap 8 字符，`constants.ts#STREAM_RECOVERY`）、吞吐 watchdog（30s warmup + 30s window，≥4 B/s 有效字节）。

---

## 附注

- open-sse 与 src/sse 有轻微重叠：前端 `src/sse/handlers/*` 在 HTTP 边界"前段"搭建（admission、auth、requestBody、dispatch），真正翻译与上游执行在 `open-sse/**`。Rust 版可以合并两者。
- "legacy PROVIDERS" shape（`config/constants.ts#PROVIDERS`）为延迟 `Proxy` lazy-init (`generateLegacyProviders` + `loadProviderCredentials`)，供 `services/provider.ts#getProviderConfig` 与 executor 层按字符串 id 查询。
- Pino 日志 + `forwardDashboardEventToLiveWs` 报文/路由事件（`open-sse/utils/routing`）→ dashboard 内的 WS 流。
