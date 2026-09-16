# OmniRoute-Rust

**A full Rust rewrite of the OmniRoute AI gateway** (reference: [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x, originally TypeScript/Next.js). One endpoint, multi-provider routing, quota-aware automatic fallback, SSE streaming format translation.

> 📖 中文文档：[README_zh.md](README_zh.md) ｜ Benchmark details: [docs/BENCHMARK.md](docs/BENCHMARK.md) ｜ Feature parity: [docs/PARITY.md](docs/PARITY.md)

## Benchmark summary (vs the original production stack, same machine + same mock upstream)

| Metric | omniroute-rust | Original (TS) | Improvement |
|---|---|---|---|
| Idle memory (process-tree RSS) | **7 MB** | ~850 MB | ~120× |
| Memory under load (64 concurrent) | **~23 MB** | ~1.1–1.2 GB | ~49× |
| JSON proxy throughput (64 concurrent) | **5,419 rps** | 32 rps | ~169× |
| Proxy latency p50/p99 (64 concurrent) | **7 / 14 ms** | 2,056 / 2,683 ms | — |
| SSE streaming throughput (64 concurrent) | **1,131 rps** | 21 rps | ~53× |

Methodology and full data: [docs/BENCHMARK.md](docs/BENCHMARK.md).

## Features

- **OpenAI-compatible API**: `/v1/chat/completions`, `/v1/completions` (legacy), `/v1/responses`, `/v1/models`, `/v1/embeddings|rerank|moderations`
- **Anthropic-native API**: `/v1/messages`, `/v1/messages/count_tokens` (claude wire format in/out, auto-translated to the target provider)
- **Health endpoints**: `/healthz`, `/readyz`, `/livez`, `/api/health(/ping)`; unknown paths return an OpenAI-shaped JSON 404 (never HTML)
- **Multi-provider**: 21 built-in providers (anthropic/openai/gemini/glm/zai/kimi/deepseek/openrouter/groq/xai/mistral/together/fireworks/perplexity/minimax/siliconflow/dashscope/doubao/ollama/ollama-cloud/lmstudio) plus dynamic `openai-compatible-*` / `anthropic-compatible-*` / `anthropic-compatible-cc-*` families
- **Combo routing strategies**: priority (failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto; `MAX_GLOBAL_ATTEMPTS=30`, `MAX_COMBO_DEPTH=3`, 10-minute combo loop safety timeout
- **Circuit health**: error-class cooldowns (401/402/404→2min, 5xx→2s, network→5s), exponential backoff (1s base, 2min cap, 15 levels), provider-level breakers (oauth/apikey/local profiles); rate limiting **queues and waits** (`RATE_LIMIT_MAX_WAIT_MS=30000`, matching the original's request-queue semantics)
- **SSE streaming**: stateful chunk-by-chunk openai↔claude↔gemini translation; `forceStream` providers (kimi) fold upstream SSE back to JSON when the client asked for non-streaming; keepalive heartbeats (15s)
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
