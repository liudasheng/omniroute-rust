# OmniRoute-Rust

**A full Rust rewrite of the OmniRoute AI gateway** (reference: [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x, originally TypeScript/Next.js). One endpoint, multi-provider routing, quota-aware automatic fallback, SSE streaming format translation.

> 📖 中文文档：[README_zh.md](README_zh.md) ｜ Docs: [docs/BENCHMARK.md](docs/BENCHMARK.md) · [docs/PARITY.md](docs/PARITY.md) · [docs/ORIGINAL-SPEC.md](docs/ORIGINAL-SPEC.md) ｜ 中文文档见 [README_zh.md](README_zh.md) 与 [docs/zh/](docs/zh/)。

## Benchmark summary (vs the original production stack, same machine + same mock upstream)

| Metric | omniroute-rust | Original (TS) | Improvement |
|---|---|---|---|
| Idle memory (process-tree RSS) | **9.1 MB** | 754 MB | ~83× |
| Peak memory (SSE, 64 concurrent) | **27.8 MB** | 1.32 GB | ~48× |
| `/healthz` throughput (64 concurrent) | **17,229 rps** | 819 rps | ~21× |
| JSON proxy throughput (64 concurrent) | **6,304 rps** | 43 rps | ~148× |
| Proxy latency p50/p99 (64 concurrent) | **7 / 14 ms** | 1,308 / 1,664 ms | — |
| SSE streaming throughput (64 concurrent) | **1,195 rps** | 43 rps | ~28× |

Zero errors on both sides in every scenario. Re-run on the current build with
identical limits (100,000 RPM / 0 ms / 128 concurrent) and the same master key
on both gateways; the bench instance never shares the live service port.

Methodology and full data: [docs/BENCHMARK.md](docs/BENCHMARK.md).

## Features

- **OpenAI-compatible API**: `/v1/chat/completions`, `/v1/completions` (legacy), `/v1/responses`, `/v1/models`, `/v1/embeddings|rerank|moderations`
- **Multimodal**: image input in chat across all three upstream formats (`image_url` verbatim for openai-format, claude base64/url blocks, gemini inlineData/fileData); generation surfaces via single-provider passthrough — `/v1/images/{generations,edits,upscale}`, `/v1/audio/{transcriptions,translations,speech}` (multipart raw passthrough), `/v1/videos`, `/v1/ocr`, `/v1/files`, `/v1/batches` (provider via `provider/model` prefix, multipart `model` field, or `x-omniroute-provider` header)
- **Anthropic-native API**: `/v1/messages`, `/v1/messages/count_tokens` (claude wire format in/out, auto-translated to the target provider)
- **Health endpoints**: `/healthz`, `/readyz`, `/livez`, `/api/health(/ping)`; unknown paths return an OpenAI-shaped JSON 404 (never HTML)
- **Multi-provider**: 21 built-in providers (anthropic/openai/gemini/glm/zai/kimi/deepseek/openrouter/groq/xai/mistral/together/fireworks/perplexity/minimax/siliconflow/dashscope/doubao/ollama/ollama-cloud/lmstudio) plus dynamic `openai-compatible-*` / `anthropic-compatible-*` / `anthropic-compatible-cc-*` families
- **Combo routing strategies**: priority (failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto; `MAX_GLOBAL_ATTEMPTS=30`, `MAX_COMBO_DEPTH=3`, 10-minute combo loop safety timeout
- **Circuit health**: error-class cooldowns (401/402/404→2min, 5xx→2s, network→5s), exponential backoff (1s base, 2min cap, 15 levels), provider-level breakers (oauth/apikey/local profiles); rate limiting **queues and waits** (`RATE_LIMIT_MAX_WAIT_MS=30000`, matching the original's request-queue semantics)
- **SSE streaming**: stateful chunk-by-chunk openai↔claude↔gemini translation; `forceStream` providers (kimi) fold upstream SSE back to JSON when the client asked for non-streaming; keepalive heartbeats (15s)
- **Web dashboard + PWA (upstream-parity UI)**: embedded single-page dashboard at `/dashboard` mirroring the original's sidebar (`sections.ts`), `DashboardLayout`, `Sidebar` and `LanguageSelector`: 25 data-backed pages (Home with quick-start + provider topology + recent requests, Endpoints, API Manager, Providers, Combos, Provider Quota, Compression + Caveman/RTK/Ultra/Aggressive/Lite, Playground, Translator, Batch, Traffic inspector, Usage, Combo Health, Utilization, Compression analytics, Provider Stats, Activity, Logs, Log export, Audit log, Health, Runtime, Resilience, Settings·General/Appearance/Sidebar/Resilience/Security, Docs), self-hosted Material Symbols font, upstream dark **and** light colour tokens, graph-paper wallpaper, collapsible sections, deterministic per-item icon accents, Ctrl+K quick navigation, 66 upstream locale packs; installable as a PWA
- **Account + multi-key management**: first-install admin password `CHANGEME` (upstream default) persisted as a salted SHA-256 record in `$DATA_DIR/dashboard-auth.json` (mode 600, re-read per check), `POST /v1/auth/login|logout|change-password`, `GET /v1/auth/me`, forced-change banner, and `omniroute reset-password [--password X | --password-stdin]` recovery; client keys via `GET/POST /v1/api-keys`, `PATCH/DELETE /v1/api-keys/{id}` (roles default/admin, `sk-or-*`, secret shown once); provider connections via `GET/POST /v1/provider-connections`, `PATCH/DELETE /{id}` and `POST /{id}/test` with runtime registry registration
- **Operations surface**: `GET /v1/stats/providers` (per-provider requests/errors/success/latency/tokens + live cooldown), `GET /v1/combo-health`, filtered `GET /v1/logs` (`provider`/`model`/`status`/`class`/`errors`/`stream`), `GET /v1/logs/export?format=csv\|json`, `GET /v1/audit` (management-action ring), `POST /v1/admin/service/restart\|stop` (systemd-friendly)
- **Electron desktop shell** (`electron/`): spawns the gateway, waits for `/healthz`, loads the dashboard; system tray (open/restart/quit), crash-restart, close-hides-to-tray
- **Token compression** (RTK / Caveman parity, opt-in): modes `off | lite | standard | aggressive | ultra | rtk` selected via the `x-omniroute-compression` request header or `[compression]` toml / `OMNIROUTE_COMPRESSION` env; `GET /v1/compression` shows the effective config; responses carry `x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>` meta (see [docs/PARITY.md §6](docs/PARITY.md))
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

Model strings support three shapes: `provider/model` (e.g. `openai/gpt-4o`), bare model aliases (`claude-sonnet-4-5` resolves to anthropic, `gpt-4o` to openai), and the `[1m]` suffix marking 1M context.

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
cargo test          # 66 unit tests + 9 integration tests (full chain via mock upstream)
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
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | Data dir |

## License

MIT (same as the original project).
