# OmniRoute-Rust

**A full Rust rewrite of the OmniRoute AI gateway** (reference: [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x, originally TypeScript/Next.js). One endpoint, multi-provider routing, quota-aware automatic fallback, SSE streaming format translation.

> 📖 中文文档：[README_zh.md](README_zh.md) ｜ Docs: [docs/BENCHMARK.md](docs/BENCHMARK.md) · [docs/PARITY.md](docs/PARITY.md) · [docs/PAGES.md](docs/PAGES.md) · [docs/ORIGINAL-SPEC.md](docs/ORIGINAL-SPEC.md) ｜ 中文文档见 [README_zh.md](README_zh.md) 与 [docs/zh/](docs/zh/)。

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
- **Combo routing strategies**: priority (failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto; `MAX_GLOBAL_ATTEMPTS=30`, `MAX_COMBO_DEPTH=3`, 10-minute combo loop safety timeout
- **Circuit health**: error-class cooldowns (401/402/404→2min, 5xx→2s, network→5s), exponential backoff (1s base, 2min cap, 15 levels), provider-level breakers (oauth/apikey/local profiles); rate limiting **queues and waits** (`RATE_LIMIT_MAX_WAIT_MS=30000`, matching the original's request-queue semantics)
- **SSE streaming**: stateful chunk-by-chunk openai↔claude↔gemini translation; `forceStream` providers (kimi) fold upstream SSE back to JSON when the client asked for non-streaming; keepalive heartbeats (15s)
- **Web dashboard + PWA (upstream-parity UI)**: embedded single-page dashboard at `/dashboard` mirroring the original's sidebar **1:1** (`sections.ts`: 10 sections, 8 groups, 94 items in original order with `labelFallback`/`subtitleFallback` and deterministic per-item icon accents) and rendering **76 pages**: 34 data-backed pages (Home with quick-start + provider topology + recent requests, Endpoints, API Manager, Providers, Combos, Provider Quota, Compression + all context engines, Playground, Combos Studio, Translator, Batch, Traffic inspector, Usage, Combo Health, Utilization, Cache Health, Route tracing, Compression analytics, Provider Stats, Free tiers, Activity, Logs, Log export, Audit log, Health, Runtime, Resilience, Settings·General/Appearance/Sidebar/Resilience/Security) plus 42 honest stub pages for upstream-only modules (agent fleets, gamification, MCP/A2A/plugin runtimes, costs accounting, …) that state the gap instead of mocking data; every page fills the content width like the original (no 1120px cap); self-hosted Material Symbols font, upstream dark **and** light colour tokens, graph-paper wallpaper, collapsible sections, Ctrl+K quick navigation, 66 upstream locale packs with browser-language auto-detection (manual switch persisted, `en` fallback); installable as a PWA (full page audit: [docs/PAGES.md](docs/PAGES.md))
- **Account + multi-key management**: first-install admin password `CHANGEME` (upstream default) persisted as a salted SHA-256 record in `$DATA_DIR/dashboard-auth.json` (mode 600, re-read per check), `POST /v1/auth/login|logout|change-password`, `GET /v1/auth/me`, forced-change banner, and `omniroute reset-password [--password X | --password-stdin]` recovery; client keys via `GET/POST /v1/api-keys`, `PATCH/DELETE /v1/api-keys/{id}` (roles default/admin, `sk-or-*`, secret shown once); provider connections via `GET/POST /v1/provider-connections`, `PATCH/DELETE /{id}` and `POST /{id}/test` with runtime registry registration
- **Operations surface**: `GET /v1/stats/providers` (per-provider requests/errors/success/latency/tokens + live cooldown), `GET /v1/combo-health`, filtered `GET /v1/logs` (`provider`/`model`/`status`/`class`/`errors`/`stream`), `GET /v1/logs/export?format=csv\|json`, `GET /v1/audit` (management-action ring), `POST /v1/admin/service/restart\|stop` (systemd-friendly)
- **Electron desktop shell** (`electron/`): spawns the gateway, waits for `/healthz`, loads the dashboard; system tray (open/restart/quit), crash-restart, close-hides-to-tray
- **Token compression** (core RTK / Caveman modes, opt-in): modes `off | lite | standard | aggressive | ultra | rtk` selected via the `x-omniroute-compression` request header or `[compression]` toml / `OMNIROUTE_COMPRESSION` env; `GET /v1/compression` shows the effective config; responses carry `x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>` plus bounded rule counts when applicable (see [docs/PARITY.md §6](docs/PARITY.md); stacked and advanced original plan layers remain unsupported)
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
cargo test          # 151 unit tests + 17 integration tests (full chain via mock upstream)
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
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | Data dir |

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
