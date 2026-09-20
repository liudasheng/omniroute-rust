# OmniRoute-Rust

**OmniRoute AI 网关的 Rust 完全重写**（参考 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x，原项目为 TypeScript/Next.js）。一个端点、多 provider 路由、配额感知自动回退、SSE 流式格式转换。

> 📖 English docs: [README.md](README.md) ｜ 性能对比：[docs/zh/BENCHMARK.md](docs/zh/BENCHMARK.md) ｜ 与原版逐项对照：[docs/zh/PARITY.md](docs/zh/PARITY.md) ｜ 页面审计：[docs/zh/PAGES.md](docs/zh/PAGES.md)

## 性能摘要（vs 原版生产栈，同机同 mock upstream）

| 指标 | omniroute-rust | 原版 (TS) | 提升 |
|---|---|---|---|
| 空闲内存（进程树 RSS） | **9.4 MB** | 718 MB | 约 76× |
| 峰值内存（SSE 64 并发） | **31.0 MB** | 1.26 GB | 约 41× |
| `/healthz` 吞吐（64 并发） | **16,032 rps** | 373 rps | 约 43× |
| JSON 代理吞吐（64 并发） | **3,381 rps** | 32 rps | 约 106× |
| 代理延迟 p50/p99（64 并发） | **13 / 32 ms** | 1,792 / 3,084 ms | — |
| SSE 流式吞吐（64 并发） | **1,077 rps** | 32 rps | 约 34× |

两侧在所有场景错误数均为 0。本次为 2026-09-20 当前构建重跑，两侧限流参数完全一致
（100,000 RPM / 0 ms / 128 并发）并使用同一 master key；基准实例绝不占用线上端口。

方法与完整数据见 [docs/zh/BENCHMARK.md](docs/zh/BENCHMARK.md)。

## 功能

- **OpenAI 兼容 API**：`/v1/chat/completions`、`/v1/completions`（legacy）、`/v1/responses`、`/v1/models`、`/v1/embeddings|rerank|moderations`
- **多模态**：chat 内图片输入三种上游格式均支持（openai 系原样透传 `image_url`；claude 系互转 base64/url source；gemini 转 `inlineData`/`fileData`）；生成类端点单 provider 透传——`/v1/images/{generations,edits,upscale}`、`/v1/audio/{transcriptions,translations,speech}`（multipart 原样转发）、`/v1/videos`、`/v1/ocr`、`/v1/files`、`/v1/batches`（provider 经 `provider/model` 前缀、multipart `model` 字段或 `x-omniroute-provider` 头指定）
- **Anthropic 原生 API**：`/v1/messages`、`/v1/messages/count_tokens`（claude 格式入/出，自动翻译到目标 provider）
- **健康检查**：`/healthz`、`/readyz`、`/livez`、`/api/health(/ping)`；未知路径返回 OpenAI 形状 JSON 404（绝不返回 HTML）
- **多 provider**：静态注册 146 个（24 手写 + 122 从原版 registry 批量提取：openai/openai-responses/claude/gemini 四种纯 HTTP API-key 条目），另支持动态 `openai-compatible-*` / `anthropic-compatible-*` / `anthropic-compatible-cc-*` 兼容族（详见 [docs/zh/PARITY.md §2](docs/zh/PARITY.md)）
- **Combo 路由策略**：priority(failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto；`MAX_GLOBAL_ATTEMPTS=30`、`MAX_COMBO_DEPTH=3`、combo 循环安全超时 10 分钟
- **熔断/冷却**：错误分级冷却（401/402/404→2min，5xx→2s，网络→5s）、指数退避（1s 起、2min 封顶、15 级）、provider 级断路器（oauth/apikey/local 三 profile）；错误限流采用**排队等待**（`RATE_LIMIT_MAX_WAIT_MS=30000`，与原版请求队列语义一致）
- **SSE 流式**：openai↔claude↔gemini 逐 chunk 有状态翻译；`forceStream` provider（kimi）非流式请求上游强制 SSE 时自动折叠回 JSON；心跳 keepalive（15s）
- **Web 仪表盘 + PWA（对齐原版 UI）**：网关内嵌 `/dashboard` 单页应用，侧边栏与原版 **1:1**（`sections.ts`：10 个 section、8 个分组、94 个条目，顺序/图标/i18nKey/labelFallback 与 subtitleFallback 全部对齐，逐项确定性图标配色），共渲染 **76 个页面**：34 个真实数据页面（首页含快速入门/提供者拓扑/最近请求、Endpoints、API Manager、Providers、Combos、Provider Quota、Compression 及全部上下文引擎、Playground、Combos Studio、Translator、Batch、Traffic inspector、Usage、Combo Health、Utilization、Cache Health、Route tracing、Compression analytics、Provider Stats、Free tiers、Activity、Logs、Log export、Audit log、Health、Runtime、Resilience、Settings·General/Appearance/Sidebar/Resilience/Security）+ 42 个原版专属模块的诚实占位页（agent 舰队、gamification、MCP/A2A/插件运行时、成本核算等——明示差异、零假数据）；所有页面内容区与原版一致占满全宽（无 1120px 限宽）；自托管 Material Symbols 字体、原版深/浅色令牌、方格纸背景、可折叠分组、Ctrl+K 快速导航、66 套原版语言包（默认语言自动跟随浏览器，可顶栏手动切换、持久化，回退 en）；可安装为 PWA（页面审计见 [docs/zh/PAGES.md](docs/zh/PAGES.md)）
- **账号与多密钥管理**：首装默认密码 `CHANGEME`（与原版一致），加盐 SHA-256 存于 `$DATA_DIR/dashboard-auth.json`（权限 600、每次校验重读），`POST /v1/auth/login|logout|change-password`、`GET /v1/auth/me`、强制改密横幅，以及 `omniroute reset-password [--password X | --password-stdin]` 找回；客户端密钥 `GET/POST /v1/api-keys`、`PATCH/DELETE /v1/api-keys/{id}`（角色 default/admin、`sk-or-*`、仅创建时显示）；provider 连接 `GET/POST /v1/provider-connections`、`PATCH/DELETE /{id}`、`POST /{id}/test`（运行时注册进注册表）
- **运维面**：`GET /v1/stats/providers`（逐 provider 请求/错误/成功率/延迟/token + 实时冷却）、`GET /v1/combo-health`、带过滤的 `GET /v1/logs`、`GET /v1/logs/export?format=csv\|json`、`GET /v1/audit`（管理动作审计环）、`POST /v1/admin/service/restart\|stop`（适配 systemd）
- **Electron 桌面壳**（`electron/`）：启动网关、等待 `/healthz` 就绪后加载仪表盘；系统托盘（打开/重启/退出）、崩溃自动重启、关闭隐藏到托盘
- **Token 压缩**（RTK / Caveman 对等实现，可选开启）：`off | lite | standard | aggressive | ultra | rtk` 六种模式，经 `x-omniroute-compression` 请求头或 `[compression]` toml / `OMNIROUTE_COMPRESSION` 环境变量选择；`GET /v1/compression` 查看生效配置；响应头 `x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>` 返回压缩统计（详见 [docs/zh/PARITY.md §6](docs/zh/PARITY.md)）
- **推理策略**：兼容 OpenAI 格式的 `reasoning_effort`、`reasoning`、`max_completion_tokens`，以及 Claude thinking、Gemini thinking budget；可通过 `[thinking]` 或 `OMNIROUTE_THINKING_MODE` 使用 `passthrough`（默认）、`auto`/`adaptive`（移除客户端推理字段，交给 provider 默认值）和 `custom` 固定预算。上下文较小时建议 `auto` 搭配 `OMNIROUTE_COMPRESSION=lite`，并用 `OMNIROUTE_THINKING_BUDGET` 设置预算。

拥有独立模型目录的客户端不会从通用 `/v1/models` 发现结果自动推断可选推理等级，
需要在客户端 profile 中显式声明 `reasoningEfforts` 和对应的 wire 兼容格式：

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

需要推理等级的客户端应使用 `api: openai-responses`。只有明确使用
`/v1/chat/completions` 时，才配置 `supportsReasoningEffort: true` 与
`thinkingFormat: openai`。
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

模型字符串支持三种写法：`provider/model`（如 `openai/gpt-4o`）、裸模型别名（`claude-sonnet-4-5` 自动映射 anthropic、`gpt-4o` 自动映射 openai）、`[1m]` 后缀标记 1M 上下文。`/v1/models` 会按具体模型返回客户端识别的 `contextWindow`/`maxTokens`（同时保留 `contextLength`）；combo 对外取候选中最大的窗口，并在实际路由时过滤过小候选，因此全是 1M 模型的 coding 组合会正确暴露 1M，不再固定为旧的 128K。

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
cargo test          # 131 单元测试 + 16 集成测试（mock upstream 全链路）
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
| `OMNIROUTE_THINKING_MODE` | passthrough | 推理策略：passthrough、auto、adaptive、custom |
| `OMNIROUTE_THINKING_BUDGET` | 无 | custom 模式的固定推理预算 |
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | 数据目录 |

Provider connection 可通过 `POST /v1/provider-connections/{id}/sync-models` 刷新
上游 `/models`。网关会持久化上游返回的非敏感上下文、输出上限、输入模态、
vision/PDF 与推理等级元数据；后续目录请求优先使用同步值，缺失字段才回退到模型规则。
连接的 `api_type` 决定出站协议；上游需要接收 `/v1/responses` 时，provider connection
也必须设置为 `openai-responses`。
OpenRouter 仍使用 OpenAI Chat 协议；支持推理的 OpenRouter 模型通过
`thinkingFormat: openrouter` 兼容映射传递推理等级，不应把整个 provider 切成 Responses。

## 许可

MIT（与原项目一致）。
