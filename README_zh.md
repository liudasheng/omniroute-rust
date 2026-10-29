# OmniRoute-Rust

**OmniRoute AI 网关的 Rust 完全重写**（参考 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.x，原项目为 TypeScript/Next.js）。一个端点、多 provider 路由、配额感知自动回退、SSE 流式格式转换。

> 📖 English docs: [README.md](README.md) ｜ 性能对比：[docs/zh/BENCHMARK.md](docs/zh/BENCHMARK.md) ｜ 与原版逐项对照：[docs/zh/PARITY.md](docs/zh/PARITY.md) ｜ 页面审计：[docs/zh/PAGES.md](docs/zh/PAGES.md) ｜ 压缩方案：[docs/LRC.md](docs/LRC.md)

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
- **Combo 路由策略**：priority(failover)/round-robin/fill-first/weighted/random/least-used/p2c/cost-optimized/lkgp/auto；`MAX_GLOBAL_ATTEMPTS=30`、`MAX_COMBO_DEPTH=3`、combo 循环安全超时 10 分钟。组合的**步骤**可为 `provider`、`provider/model`（均可加 `=weight`）、**另一组合名**，或 **`auto/*` 虚拟组合**（例：`coding-free → auto/free`）：嵌套步骤在请求时就地展开，受 `MAX_COMBO_DEPTH` 限深并有环检测；`POST /v1/combos/managed` 回显展开结果 `resolved`，仪表盘组合编辑器的步骤选择器亦列出自动虚拟组合与已有组合
- **熔断/冷却**：错误分级冷却（401/402/404→2min，5xx→2s，网络→5s）、指数退避（1s 起、2min 封顶、15 级）、provider 级断路器（oauth/apikey/local 三 profile）；错误限流采用**排队等待**（`RATE_LIMIT_MAX_WAIT_MS=30000`，与原版请求队列语义一致）
- **SSE 流式**：openai↔claude↔gemini 逐 chunk 有状态翻译；`forceStream` provider（kimi）非流式请求上游强制 SSE 时自动折叠回 JSON；心跳 keepalive（15s）
- **Web 仪表盘 + PWA（对齐原版 UI）**：网关内嵌 `/dashboard` 单页应用，侧边栏与原版 **1:1**（`sections.ts`：10 个 section、8 个分组、94 个条目，顺序/图标/i18nKey/labelFallback 与 subtitleFallback 全部对齐，逐项确定性图标配色），共渲染 **76 个页面**：34 个真实数据页面（首页含快速入门/提供者拓扑/最近请求、Endpoints、API Manager、Providers、Combos、Provider Quota、Compression 及全部上下文引擎、Playground、Combos Studio、Translator、Batch、Traffic inspector、Usage、Combo Health、Utilization、Cache Health、Route tracing、Compression analytics、Provider Stats、Free tiers、Activity、Logs、Log export、Audit log、Health、Runtime、Resilience、Settings·General/Appearance/Sidebar/Resilience/Security）+ 42 个原版专属模块的诚实占位页（agent 舰队、gamification、MCP/A2A/插件运行时、成本核算等——明示差异、零假数据）；所有页面内容区与原版一致占满全宽（无 1120px 限宽）；自托管 Material Symbols 字体、原版深/浅色令牌、方格纸背景、可折叠分组、Ctrl+K 快速导航、66 套原版语言包（默认语言自动跟随浏览器，可顶栏手动切换、持久化，回退 en）；可安装为 PWA（页面审计见 [docs/zh/PAGES.md](docs/zh/PAGES.md)）
- **账号与多密钥管理**：首装默认密码 `CHANGEME`（与原版一致），加盐 SHA-256 存于 `$DATA_DIR/dashboard-auth.json`（权限 600、每次校验重读），`POST /v1/auth/login|logout|change-password`、`GET /v1/auth/me`、强制改密横幅，以及 `omniroute reset-password [--password X | --password-stdin]` 找回；客户端密钥 `GET/POST /v1/api-keys`、`PATCH/DELETE /v1/api-keys/{id}`（角色 default/admin、`sk-or-*`、仅创建时显示）；provider 连接 `GET/POST /v1/provider-connections`、`PATCH/DELETE /{id}`、`POST /{id}/test`（运行时注册进注册表）
- **运维面**：`GET /v1/stats/providers`（逐 provider 请求/错误/成功率/延迟/token + 实时冷却）、`GET /v1/combo-health`、带过滤的 `GET /v1/logs`、`GET /v1/logs/export?format=csv\|json`、`GET /v1/audit`（管理动作审计环）、`POST /v1/admin/service/restart\|stop`（适配 systemd）
- **Electron 桌面壳**（`electron/`）：启动网关、等待 `/healthz` 就绪后加载仪表盘；系统托盘（打开/重启/退出）、崩溃自动重启、关闭隐藏到托盘
- **Token 压缩**（核心 RTK / Caveman 模式，可选开启）：`off | lite | standard | aggressive | ultra | rtk | lrc` 七种模式，经 `x-omniroute-compression` 请求头或 `[compression]` toml / `OMNIROUTE_COMPRESSION` 环境变量选择；`GET /v1/compression` 查看生效配置；响应头 `x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>` 返回压缩统计，并在适用时附带有界规则计数（详见 [docs/zh/PARITY.md §6](docs/zh/PARITY.md)；stacked 和原版高级计划层仍未实现）
- **LRC 分层可逆压缩**（`lrc`，可选开启）：按桶分流、fail-open 的流水线（保护层 → 无损层 → 有损可逆层 → 门禁层 → 账本层），专攻文本级引擎碰不到的那几块——历史 reasoning、`tool_calls[].function.arguments`、工具结果与 `tools` JSON schema；保护片段还原率 100%、`tool_call`↔`tool_result` 配对不变；档位 `conservative | balanced | aggressive | extreme`；设计见 [docs/LRC.md](docs/LRC.md)，实测见 [docs/reports/lrc-benchmark-2026-10-22.md](docs/reports/lrc-benchmark-2026-10-22.md)
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
- **OpenCode Zen 免费档（零配置、自动发现）**：`opencode` / `opencode-zen` 两个 provider 用字面量 `public` 凭据即可路由，匿名免费档**免注册、免充值**。名册 = 实时目录（`GET https://opencode.ai/zen/v1/models`，300s TTL，缓存于 `$DATA_DIR/opencode-free.json`）∩ models.dev 零成本判定，再减去本地黑名单与**本网关实测学到**的封锁（区域不可用／已下架／限流）。免费档凭据按请求选择：`auto`（默认）在 `opencode` / `opencode-zen` 的托管账号间轮询、各用其自己的 key，账号池为空/耗尽/冷却时退回字面量 `public`（`key` 模式亦允许 env/config key，`public` 模式强制匿名；`OMNIROUTE_OPENCODE_FREE_USE_KEY` 保留为兼容别名）。带 key 调 Zen 免费档会被 workspace 的「Allow free endpoints」隐私开关拦成 400，被拒后本通道改停 `public` 6 小时。本地 UTC 日预算（`$DATA_DIR/opencode-free-budget.json`，每账号与 `public` 各 200 次，重启不丢）会直接跳过已耗尽的凭据而不发上游请求；收到日额度 `429 FreeUsageLimitError` 时记录实测上限（`learned_limit`，取保守值，跨 UTC 日保留，7 天未见新 429 后每周期回升 20%），把该凭据冷却到 `Retry-After`（短 `Retry-After` 按突发限流仅短冷却）；若同模型的两个凭据在 60 秒内先后收到日额度 429，则判定预算按出口（IP）计，整个 UTC 窗口停止逐账号试探。`GET /v1/opencode-free` 暴露模式、学到的作用域与逐凭据计数。对话会话号持久化到 `$DATA_DIR/opencode-free-sessions.json`（只存 seed 的摘要，不存用户原文），重启后继续复用上游已接受的 `ses_*`，不再为每个对话新造会话；同一文件保存最近 8 个上游已接受（warm）会话与会话准入状态。Dashboard 的 probe 每个 provider 共用一个会话（而非每模型一个）。`OMNIROUTE_OPENCODE_FREE_SESSION_FALLBACK=true` 时，新会话被长 `Retry-After` 拒绝（会话准入额度，而非请求额度）会用 warm 会话原 body 重试一次；默认关闭，待上游行为验证后再开。与开关无关：日额度类 429 若发生在已接受请求数少于 `OMNIROUTE_OPENCODE_FREE_LEARNED_MIN_SAMPLES`（默认 10）时，按**疑似会话准入拒绝**处理，只做 `OMNIROUTE_OPENCODE_FREE_SHORT_QUOTA_COOLDOWN_SECS`（默认 60 秒）短冷却，不写 learned 上限、不整窗口停车、不触发 egress 学习；同一 UTC 窗口内第 3 次此类低用量 429 才升级为正常日额度处理（避免真正的小额度账号被永久放过）。`GET /v1/opencode-free` 暴露 `sessionAdmissionSuspected` 与 `shortQuotaRefusals`。上游的「agent 形状」门禁自动满足：强制流式、补齐 `bash`+`read` 门禁工具（客户端未带工具时并置 `tool_choice: "none"`）、注入会话稳定的规范 `ses_*` 会话号。`/v1/models` 对免费模型标 `"free": true`；`GET /v1/opencode-free` 汇报名册、判定来源与全部已学到的封锁；`POST /v1/opencode-free/refresh` 立即重读目录。显式 `opencode/...` 调用不再向其他 provider 扇出——失败即刻如实返回，不再串行等待无关 provider（含 LM Studio/Ollama 默认端口）的连接超时
- **OpenCode 订阅额度（自动发现）**：后台轮询 `GET https://opencode.ai/zen/go/v1/usage`（Bearer key），解析 rolling/weekly/monthly 三窗口 `{status, percent, resetsAt}`。读到「已耗尽」且读数新鲜时，该 provider 在组合链中被**软跳过**（原因写入错误体），半开探测仍保留恢复通道；读数过期则一律不拦。`GET /v1/opencode-usage` 查看读数，`POST /v1/opencode-usage/refresh` 立即重读，`/v1/quotas` 同时内嵌两条通道状态
- **零成本路由策略**：`auto/free`（别名 `auto/best-free`，此前实为「每 provider 取首个模型」并不筛免费）只解析到自动发现的免费名册——先 OpenCode Zen 匿名线，再其他 provider 的免费 id——故新发布的免费模型无需改组合即入链。链序为**实时综合评分**，每次解析重算，取自已发现名册：**新鲜度** 0.40（该 id 首次出现在本机目录距今多久；上游不公布发布日期）、**上下文** 0.30（以 1M 归一）、**多模态输入** 0.20（图像 0.5 + PDF 0.5）、**近期可用性** 0.10（24h 内线性衰减）。三项公开准则占主导，故「刚成功过」绝不压过「更新且更大」者，仅在接近平局时向已知可用者倾斜；总分相同按字典序，保证重复解析稳定。`GET /v1/opencode-free` 给出 `score` 明细（`newness`/`context`/`modalities`/`verified`/`total`/`contextWindow`）与 `scoreReason`、`firstSeenMs`；首次见到的时间戳落盘，重启不会让整份名册变「新」。目录本身每 **12 小时**重读一次（`OMNIROUTE_OPENCODE_FREE_TTL_SECONDS`，默认 43200）——可用性按请求即时学习、评分在读取时重算，缩得更短并无收益。


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
cargo test          # 219 单元测试 + 28 集成测试（mock upstream 全链路）
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
| `OMNIROUTE_MODEL_CATALOG_AUTOUPDATE` | true | 模型能力目录后台自动刷新 |
| `OMNIROUTE_MODEL_CATALOG_TTL_HOURS` | 24 | 快照超过该时长才刷新 |
| `OMNIROUTE_MODEL_CATALOG_URL` | https://models.dev/api.json | 上游能力目录地址 |
| `OMNIROUTE_OPENCODE_FREE_AUTOUPDATE` | true | 从实时目录自动发现 OpenCode Zen 免费档名册 |
| `OMNIROUTE_OPENCODE_FREE_TTL_SECONDS` | 43200 | 实时目录刷新周期（12 小时） |
| `OMNIROUTE_OPENCODE_FREE_URL` | https://opencode.ai/zen | 免费档发现所用的 Zen 基址 |
| `OMNIROUTE_OPENCODE_FREE_CREDENTIAL_MODE` | auto | 免费档凭据模式：`auto` 轮询托管账号 key、池空时退回 `public`；`key` 另允许 env/config key；`public` 强制匿名 |
| `OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PER_ACCOUNT` | 200 | 每个托管账号的本地 UTC 日请求上限（每窗口再被实测上限收紧） |
| `OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PUBLIC` | 200 | 匿名 `public` 凭据的本地 UTC 日请求上限 |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT` | true | 从日额度 `429 FreeUsageLimitError` 学习真实上限（`learned = min(learned, used_at_429)`，跨日保留） |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT_TTL_DAYS` | 7 | 连续多日未再遇 429 时按周期把实测上限回升 20%（0 关闭） |
| `OMNIROUTE_OPENCODE_FREE_MAX_IN_FLIGHT_PER_ACCOUNT` | 2 | 单凭据在途请求上限，超出则换账号或短暂排队（0 不限） |
| `OMNIROUTE_OPENCODE_FREE_SESSION_FALLBACK` | false | 新会话被长 `Retry-After` 拒绝时，用 warm（已接受）会话原 body 重试一次，避免把会话准入误判为额度耗尽（待实测验证后打开） |
| `OMNIROUTE_OPENCODE_FREE_SESSION_MEMO_FILE` | opencode-free-sessions.json | 数据目录内保存会话 memo、warm 会话集合与准入状态的文件名 |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_MIN_SAMPLES` | 10 | 已接受请求数少于该值时出现的日额度类 429 视为疑似会话准入（仅短冷却、不学 learned、不整窗口停车），同窗口累计 3 次后升级为正常日额度处理；0=旧行为 |
| `OMNIROUTE_OPENCODE_FREE_SHORT_QUOTA_COOLDOWN_SECS` | 60 | 单次疑似低用量拒绝的冷却时长（有 `Retry-After` 时取更短者） |
| `OMNIROUTE_OPENCODE_FREE_USE_KEY` | — | 兼容别名：`true` 等同 `CREDENTIAL_MODE=key`，`false` 等同 `public` |
| `OMNIROUTE_OPENCODE_QUOTA_AUTOUPDATE` | true | 自动发现 OpenCode Go 订阅额度（rolling/weekly/monthly） |
| `OMNIROUTE_OPENCODE_QUOTA_TTL_SECONDS` | 300 | 订阅额度刷新周期 |
| `OMNIROUTE_PROVIDER_SYNC_AUTOUPDATE` | true | 从各上游 `/models` 自动刷新提供者连接模型 |
| `OMNIROUTE_PROVIDER_SYNC_TTL_SECONDS` | 14400 | 提供者模型同步周期（秒，默认 4 小时；可用小于 1 小时的值做冒烟测试） |
| `OMNIROUTE_PROVIDER_SYNC_TTL_HOURS` | — | 秒级变量未设置时的粗粒度回退周期 |
| `OMNIROUTE_DATA_DIR`/`DATA_DIR` | ~/.omniroute-rust | 数据目录 |

模型能力（上下文窗口、最大输出、输入模态、是否支持推理）来自运行时目录快照，
缓存于 `$DATA_DIR/model-catalog.json` 并自动从 `models.dev` 刷新——因为多数
provider 的 `/models` 只返回模型 id。`GET /v1/model-catalog` 查看缓存状态，
`POST /v1/model-catalog/refresh` 立即强制刷新。

Provider connection 可通过 `POST /v1/provider-connections/{id}/sync-models` 刷新
上游 `/models`，并通过 `DELETE /v1/provider-connections/{id}/models/{model}`
删除手动添加的模型。网关会持久化上游返回的非敏感上下文、输出上限、输入模态、
vision/PDF 与推理等级元数据；后续目录请求优先使用同步值，缺失字段才回退到模型规则。
同步模型与内置模型重复时只展示同步条目。
连接的 `api_type` 决定出站协议；上游需要接收 `/v1/responses` 时，provider connection
也必须设置为 `openai-responses`。
OpenRouter 仍使用 OpenAI Chat 协议；支持推理的 OpenRouter 模型通过
`thinkingFormat: openrouter` 兼容映射传递推理等级，不应把整个 provider 切成 Responses。

## 许可

MIT（与原项目一致）。
