# Feature Parity: omniroute-rust vs the original (TypeScript)

Reference: `diegosouzapw/OmniRoute` v3.8.x (shallow-clone analysis of the
`open-sse/` gateway engine + `src/server` HTTP layer + `bin/cli`). The original
project is ~14,400 files (Next.js desktop/PWA/UI, 352+ provider-specific web
executors, MCP/A2A, etc.). This repository is a **full Rust rewrite of the
gateway core**. Item-by-item comparison below; 中文版见 [docs/zh/PARITY.md](zh/PARITY.md)。

## 1. Endpoints

| Endpoint | Original | Rust | Notes |
|---|---|---|---|
| `GET/POST /v1/chat/completions` | ✅ | ✅ | JSON + SSE |
| `POST /v1/messages` (+count_tokens) | ✅ | ✅ | claude wire format in/out; count_tokens is a local estimate |
| `POST /v1/responses` | ✅ | ✅ | openai-responses ⇄ chat translation |
| `POST /v1/completions` | ✅ | ✅ | legacy prompt shape projected onto chat |
| `GET /v1/models`, `/v1` | ✅ | ✅ | `{object:"list", data:[{id,provider,contextLength,...}]}` |
| `POST /v1/embeddings|rerank|moderations` | ✅ | ✅ | single-provider passthrough via `provider/model` prefix |
| `POST /v1/images/{generations,edits,upscale}` | ✅ | ✅ | single-provider passthrough (`provider/model` prefix or `x-omniroute-provider` header); no IMAGE_PROVIDERS registry |
| `POST /v1/audio/{transcriptions,translations,speech}`, `/v1/speech-to-text`, `/v1/text-to-speech` | ✅ | ✅ | **raw passthrough**: client body + content-type forwarded verbatim (multipart supported; provider from the `model` form field or header) |
| `POST /v1/videos`, `/v1/ocr`, `/v1/files` | ✅ | ✅ | passthrough (ocr/files accept multipart) |
| `POST /v1/batches`, `GET /v1/batches`, `GET /v1/batches/{id}` | ✅ | ✅ | batches have no model field → provider via `x-omniroute-provider` header |
| `GET /healthz /readyz /livez /api/health(/ping)` | ✅ | ✅ | same shapes (`ok\n` / JSON) |
| Unknown paths | JSON 404 `unknown_route` | ✅ identical | never HTML |
| `/v1/combos(/test)`, `/v1/providers`, `/v1/quotas` | ✅ | ✅ | combos read-only + dry-run test; providers/quotas aggregated from in-memory circuit state |
| Management plane (dashboard session, CRUD, analytics) | ✅ | ✅ | Rust: session login + API keys + provider connections + analytics/audit/log-export (no database — JSON files + in-memory rings). See §9 |
| Error shape | `{error:{message,type,code}}` | ✅ identical | matches `errorConfig.ts#ERROR_TYPES` |

### Multimodal image input (chat)

Image input inside chat messages is supported across all three upstream
formats (parity: the original's content-block translation):

| Direction | Mapping |
|---|---|
| openai → openai-format | `image_url` parts pass through verbatim |
| openai → claude-format | `image_url` → claude image block: `data:` URLs → `{type:"base64", media_type, data}`; http(s) → `{type:"url"}` |
| claude → openai-format | claude image block → `image_url` (base64 source → data URL; url source → url) |
| openai → gemini | `data:` URL → `inlineData {mimeType, data}`; http(s) URL → `fileData {fileUri, mimeType}` |
| openai-responses → chat | `input_image` → `image_url` |

## 2. Provider system

- Original `providerRegistry.ts` ≈ 240 providers (OAuth / web-reverse executors:
  antigravity, grok-web, deepseek-web, cursor, bedrock, vertex, ...).
- The Rust version statically registers **21 high-value API-key providers**
  (anthropic, openai, gemini, glm, zai, kimi, deepseek, openrouter, groq, xai,
  mistral, together, fireworks, perplexity, minimax, siliconflow, dashscope,
  doubao, ollama, ollama-cloud, lmstudio) with URLs / auth headers / URL
  suffixes (`?beta=true`) matching the original registry:
  - anthropic: `https://api.anthropic.com/v1/messages?beta=true` + `x-api-key` + `anthropic-version: 2023-06-01`
  - gemini: `{base}/models/{m}:streamGenerateContent?alt=sse` + `x-goog-api-key`
  - kimi: `forceStream` (upstream always streams; the gateway folds back to JSON)
- Dynamic families `openai-compatible-*` / `anthropic-compatible-*` /
  `anthropic-compatible-cc-*` (cc family uses `/chat_completion?beta=true` +
  an `anthropic-beta` header) — same behavior as the original
  `services/provider.ts`.
- `parseModel` (`open-sse/services/model.ts`): `provider/model`, alias
  resolution, bare-model heuristics, `[1m]`/`:1m` extended-context suffix — all
  implemented; the Rust version additionally validates the prefix against the
  live registry at routing time (more permissive than the original's
  alias-only table).

## 3. Routing / fallback (combo)

| Item | Original | Rust |
|---|---|---|
| Strategy set | 21 (incl. fusion/pipeline/quota-share/context-relay) | **10 core**: priority, round-robin, fill-first, weighted, random, least-used, p2c, cost-optimized, lkgp, auto; aliases match (`failover`→priority, `usage`→least-used); unknown→priority |
| `MAX_COMBO_DEPTH` | 3 (hard 10) | 3 / hard 10 ✅ |
| `MAX_GLOBAL_ATTEMPTS` | 30 (hard 200) | 30 / hard 200 ✅ |
| `MAX_FALLBACK_WAIT_MS` | 5000 | constant retained ✅ |
| Combo loop safety timeout | 10 min | 10 min ✅ |
| Failure classification | `checkFallbackError` / RateLimitReason | ✅ same classes (auth/payment/not_found/rate_limit/server/network/client) |
| 400 handling | param-validation 400 = user-fixable: return immediately, no provider penalty | ✅ same semantics |
| Rate-limit queue | requests queue and wait up to `maxWaitMs` before the dispatch fails | ✅ same semantics (`RATE_LIMIT_MAX_WAIT_MS`) |
| Quota preflight | per-provider quota fetchers | ❌ approximated with in-memory counters (`/v1/quotas`) |

## 4. Circuit breaking / cooldowns / backoff

| Item | Original | Rust |
|---|---|---|
| `COOLDOWN_MS` | unauthorized/paymentRequired/notFound 2min, notFoundLocal 5s, serviceUnavailable 2s, transientInitial 5s | ✅ identical |
| `BACKOFF_CONFIG` | base 1s / max 2min / level 15 | ✅ identical |
| `BACKOFF_STEPS_MS` | [60s,120s,300s,600s,1200s] | ✅ identical (model-level ban escalation) |
| Three profiles (oauth/apikey/local) | threshold 8/12/2, reset 60s/30s/15s, provider breaker 10@15min→5min / 15@30min→10min / 2@5min→1min | ✅ identical |
| env overrides | `OMNIROUTE_CIRCUIT_BREAKER_*` | ✅ `*_THRESHOLD` / `*_RESET_MS` |
| `DEFAULT_API_LIMITS` | 60 RPM / 350ms / 6 concurrent | ✅ identical (sliding-window limiter + queue-like wait) |

## 5. Streaming translation

- Original: hub-and-spoke (any format → openai → target format), stateful
  per-chunk (`translateResponse` + initState).
- Rust: same architecture — `UpstreamSource` (upstream → openai chunks:
  openai passthrough / anthropic events / gemini SSE) + `InboundSink`
  (openai chunks → inbound format: openai / claude / responses / completions).
- Timeouts: connect 30s, request 600s, first-byte readiness 80s, stream idle
  600s, heartbeat 15s — same values.
- Heartbeat frame: the original uses `OPENAI_KEEPALIVE_FRAME`; the Rust
  version uses the SSE comment `: keepalive` (equivalent and safe for every
  compliant client; recorded divergence).
- `jsonBodyToSse` (synthesize SSE when the provider ignored `stream`) ✅;
  `forceStream` folding back to JSON ✅.

## 6. Token compression (RTK / Caveman)

The original's proactive context compression (`open-sse/services/compression/*`) is
now implemented with per-engine parity:

| Engine (mode) | Original | Rust |
|---|---|---|
| `lite` (RTK minimal tier) | `collapseWhitespace` (3+ newlines → 2, trailing spaces), `dedupSystemPrompt` (200-char key), `compressToolResults` (>2000 chars → word-boundary truncate + `...[truncated]`, lookback 80), `removeRedundantContent` (consecutive same-role identical content), `replaceImageUrls` (non-vision → `[image: format]`) | ✅ all five techniques, same constants |
| `standard` (Caveman) | 34-rule phrase compression, intensity lite/full/ultra, role contexts (all/user/assistant), `skipRules`, `minMessageLength=50`, `compressRoles=["user"]` default, preserved-block tombstoning (fences/inline code/URLs/paths/errors/stack frames), artifact cleanup, sentence recapitalization, code-dominant skip (≥3 lines, ≥30% code-like) | ✅ full rule table ported (34 rules, same patterns/maps/contexts/intensity ranks) |
| `aggressive` | tool-result compressors (fileContent head20+tail5 / grepSearch top30 / shellOutput ANSI-strip+last50+dedupe / json first5+last2 & top-20 keys / errorMessage head10+tail3) → progressive aging → rule summarizer `[COMPRESSED:summary]` → downgrade chain to caveman then lite when savings < 5% | ✅ same tool compressors + extractive summarizer (intents/files/errors/decision) + downgrade chain; divergence: progressive aging approximated by the summarizer step |
| `ultra` | Tier-A heuristic token pruning: scoreToken (force-preserve digits/URLs/paths/errors/fences; polarity words never pruned #13454; stopwords 0.1; ≤2 chars 0.2; Capitalized 0.8; ≥6 chars 0.7), prune to keepRate 0.5, minScore 0.3; SLM tier optional (falls back to heuristic) | ✅ same scoring table + pruning; the SLM tier is skipped (heuristic IS the ultra engine, matching the original's fallback path) |
| `rtk` | full filter registry per command type (npm/make/docker/custom filters), raw-output pointers, learn/verify | ⚠️ simplified: ANSI strip, progress-bar line filter, consecutive-duplicate dedupe, head+tail max-lines cap (default 200), document-read guard (#4559 — unknown content without command/error markers keeps its middle); per-command filter registry remains out of scope |
| `stacked` / `omniglyph` / codex-responses | engine pipelines | ❌ out of scope |

Selection precedence (parity: `resolveBasePlan`): master off → request header
`x-omniroute-compression` (`off|default|lite|standard|aggressive|ultra|rtk`;
unknown values fall through, never error) → auto-trigger at
`auto_trigger_tokens` → configured `default_mode`. Compression is **opt-in**
(default off, same as the original), configurable via the `[compression]`
toml table / `OMNIROUTE_COMPRESSION` env; every response carries
`x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>;
rules=<n>`; `GET /v1/compression` returns the effective configuration.

Token estimation is chars/4 (`estimateCompressionTokens` parity).

## 7. Configuration / CLI

- Default port **20128** (`--port` > `PORT` env > toml > 20128) — same as the
  original (8317 is only the original's mitm subsystem port).
- Credentials file is the original's `provider-credentials.json` (camelCase
  `apiKey`/`baseUrl` compatible; flat schema also accepted).
- `.env` three-layer first-wins loading is the same.
- CLI: the original ships ~88 subcommands (Electron tray, MCP stdio, dashboard
  management, backup/update, ...); the Rust version implements the core
  **7**: serve (default)/status/stop/models/providers/combos/doctor, with the
  global `--output json|table/--api-key/--base-url/--port` flags matching the
  original. pidfile start/stop logic mirrors the original `processSupervisor`.

## 8. Web dashboard / PWA / Electron desktop shell

The original's dashboard is a Next.js app (`src/app/(dashboard)`) wrapped by
an Electron shell (`electron/main.js`: spawn server → wait `/healthz` →
BrowserWindow + system tray + auto-updater). The Rust version replaces the
Next.js runtime with an **embedded web dashboard served by the gateway
itself** (no Node needed), plus a parity Electron wrapper:

| Surface | Original | Rust |
|---|---|---|
| Dashboard UI | Next.js React app (dashboard settings/analytics/logs pages) | ✅ embedded single-page app at `/dashboard` (vanilla JS, no build step): Overview cards, Providers health, Models catalog (filter), Combos, Compression runtime editor, request Logs; `/` redirects to `/dashboard` |
| PWA | installable dashboard | ✅ `manifest.webmanifest` (standalone) + service worker (`/dashboard/sw.js`, network-first, never caches `/v1/*` API) |
| Electron shell | `electron/main.js` + tray + auto-updater + login/remote mode | ✅ `electron/` (spawn gateway → `/healthz` readiness → window; tray open/restart/quit; close-hides-to-tray; crash restart parity with ServerSupervisor; single-instance lock). Divergence: no auto-updater / remote login / credential inspection |
| Request history | SQLite request-history DB | ✅ in-memory bounded ring buffer (last 500 requests) + `GET /v1/logs` |
| Runtime stats | dashboard analytics | ✅ `GET /v1/stats` (uptime, request/failure counters, process RSS via /proc) |
| Compression management UI | compression settings pages | ✅ runtime-editable compression config: `GET/POST /v1/compression` (validated) + dashboard editor; boot values still come from toml/env |
| Dashboard auth | dashboard JWT/session | ✅ account login: first deployment default admin password **CHANGEME** (parity); `POST /v1/auth/login|logout|change-password`, sessions 7-day + cookie; `OMNIROUTE_ADMIN_PASSWORD` env override; "default password active" banner until changed |

## 9. Explicitly out of scope (beyond the gateway core)

- Web-reverse executors (hundreds of `open-sse/executors/*.ts`: chatgpt-web,
  claude-web, gemini-web, cursor, antigravity, grok-web, kiro, ...)
- MCP/A2A protocol servers, the WebSocket routing event stream
- Semantic/idempotency caches, server-owned tool-loop, streamRecovery,
  throughput watchdog (compression itself is implemented — see §6)
- SQLite storage + `STORAGE_ENCRYPTION_KEY` encrypted columns (the Rust
  configuration comes from files + environment variables)

## Verification

- `cargo test`: 66 unit (parsing/translation/strategies/circuits/rate
  limiting/SSE parsing) + 9 integration (mock upstream, full chain: non-stream,
  SSE, claude⇄openai both directions, failover, catalog, auth, 404s).
- Binary smoke test: `omniroute serve` then `/healthz`, `/v1/models`, CLI
  status/doctor.
- Benchmarks vs the original: see [BENCHMARK.md](BENCHMARK.md).

## 9. Dashboard & management plane (Rust implementation)

The Rust dashboard mirrors the upstream sidebar information architecture
(`src/shared/constants/sidebarVisibility/sections.ts`) and the upstream
`DashboardLayout` / `Sidebar` / `LanguageSelector` components, without running
Next.js: the gateway serves an embedded SPA at `/dashboard`.

### Auth
| Item | Rust |
|---|---|
| First-install password | **`CHANGEME`** (parity with the original's first-deployment default) |
| Storage | `$DATA_DIR/dashboard-auth.json` — salted SHA-256, mode 600, re-read per check so a reset applies without a restart |
| Env override | `OMNIROUTE_ADMIN_PASSWORD` (wins on every boot) |
| Endpoints | `POST /v1/auth/login` (7-day session + HttpOnly cookie), `/logout`, `GET /auth/me`, `POST /auth/change-password` |
| Recovery | `omniroute reset-password [--password X \| --password-stdin \| piped stdin]` (parity: `bin/reset-password.mjs`) |
| Default-password UX | `using_default_password` in `/auth/me` + forced-change banner |

### Management endpoints
| Endpoint | Purpose |
|---|---|
| `GET/POST /v1/api-keys`, `PATCH/DELETE /v1/api-keys/{id}` | multi-key management; roles default/admin; `sk-or-*`; secret shown once; model-access/usage-limit/chaos fields per `createKeySchema` |
| `GET/POST /v1/provider-connections`, `PATCH/DELETE /{id}`, `POST /{id}/test` | connection CRUD with runtime registry registration + 1-token connectivity probe |
| `GET /v1/provider-catalog` | 352-provider catalog (freeTier/ide/serviceKinds/website re-extracted from `src/shared/constants/providers/**`, partition tags per the original's ID sets) joined with live stats (`total/connected/error/allDisabled`), registry+connection models for the model-search filter, dynamic `compatibleNodes`, and honest `expirations`/`blockedProviders`/`openRouterStats` stubs |
| `POST /v1/providers/test-batch` `{mode, providerId?, connectionIds?}` | parity with `/api/providers/test-batch`: modes all/provider/oauth/free/no-auth/apikey/compatible/web-cookie/search/audio/local/upstream-proxy/cloud-agent/ide/selected (enabled-only except `selected`); `{mode, results[], summary{total,passed,failed}, testedAt}` |
| `GET /v1/stats`, `/v1/stats/providers`, `/v1/quotas`, `/v1/combo-health` | runtime + per-provider/per-combo analytics |
| `GET /v1/logs` (+`provider`,`model`,`status`,`class`,`errors`,`stream`), `GET /v1/logs/export?format=csv\|json` | request analytics + export |
| `GET /v1/audit` | management-action audit ring (login, keys, providers, password, service) |
| `GET/POST /v1/compression`, `GET /v1/settings` | runtime compression config + limits/auth view |
| `POST /v1/admin/service/restart\|stop` | sidebar service actions (systemd-friendly: restart aborts, stop exits 0) |

### Sidebar coverage
Implemented with real gateway data (26 pages; full audit: [PAGES.md](PAGES.md)):
Home (quick start, provider topology, recent requests) · Endpoints · API
Manager · Providers · Combos · Provider Quota · Compression (settings +
Caveman/RTK/Ultra/Aggressive/Lite) · Playground · Translator · Batch ·
Traffic inspector (log row-detail) · Usage · Combo Health · Utilization ·
Compression analytics · Provider Stats · Free tiers · Activity · Logs · Log
export · Audit log · Health · Runtime · Resilience · Settings
(General/Appearance/Sidebar/Resilience/Security) · Docs.

UI parity details: 66 upstream locale packs (`src/i18n/messages/*`) with the
`LanguageSelector` picker, Material Symbols Outlined self-hosted font,
upstream dark **and light** colour tokens (`globals.css`), graph-paper
wallpaper, collapsible sidebar sections persisted in
`sidebar-expanded-sections`, deterministic per-item icon accents
(`getDeterministicIconAccent` port), 220px sidebar, Ctrl+K quick navigation,
`--fd-sidebar-width`/`#10141e` tokens.

### Deliberate gaps (no Rust equivalent)
OAuth/web-reverse executors (antigravity, grok-web, cursor, ...) · MCP stdio
engine · A2A · cloud agents/conductor · gamification/tokens/leaderboard ·
media-provider pipelines · proxy pool/webhooks editor · feature-flag and cache
admin pages · `costs/*` cost accounting (no pricing table) · `analytics/evals`,
`analytics/search` · Electron desktop shell (the Rust build ships the PWA only).

### Providers page parity notes
The Providers page mirrors the original's structure: first-provider hint,
summary card (provider/model search, All/Configured/Compact display modes,
onboarding wizard, file import with template, Test-all), category chips with
configured/total counts, media (service-kind) filter chips, and the original
section order (Compatible → OAuth → IDE → Web/Cookie → Free → API-key/LLM →
No-auth → Upstream-proxy → Web-fetch → Aggregators → Enterprise → Cloud-agent
→ Local → Search → Embedding → Image → Audio → Video) with per-section Test-all
and Test-results modal (`mode/results/summary`), provider cards with
connected/error/disabled status + enable toggle + 1-token test, a detail view
(connections CRUD, models, test) standing in for `/dashboard/providers/[id]`,
and `?search=&model=&mode=&cat=&media=` URL filter sync. Honest divergences:
OAuth sign-in / browser-cookie sessions / IDE keychain import / expiry tracking
/ OpenRouter popularity / risk-term copy are rendered as informational states
(the cards navigate to the detail view where a key or base URL can be stored);
the gateway executes only API-key / compatible / local connections.
