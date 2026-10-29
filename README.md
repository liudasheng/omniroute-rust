# OmniRoute-Rust

**A full Rust rewrite of the OmniRoute AI gateway** (reference: [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x, originally TypeScript/Next.js). One endpoint, multi-provider routing, quota-aware automatic fallback, SSE streaming format translation.

> 📖 中文文档：[README_zh.md](README_zh.md) ｜ Docs: [docs/BENCHMARK.md](docs/BENCHMARK.md) · [docs/PARITY.md](docs/PARITY.md) · [docs/PAGES.md](docs/PAGES.md) · [docs/ORIGINAL-SPEC.md](docs/ORIGINAL-SPEC.md) · [docs/LRC.md](docs/LRC.md) ｜ 中文文档见 [README_zh.md](README_zh.md) 与 [docs/zh/](docs/zh/)。

## Benchmark summary (vs the original production stack, same machine + same mock upstream)

| Metric | omniroute-rust | Original (TS) | Improvement |
|---|---|---|---|
| Idle memory (process-tree RSS) | **9.4 MB** | 718 MB | ~76× |
| Peak memory (SSE, 64 concurrent) | **31.0 MB** | 1.26 GB | ~41× |
| `/healthz` throughput (64 concurrent) | **16,032 rps** | 373 rps | ~43× |
| JSON proxy throughput (64 concurrent) | **3,381 rps** | 32 rps | ~106× |
| Proxy latency p50/p99 (64 concurrent) | **13 / 32 ms** | 1,792 / 3,084 ms | — |
| SSE streaming throughput (64 concurrent) | **1,077 rps** | 32 rps | ~34× |

Zero errors on both sides in every scenario. Re-run on 2026-09-20 on the current
build with identical limits (100,000 RPM / 0 ms / 128 concurrent) and the same master key
on both gateways; the bench instance never shares the live service port.

Methodology and full data: [docs/BENCHMARK.md](docs/BENCHMARK.md).

## Features

- **OpenAI-compatible API**: `/v1/chat/completions`, `/v1/completions` (legacy), `/v1/responses`, `/v1/models`, `/v1/embeddings|rerank|moderations`
- **Multimodal**: image input in chat across all three upstream formats (`image_url` verbatim for openai-format, claude base64/url blocks, gemini inlineData/fileData); generation surfaces via single-provider passthrough — `/v1/images/{generations,edits,upscale}`, `/v1/audio/{transcriptions,translations,speech}` (multipart raw passthrough), `/v1/videos`, `/v1/ocr`, `/v1/files`, `/v1/batches` (provider via `provider/model` prefix, multipart `model` field, or `x-omniroute-provider` header)
- **Anthropic-native API**: `/v1/messages`, `/v1/messages/count_tokens` (claude wire format in/out, auto-translated to the target provider)
- **Health endpoints**: `/healthz`, `/readyz`, `/livez`, `/api/health(/ping)`; unknown paths return an OpenAI-shaped JSON 404 (never HTML)
- **Multi-provider**: 146 built-in providers (24 hand-written + 122 bulk-extracted from the original registry: plain-HTTP API-key entries in openai/openai-responses/claude/gemini formats) plus dynamic `openai-compatible-*` / `anthropic-compatible-*` / `anthropic-compatible-cc-*` families (see [docs/PARITY.md §2](docs/PARITY.md))
- **Combo routing strategies**: priority (failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto; `MAX_GLOBAL_ATTEMPTS=30`, `MAX_COMBO_DEPTH=3`, 10-minute combo loop safety timeout. A combo **step** may be a `provider`, a `provider/model` (both with an optional `=weight`), **another combo by name**, or an **`auto/*` virtual combo** (e.g. `coding-free → auto/free`): nested steps expand in place at request time, bounded by `MAX_COMBO_DEPTH` and cycle-checked; `POST /v1/combos/managed` echoes the expanded list as `resolved`, and the dashboard combo builder offers auto virtual combos and existing combos in the step picker
- **Circuit health**: error-class cooldowns (401/402/404→2min, 5xx→2s, network→5s), exponential backoff (1s base, 2min cap, 15 levels), provider-level breakers (oauth/apikey/local profiles); rate limiting **queues and waits** (`RATE_LIMIT_MAX_WAIT_MS=30000`, matching the original's request-queue semantics)
- **SSE streaming**: stateful chunk-by-chunk openai↔claude↔gemini translation; `forceStream` providers (kimi) fold upstream SSE back to JSON when the client asked for non-streaming; keepalive heartbeats (15s)
- **Web dashboard + PWA (upstream-parity UI)**: embedded single-page dashboard at `/dashboard` mirroring the original's sidebar **1:1** (`sections.ts`: 10 sections, 8 groups, 94 items in original order with `labelFallback`/`subtitleFallback` and deterministic per-item icon accents) and rendering **76 pages**: 34 data-backed pages (Home with quick-start + provider topology + recent requests, Endpoints, API Manager, Providers, Combos, Provider Quota, Compression + all context engines, Playground, Combos Studio, Translator, Batch, Traffic inspector, Usage, Combo Health, Utilization, Cache Health, Route tracing, Compression analytics, Provider Stats, Free tiers, Activity, Logs, Log export, Audit log, Health, Runtime, Resilience, Settings·General/Appearance/Sidebar/Resilience/Security) plus 42 honest stub pages for upstream-only modules (agent fleets, gamification, MCP/A2A/plugin runtimes, costs accounting, …) that state the gap instead of mocking data; every page fills the content width like the original (no 1120px cap); self-hosted Material Symbols font, upstream dark **and** light colour tokens, graph-paper wallpaper, collapsible sections, Ctrl+K quick navigation, 66 upstream locale packs with browser-language auto-detection (manual switch persisted, `en` fallback); installable as a PWA (full page audit: [docs/PAGES.md](docs/PAGES.md))
- **Account + multi-key management**: first-install admin password `CHANGEME` (upstream default) persisted as a salted SHA-256 record in `$DATA_DIR/dashboard-auth.json` (mode 600, re-read per check), `POST /v1/auth/login|logout|change-password`, `GET /v1/auth/me`, forced-change banner, and `omniroute reset-password [--password X | --password-stdin]` recovery; client keys via `GET/POST /v1/api-keys`, `PATCH/DELETE /v1/api-keys/{id}` (roles default/admin, `sk-or-*`, secret shown once); provider connections via `GET/POST /v1/provider-connections`, `PATCH/DELETE /{id}` and `POST /{id}/test` with runtime registry registration
- **Operations surface**: `GET /v1/stats/providers` (per-provider requests/errors/success/latency/tokens + live cooldown), `GET /v1/combo-health`, filtered `GET /v1/logs` (`provider`/`model`/`status`/`class`/`errors`/`stream`), `GET /v1/logs/export?format=csv\|json`, `GET /v1/audit` (management-action ring), `POST /v1/admin/service/restart\|stop` (systemd-friendly)
- **Electron desktop shell** (`electron/`): spawns the gateway, waits for `/healthz`, loads the dashboard; system tray (open/restart/quit), crash-restart, close-hides-to-tray
- **Token compression** (core RTK / Caveman modes, opt-in): modes `off | lite | standard | aggressive | ultra | rtk | lrc` selected via the `x-omniroute-compression` request header or `[compression]` toml / `OMNIROUTE_COMPRESSION` env; `GET /v1/compression` shows the effective config; responses carry `x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>` plus bounded rule counts when applicable (see [docs/PARITY.md §6](docs/PARITY.md); stacked and advanced original plan layers remain unsupported)
- **LRC — Layered Reversible Compression** (`lrc`, opt-in): bucket-aware, fail-open pipeline (protect → lossless → lossy-reversible → gate → ledger) that targets the parts of an agent payload the text-level engines never touch — historical reasoning, `tool_calls[].function.arguments`, tool results, and `tools` JSON schema — while keeping 100% protected-fragment fidelity and tool_call↔tool_result pairing; profiles `conservative | balanced | aggressive | extreme`; see [docs/LRC.md](docs/LRC.md) for the design and [docs/reports/lrc-benchmark-2026-10-22.md](docs/reports/lrc-benchmark-2026-10-22.md) for measured savings
- **Reasoning policy**: OpenAI-compatible `reasoning_effort`, `reasoning`, `max_completion_tokens`, Claude thinking, and Gemini thinking budgets; `[thinking]` or `OMNIROUTE_THINKING_MODE` supports `passthrough` (default), `auto`/`adaptive` (strip client reasoning for provider defaults), and `custom` with `OMNIROUTE_THINKING_BUDGET`. Use `auto` with `OMNIROUTE_COMPRESSION=lite` for small-context clients.

Clients with their own model registry do not infer selectable reasoning levels
from a generic `/v1/models` discovery response. Declare them in the client
profile with `reasoningEfforts` and the matching wire compatibility:

```yaml
api: openai-responses
models:
  - id: custom-model
    contextWindow: 1000000
    maxTokens: 131072
    input: [text, image]
    reasoningEfforts:
      off:
      low: low
      medium: medium
      high: high
```

Use `api: openai-responses` for reasoning-capable clients. The
`supportsReasoningEffort`/`thinkingFormat: openai` compat block is only for
clients deliberately using `/v1/chat/completions`.
- **Rate limiting**: defaults 60 RPM / 350ms min interval / 6 concurrent per connection (DEFAULT_API_LIMITS; applies to api-key providers only, local providers exempt), all overridable via environment variables
- **OpenCode Zen free lane (zero config, auto-discovered)**: the `opencode` / `opencode-zen` providers are routable with the literal `public` credential, so the anonymous free tier needs no registration. The roster is the live directory (`GET https://opencode.ai/zen/v1/models`, 300s TTL, cached at `$DATA_DIR/opencode-free.json`) intersected with the models.dev zero-cost verdict, minus locally blocklisted ids and ids this gateway has learned are region-blocked, delisted or rate-limited. Free-lane credential selection is a per-request decision: `auto` (default) round-robins the managed `opencode` / `opencode-zen` accounts and sends each one's own key, falling back to the literal `public` credential when the pool is empty, exhausted or cooling (`key` also allows the env/config key, `public` forces anonymous; `OMNIROUTE_OPENCODE_FREE_USE_KEY` stays as a legacy alias). A keyed Zen free call is gated by the workspace's "Allow free endpoints" privacy setting and answers 400 until the operator opts in; a refused keyed call parks the lane on `public` for 6h. A local UTC-day budget (`$DATA_DIR/opencode-free-budget.json`, 200 requests per account and for `public`, persisted across restarts) skips spent credentials without an upstream request, records the ceiling observed from a daily `429 FreeUsageLimitError` (`learned_limit`, conservative, kept across the UTC rollover and re-raised 20% per period after 7 stale days), cools the credential to its `Retry-After` (a short one is treated as a burst limit and cools only briefly), and — when two credentials of the same model answer a daily 429 within 60s — learns that the budget is egress-scoped and parks the lane for the rest of the window instead of probing accounts. `GET /v1/opencode-free` exposes the mode, the learned scope and the per-credential counters. Conversation sessions are memoised in `$DATA_DIR/opencode-free-sessions.json` (seed digests only), so a restart keeps reusing the `ses_*` ids the upstream already accepted instead of minting a fresh one per conversation; the same file keeps the last eight upstream-accepted (warm) sessions and the session-admission state. Dashboard probes use one shared session per provider rather than one per model. `OMNIROUTE_OPENCODE_FREE_SESSION_FALLBACK=true` additionally retries a request once on a warm session when a fresh session is refused with a long `Retry-After` (a session-admission refusal, not quota); it is off by default until the upstream behaviour is fully validated. Independently of that flag, a daily-class 429 that arrives after fewer than `OMNIROUTE_OPENCODE_FREE_LEARNED_MIN_SAMPLES` (default 10) accepted requests is treated as a **suspected session-admission refusal** rather than a dead quota: it only backs off for `OMNIROUTE_OPENCODE_FREE_SHORT_QUOTA_COOLDOWN_SECS` (default 60), writes no learned ceiling, does not park the window and teaches no egress scope; three such refusals in one UTC window upgrade to the normal daily-quota handling, so a genuinely tiny quota account is still honoured. `GET /v1/opencode-free` exposes `sessionAdmissionSuspected` and `shortQuotaRefusals`. The upstream's agent-shape gate is satisfied automatically: forced streaming, appended `bash`+`read` gate tools (`tool_choice: "none"` when the client sent none), and a canonical conversation-stable `ses_*` session id. `/v1/models` marks free ids with `"free": true`; `GET /v1/opencode-free` reports the roster, its provenance and every learned denial; `POST /v1/opencode-free/refresh` re-reads the directory now. A single-provider `opencode/...` call never fans out to other providers, so a lane failure is reported fast instead of chaining through unrelated providers
- **OpenCode subscription quota (auto-discovered)**: `GET https://opencode.ai/zen/go/v1/usage` (Bearer key) is polled in the background and parsed into rolling/weekly/monthly `{status, percent, resetsAt}` windows. A fresh reading of an exhausted window soft-skips the provider in the combo chain — with the reason in the error body — while the half-open probe still allows recovery; a stale reading never gates. `GET /v1/opencode-usage` shows the reading, `POST /v1/opencode-usage/refresh` re-reads it, and `/v1/quotas` embeds both lanes
- **Free-lane egress proxies (per-IP quota scaling)**: the free budget is counted **per egress IP**, so one address can be exhausted while another still answers. The anonymous lane can spread its traffic over a proxy pool — `OMNIROUTE_OPENCODE_FREE_PROXY_POOL` (comma-separated) or `[opencode_free] proxy_pool = [...]` in `omniroute.toml` — with `http://[user:pass@]host:port` (HTTP CONNECT; the userinfo becomes `Proxy-Authorization: Basic …`), `socks5://…` (the proxy resolves the hostname) or `socks5h://…` (the gateway resolves it locally). Selection is **sticky per conversation** (the session seed hashes to a stable pool index); a 429 or a tunnel failure rotates to the next exit, and one request tries at most `min(pool size, OMNIROUTE_OPENCODE_FREE_MAX_PROXY_EXITS)` exits (default 4). A managed account can instead carry its own `proxyUrl` in `provider-connections.json`: that egress applies only to that account, and an account without one keeps dialling directly. Budget counters gain an egress dimension, so every exit has its own learned ceiling/cooldown; a daily 429 parks only that exit's buckets and an egress-scoped lesson parks only that exit. A refused/unreachable proxy cools that exit for `OMNIROUTE_OPENCODE_FREE_PROXY_COOLDOWN_SECS` (default 30 s), refunds the reservation and rotates, so a request that never reached the upstream is not billed. `GET /v1/opencode-free` exposes `egress.exits` (masked `host:port`, mode, used/limit/in-flight/cooldown, healthy) and proxy credentials never appear in logs or status. `https://` proxy URLs are parsed but rejected (this transport does not speak TLS to the proxy), and the SOCKS scheme contract is the inverse of curl's: `socks5://` resolves at the proxy, `socks5h://` locally. When `omniroute.toml`/`.env` carries proxy credentials, keep it mode `0600`.
- **Zero-cost routing strategy**: `auto/free` (alias `auto/best-free`, which previously routed to whatever model each provider listed first) resolves to the discovered free roster only — the OpenCode Zen anonymous lane first, then any other provider's free ids — so a newly published free model joins the chain with no combo edit. The chain order is a **live composite score**, recomputed on every resolve from the discovered roster: **newness** (0.40 — how recently the id first appeared in this gateway's directory; the upstream publishes no release date), **context window** (0.30, normalised against 1M), **multimodal input** (0.20 — 0.5 for image, 0.5 for pdf), and a **recent-success** nudge (0.10, decaying over 24h). The three published criteria dominate, so a model this gateway just answered with never outranks a strictly newer and larger one — it only breaks near-ties toward something known to work. Equal totals fall back to alphabetical order so a re-resolve is stable. `GET /v1/opencode-free` reports each id's `score` breakdown (`newness`/`context`/`modalities`/`verified`/`total`/`contextWindow`), `scoreReason` and `firstSeenMs`; first-seen stamps are persisted so a restart does not make the whole roster look new. The directory itself is re-read every 12h (`OMNIROUTE_OPENCODE_FREE_TTL_SECONDS`, default 43200) — availability is learned per request and the score is recomputed at read time, so a shorter cadence buys nothing.


## Quick start

```bash
# Build
cargo build --release

# Configure providers (env vars or credentials file)
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# Start (default port 20128)
./target/release/omniroute serve

# Or with a custom port
./target/release/omniroute serve --port 3000

# Use (OpenAI clients)
curl http://127.0.0.1:20128/v1/chat/completions \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'

# Anthropic clients
curl http://127.0.0.1:20128/v1/messages \
  -d '{"model":"anthropic/claude-sonnet-4-5","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
```

Model strings support three shapes: `provider/model` (e.g. `openai/gpt-4o`), bare model aliases (`claude-sonnet-4-5` resolves to anthropic, `gpt-4o` to openai), and the `[1m]` suffix marking 1M context. `/v1/models` reports model-aware `contextWindow`/`maxTokens` (plus legacy `contextLength`); combo ids advertise the largest candidate window and filter smaller candidates at dispatch, so homogeneous 1M coding chains expose 1M instead of the old 128K fallback.

## Configuration

The data dir defaults to `~/.omniroute-rust` (override with `$OMNIROUTE_DATA_DIR`/`$DATA_DIR`); layering is first-wins per key, same as the original `loadEnvFile`:

| Source | Contents |
|---|---|
| `$DATA_DIR/provider-credentials.json` | provider credentials (`{"providers": {"openai": {"apiKey": "...", "baseUrl": "..."}}}` or flat `{"openai": {"api_key": "..."}}`); compatible with the original's camelCase fields |
| `$DATA_DIR/omniroute.toml` | port/host, `[providers.*]` tuning, `[[combos]]` |
| `.env` files | `$DATA_DIR/.env` → `~/.omniroute-rust/.env` → `cwd/.env`; `<ID>_API_KEY` auto-mapping (e.g. `ZAI_API_KEY` → provider `zai`) |
| Process environment | `OMNIROUTE_API_KEY` (gateway auth key), `PORT`, timeout/circuit/rate-limit overrides |

Example `omniroute.toml` (full example: [examples/omniroute.toml](examples/omniroute.toml)):

```toml
[server]
port = 20128

[[combos]]
name = "coding"
strategy = "priority"          # failover is an accepted alias
providers = ["anthropic/claude-sonnet-4-5", "openai/gpt-4o", "groq=2"]  # "=2" is a weighted weight
models = ["claude-sonnet-4-5"] # optional: only route these models through the combo

# Anonymous free-lane egress pool (optional): every entry is a separate
# upstream IP quota. Userinfo is masked in logs and the status endpoint.
[opencode_free]
# proxy_pool = [
#   "http://user:pass@proxy.example:3128",    # HTTP CONNECT
#   "socks5h://user:pass@socks.example:1080", # SOCKS5 with local DNS
# ]
# max_proxy_exits = 4       # exit attempts per request (default 4)
# proxy_cooldown_secs = 30  # short cooldown of a failed exit (default 30)
```

## CLI

```bash
omniroute serve [--port N] [--host ADDR]   # start the gateway (default subcommand)
omniroute status --port 20128              # pid + /healthz probe
omniroute stop                             # stop via pidfile
omniroute models [--base-url URL]          # GET /v1/models
omniroute providers                        # GET /v1/providers (connection health)
omniroute combos                           # locally configured combos
omniroute doctor                           # config/credentials checkup
# Global flags: --output json|table, --api-key, --base-url
```

## Benchmark tooling

```bash
cargo build --release --example mock_upstream --example loadgen
./target/release/examples/mock_upstream 9900 &            # high-performance mock upstream
./target/release/examples/loadgen --url http://127.0.0.1:20128/v1/chat/completions \
  --mode json|sse|get --concurrency 64 --duration 6 --model "openai-compatible-bench/mock-model"
# Output: requests / rps / p50 / p95 / p99 / errors
```

## Testing

```bash
cargo test          # 219 unit tests + 28 integration tests (full chain via mock upstream)
cargo test --test integration
cargo clippy        # 0 warnings
```

## Environment variables (same names/defaults as the original)

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | 20128 | Main port (`--port` overrides) |
| `OMNIROUTE_API_KEY` | unset | When set, /v1/* requires a Bearer key |
| `REQUEST_TIMEOUT_MS` | 600000 | Upstream total timeout |
| `FETCH_CONNECT_TIMEOUT_MS` | 30000 | Connect timeout |
| `STREAM_IDLE_TIMEOUT_MS` | 600000 | Stream idle timeout |
| `STREAM_READINESS_TIMEOUT_MS` | 80000 | First-byte readiness timeout |
| `SSE_HEARTBEAT_INTERVAL_MS` | 15000 | Heartbeat keepalive |
| `RATE_LIMIT_MAX_WAIT_MS` | 30000 | Max queue wait on the rate gate (same name as the original) |
| `OMNIROUTE_REQUESTS_PER_MINUTE` | 60 | Rate limit RPM |
| `OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS` | 350 | Rate limit min interval |
| `OMNIROUTE_CONCURRENT_REQUESTS` | 6 | Per-connection concurrency cap |
| `OMNIROUTE_THINKING_MODE` | passthrough | Reasoning policy: passthrough, auto, adaptive, custom |
| `OMNIROUTE_THINKING_BUDGET` | unset | Fixed reasoning budget used by custom mode |
| `OMNIROUTE_MODEL_CATALOG_AUTOUPDATE` | true | Background refresh of the model-capability catalog |
| `OMNIROUTE_MODEL_CATALOG_TTL_HOURS` | 24 | Refresh the catalog when older than this |
| `OMNIROUTE_MODEL_CATALOG_URL` | https://models.dev/api.json | Upstream capability catalog |
| `OMNIROUTE_OPENCODE_FREE_AUTOUPDATE` | true | Discover the anonymous OpenCode Zen free roster from the live directory |
| `OMNIROUTE_OPENCODE_FREE_TTL_SECONDS` | 43200 | Live-directory refresh cadence (12h) |
| `OMNIROUTE_OPENCODE_FREE_URL` | https://opencode.ai/zen | Zen base used by the free-lane discovery |
| `OMNIROUTE_OPENCODE_FREE_CREDENTIAL_MODE` | auto | Free-lane credential mode: `auto` rotates the managed account keys and falls back to `public`, `key` also uses the env/config key, `public` forces the anonymous credential |
| `OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PER_ACCOUNT` | 200 | Local UTC-day request ceiling per managed account (tightened per window by the learned ceiling) |
| `OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PUBLIC` | 200 | Local UTC-day request ceiling of the anonymous `public` credential |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT` | true | Learn the real ceiling from daily `429 FreeUsageLimitError` answers (`learned = min(learned, used_at_429)`, persisted across days) |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT_TTL_DAYS` | 7 | Raise a stale learned ceiling by 20% per elapsed period after this many days without a fresh 429 (0 disables) |
| `OMNIROUTE_OPENCODE_FREE_MAX_IN_FLIGHT_PER_ACCOUNT` | 2 | In-flight requests one credential may have before the request rotates to a sibling (0 = unlimited) |
| `OMNIROUTE_OPENCODE_FREE_SESSION_FALLBACK` | false | Retry once on a warm (already accepted) session when a fresh session is refused with a long `Retry-After`, before treating it as quota exhaustion (off until validated against the live upstream) |
| `OMNIROUTE_OPENCODE_FREE_SESSION_MEMO_FILE` | opencode-free-sessions.json | File (in the data dir) holding the conversation-session memo, the warm-session set and the admission state |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_MIN_SAMPLES` | 10 | Daily-class 429s observed after fewer accepted requests than this are treated as suspected session admission (short cooldown only, no learned ceiling, no window park); three such refusals per window upgrade to normal daily handling. 0 = old behaviour |
| `OMNIROUTE_OPENCODE_FREE_SHORT_QUOTA_COOLDOWN_SECS` | 60 | Cooldown of one suspected low-usage refusal (capped at the upstream `Retry-After` when one is given) |
| `OMNIROUTE_OPENCODE_FREE_PROXY_POOL` | unset | Anonymous free-lane egress pool, comma-separated `http(s)://[user:pass@]host:port`, `socks5://…` or `socks5h://…` entries; unset keeps the direct dial |
| `OMNIROUTE_OPENCODE_FREE_MAX_PROXY_EXITS` | 4 | Max proxy exits one free-lane request may try (also bounded by the pool size) |
| `OMNIROUTE_OPENCODE_FREE_PROXY_COOLDOWN_SECS` | 30 | Cooldown of a proxy exit whose tunnel failed (unreachable proxy, refused CONNECT, SOCKS negotiation failure) |
| `OMNIROUTE_OPENCODE_FREE_USE_KEY` | — | Legacy alias: `true` maps to `CREDENTIAL_MODE=key`, `false` to `public` |
| `OMNIROUTE_OPENCODE_QUOTA_AUTOUPDATE` | true | Discover OpenCode Go rolling/weekly/monthly subscription usage |
| `OMNIROUTE_OPENCODE_QUOTA_TTL_SECONDS` | 300 | Subscription-usage refresh cadence |
| `OMNIROUTE_PROVIDER_SYNC_AUTOUPDATE` | true | Refresh provider-connection models from each upstream `/models` |
| `OMNIROUTE_PROVIDER_SYNC_TTL_SECONDS` | 14400 | Provider-model sync cadence in seconds (default 4h; sub-hour values work for smoke tests) |
| `OMNIROUTE_PROVIDER_SYNC_TTL_HOURS` | — | Coarse fallback cadence used only when the seconds variable is unset |
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | Data dir |

Model capabilities (context window, max output, input modalities, reasoning)
come from a runtime catalog snapshot cached at `$DATA_DIR/model-catalog.json`
and refreshed from `models.dev`, since most providers only return model ids.
`GET /v1/model-catalog` reports the cache state, `POST /v1/model-catalog/refresh`
forces a refresh.

Provider connections can refresh `/models` with `POST /v1/provider-connections/{id}/sync-models`.
Manually added models can be removed with `DELETE /v1/provider-connections/{id}/models/{model}`.
The gateway persists non-secret context, output, input-modality, vision/PDF, and
reasoning-level metadata returned by that endpoint and prefers it over inferred
model defaults on the next catalog request. Duplicate synced and registry models
are shown once as synced entries.
The connection's `api_type` controls the outbound wire protocol; set it to
`openai-responses` when the upstream must receive `/v1/responses`.
OpenRouter remains an OpenAI Chat provider; reasoning-capable OpenRouter models
use the `thinkingFormat: openrouter` compatibility mapping instead of switching
the whole provider to Responses.

## License

MIT (same as the original project).
