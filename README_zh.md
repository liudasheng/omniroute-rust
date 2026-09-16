# OmniRoute-Rust

**OmniRoute AI 网关的 Rust 完全重写**（参考 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x，原项目为 TypeScript/Next.js）。一个端点、多 provider 路由、配额感知自动回退、SSE 流式格式转换。

> 📖 English docs: [README.md](README.md) ｜ 性能对比：[docs/BENCHMARK.md](docs/BENCHMARK.md) ｜ 与原版逐项对照：[docs/PARITY.md](docs/PARITY.md)

## 性能摘要（vs 原版生产栈，同机同 mock upstream）

| 指标 | omniroute-rust | 原版 (TS) | 提升 |
|---|---|---|---|
| 空闲内存（进程树 RSS） | **7 MB** | ~850 MB | ~120× |
| 满负载内存（64 并发） | **~23 MB** | ~1.1–1.2 GB | ~49× |
| JSON 代理吞吐（64 并发） | **5,419 rps** | 32 rps | ~169× |
| 代理延迟 p50/p99（64 并发） | **7 / 14 ms** | 2,056 / 2,683 ms | — |
| SSE 流式吞吐（64 并发） | **1,131 rps** | 21 rps | ~53× |

方法与完整数据见 [docs/BENCHMARK.md](docs/BENCHMARK.md)。

## 功能

- **OpenAI 兼容 API**：`/v1/chat/completions`、`/v1/completions`（legacy）、`/v1/responses`、`/v1/models`、`/v1/embeddings|rerank|moderations`
- **Anthropic 原生 API**：`/v1/messages`、`/v1/messages/count_tokens`（claude 格式入/出，自动翻译到目标 provider）
- **健康检查**：`/healthz`、`/readyz`、`/livez`、`/api/health(/ping)`；未知路径返回 OpenAI 形状 JSON 404（绝不返回 HTML）
- **多 provider**：静态注册 anthropic/openai/gemini/glm/zai/kimi/deepseek/openrouter/groq/xai/mistral/together/fireworks/perplexity/minimax/siliconflow/dashscope/doubao/ollama/ollama-cloud/lmstudio 等 21 个，另支持动态 `openai-compatible-*` / `anthropic-compatible-*` / `anthropic-compatible-cc-*` 兼容族
- **Combo 路由策略**：priority(failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto；`MAX_GLOBAL_ATTEMPTS=30`、`MAX_COMBO_DEPTH=3`、combo 循环安全超时 10 分钟
- **熔断/冷却**：错误分级冷却（401/402/404→2min，5xx→2s，网络→5s）、指数退避（1s 起、2min 封顶、15 级）、provider 级断路器（oauth/apikey/local 三 profile）；错误限流采用**排队等待**（`RATE_LIMIT_MAX_WAIT_MS=30000`，与原版请求队列语义一致）
- **SSE 流式**：openai↔claude↔gemini 逐 chunk 有状态翻译；`forceStream` provider（kimi）非流式请求上游强制 SSE 时自动折叠回 JSON；心跳 keepalive（15s）
- **限流**：默认 60 RPM / 最小间隔 350ms / 6 并发（DEFAULT_API_LIMITS，仅作用于 api-key provider，本地 provider 豁免），均可通过环境变量覆盖

## 快速开始

```bash
# 构建
cargo build --release

# 配置 provider（环境变量或凭据文件）
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# 启动（默认端口 20128）
./target/release/omniroute serve

# 或自定义端口
./target/release/omniroute serve --port 3000

# 使用（OpenAI 客户端）
curl http://127.0.0.1:20128/v1/chat/completions \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'

# Anthropic 客户端
curl http://127.0.0.1:20128/v1/messages \
  -d '{"model":"anthropic/claude-sonnet-4-5","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
```

模型字符串支持三种写法：`provider/model`（如 `openai/gpt-4o`）、裸模型别名（`claude-sonnet-4-5` 自动映射 anthropic、`gpt-4o` 自动映射 openai）、`[1m]` 后缀标记 1M 上下文。

## 配置

配置目录默认 `~/.omniroute-rust`（可用 `$OMNIROUTE_DATA_DIR`/`$DATA_DIR` 覆盖），分层规则与原版 `loadEnvFile` 一致（先到先得）：

| 来源 | 内容 |
|---|---|
| `$DATA_DIR/provider-credentials.json` | provider 凭据（`{"providers": {"openai": {"apiKey": "...", "baseUrl": "..."}}}` 或扁平 `{"openai": {"api_key": "..."}}`），兼容原版 camelCase 字段 |
| `$DATA_DIR/omniroute.toml` | 端口/host、`[providers.*]` 调优、`[[combos]]` |
| `.env` 文件 | `$DATA_DIR/.env` → `~/.omniroute-rust/.env` → `cwd/.env`，`<ID>_API_KEY` 自动映射（如 `ZAI_API_KEY` → provider `zai`） |
| 进程环境 | `OMNIROUTE_API_KEY`（网关鉴权 key）、`PORT`、超时/熔断/限流覆盖 |

`omniroute.toml` 示例（完整示例见 [examples/omniroute.toml](examples/omniroute.toml)）：

```toml
[server]
port = 20128

[[combos]]
name = "coding"
strategy = "priority"          # failover 别名
providers = ["anthropic/claude-sonnet-4-5", "openai/gpt-4o", "groq=2"]  # "=2" 为 weighted 权重
models = ["claude-sonnet-4-5"] # 可选：命中这些模型时使用该 combo
```

## CLI

```bash
omniroute serve [--port N] [--host ADDR]   # 启动网关（默认子命令）
omniroute status --port 20128              # pid + /healthz 探活
omniroute stop                             # 通过 pidfile 停止
omniroute models [--base-url URL]          # GET /v1/models
omniroute providers                        # GET /v1/providers（连接健康度）
omniroute combos                           # 本地配置的 combo 列表
omniroute doctor                           # 配置/凭据体检
# 通用 flag：--output json|table、--api-key、--base-url
```

## 基准测试工具

```bash
cargo build --release --example mock_upstream --example loadgen
./target/release/examples/mock_upstream 9900 &            # 高性能 mock upstream
./target/release/examples/loadgen --url http://127.0.0.1:20128/v1/chat/completions \
  --mode json|sse|get --concurrency 64 --duration 6 --model "openai-compatible-bench/mock-model"
# 输出：requests / rps / p50 / p95 / p99 / errors
```

## 测试

```bash
cargo test          # 66 单元测试 + 9 集成测试（mock upstream 全链路）
cargo test --test integration
cargo clippy        # 0 警告
```

## 环境变量速查（与原版同名同默认）

| 变量 | 默认 | 说明 |
|---|---|---|
| `PORT` | 20128 | 主端口（`--port` 可覆盖） |
| `OMNIROUTE_API_KEY` | 无 | 设置后 /v1/* 需要 Bearer 鉴权 |
| `REQUEST_TIMEOUT_MS` | 600000 | 上游总超时 |
| `FETCH_CONNECT_TIMEOUT_MS` | 30000 | 连接超时 |
| `STREAM_IDLE_TIMEOUT_MS` | 600000 | 流空闲超时 |
| `STREAM_READINESS_TIMEOUT_MS` | 80000 | 首字节就绪超时 |
| `SSE_HEARTBEAT_INTERVAL_MS` | 15000 | 心跳 keepalive |
| `RATE_LIMIT_MAX_WAIT_MS` | 30000 | 限流排队最大等待（与原版同名） |
| `OMNIROUTE_REQUESTS_PER_MINUTE` | 60 | 限流 RPM |
| `OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS` | 350 | 限流最小间隔 |
| `OMNIROUTE_CONCURRENT_REQUESTS` | 6 | 单连接并发上限 |
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | 数据目录 |

## 许可

MIT（与原项目一致）。
