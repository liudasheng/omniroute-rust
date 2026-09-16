# OmniRoute-Rust

**OmniRoute AI 网关的 Rust 完全重写**（参考 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute)，原项目为 TypeScript/Next.js）。一个端点、多 provider 路由、配额感知自动回退、SSE 流式转换。

> 实现范围与原项目的对照见 [`docs/PARITY.md`](docs/PARITY.md)。

## 功能

- **OpenAI 兼容 API**：`/v1/chat/completions`、`/v1/completions`（legacy）、`/v1/responses`、`/v1/models`、`/v1/embeddings|rerank|moderations`
- **Anthropic 原生 API**：`/v1/messages`、`/v1/messages/count_tokens`（claude 格式入/出，自动翻译到目标 provider）
- **健康检查**：`/healthz`、`/readyz`、`/livez`、`/api/health(/ping)`；未知路径返回 OpenAI 形状 JSON 404（绝不返回 HTML）
- **多 provider**：静态注册 anthropic/openai/gemini/glm/zai/kimi/deepseek/openrouter/groq/xai/mistral/together/fireworks/perplexity/minimax/siliconflow/dashscope/doubao/ollama/ollama-cloud/lmstudio 等，另支持动态 `openai-compatible-*` / `anthropic-compatible-*` 兼容族
- **Combo 路由策略**：priority(failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto；`MAX_GLOBAL_ATTEMPTS=30`、`MAX_COMBO_DEPTH=3`、combo 循环安全超时 10 分钟
- **熔断/冷却**：错误分级冷却（401/402/404→2min，5xx→2s，网络→5s）、指数退避（1s 起，2min 封顶，15 级）、provider 级断路器（oauth/apikey/local 三 profile）
- **SSE 流式**：openai↔claude↔gemini 逐 chunk 有状态翻译；`forceStream` provider（kimi）非流式请求上游强制 SSE 时自动折叠回 JSON；心跳 keepalive（15s）
- **限流**：60 RPM / 最小请求间隔 350ms / 6 并发（DEFAULT_API_LIMITS）

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
  -H "Authorization: Bearer sk-master" \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"hi"}]}'

# Anthropic 客户端
curl http://127.0.0.1:20128/v1/messages \
  -d '{"model":"anthropic/claude-sonnet-4-5","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
```

## 配置

配置目录默认 `~/.omniroute-rust`（可用 `$OMNIROUTE_DATA_DIR`/`$DATA_DIR` 覆盖），分层规则与原版 `loadEnvFile` 一致（先到先得）：

| 来源 | 内容 |
|---|---|
| `$DATA_DIR/provider-credentials.json` | provider 凭据（`{"providers": {"openai": {"apiKey": "...", "baseUrl": "..."}}}` 或扁平 `{"openai": {"api_key": "..."}}`），兼容原版 camelCase 字段 |
| `$DATA_DIR/omniroute.toml` | 端口/host、[providers.*] 调优、[[combos]] |
| `.env` 文件 | `$DATA_DIR/.env` → `~/.omniroute-rust/.env` → `cwd/.env`，`<ID>_API_KEY` 自动映射（如 `ZAI_API_KEY` → provider `zai`） |
| 进程环境 | `OMNIROUTE_API_KEY`（网关鉴权 key）、`PORT`、超时/熔断覆盖（`REQUEST_TIMEOUT_MS`、`OMNIROUTE_CIRCUIT_BREAKER_*` 等） |

`omniroute.toml` 示例（combo 多 provider 回退）：

```toml
[server]
port = 20128

[[combos]]
name = "coding"
strategy = "priority"          # failover 别名
providers = ["anthropic/claude-sonnet-4-5", "openai/gpt-4o", "groq=2"]  # "=2" 为权重
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

## 测试

```bash
cargo test          # 66 单元测试 + 9 集成测试（mock upstream 全链路）
cargo test --test integration
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
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | 数据目录 |

## 许可

MIT（与原项目一致）。
