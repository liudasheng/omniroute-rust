# 与原项目（TypeScript）的对照 / Parity

> English version: [docs/PARITY.md](../PARITY.md)。

来源：`diegosouzapw/OmniRoute` v3.8.x（ shallow clone 分析于 `open-sse/` 网关引擎 + `src/server` HTTP 层 + `bin/cli`）。原项目约 14,400 文件（含 Next.js 桌面/PWA/UI、352+ provider 专网 executor、MCP/A2A 等）。本仓库用 Rust 完全重写其**核心网关**，以下逐项对照。

## 1. 端点对照

| 端点 | 原版 | Rust 版 | 说明 |
|---|---|---|---|
| `GET/POST /v1/chat/completions` | ✅ | ✅ | SSE/JSON 双形态 |
| `POST /v1/messages` (+count_tokens) | ✅ | ✅ | claude 入/出；count_tokens 本地估算 |
| `POST /v1/responses` | ✅ | ✅ | openai-responses ⇄ chat 翻译 |
| `POST /v1/completions` | ✅ | ✅ | legacy prompt 形状投影 |
| `GET /v1/models`、`/v1` | ✅ | ✅ | `{object:"list", data:[{id,provider,contextLength,...}]}` |
| `POST /v1/embeddings|rerank|moderations` | ✅ | ✅ | 单 provider 透传（`provider/model` 前缀） |
| `POST /v1/images/*`, `/v1/audio/*`, `/v1/videos`, `/v1/ocr`, `/v1/batches` | ✅ | ❌ 404 | 原版依赖 IMAGE/AUDIO 专用 provider 注册表与专用 executor |
| `GET /healthz /readyz /livez /api/health(/ping)` | ✅ | ✅ | 同形状（`ok\n` / JSON） |
| 未知路径 | JSON 404 `unknown_route` | ✅ 相同 | 绝不返回 HTML |
| `/v1/combos(/test)`, `/v1/providers`, `/v1/quotas` | ✅ | ✅ | combos 只读 + test 干跑；providers/quotas 从内存熔断态聚合 |
| 管理面（dashboard JWT/session、CRUD） | ✅ | ❌ | Rust 版无数据库/无 dashboard |
| `错误形状` | `{error:{message,type,code}}` | ✅ 相同 | `errorConfig.ts#ERROR_TYPES` 映射一致 |

## 2. Provider 体系

- 原版 `providerRegistry.ts` ≈ 240 个 provider（OAuth/网页逆向 executor：antigravity/grok-web/deepseek-web/cursor/bedrock/vertex…）。
- Rust 版静态注册 **21 个高频 API-key provider**（anthropic/openai/gemini/glm/zai/kimi/deepseek/openrouter/groq/xai/mistral/together/fireworks/perplexity/minimax/siliconflow/dashscope/doubao/ollama/ollama-cloud/lmstudio），URL/认证头/URL 后缀（`?beta=true`）与原版 registry 一致：
  - anthropic：`https://api.anthropic.com/v1/messages?beta=true` + `x-api-key` + `anthropic-version: 2023-06-01`
  - gemini：`{base}/models/{m}:streamGenerateContent?alt=sse` + `x-goog-api-key`
  - kimi：`forceStream`（上游强制 SSE，网关折叠回 JSON）
- 动态兼容族 `openai-compatible-*` / `anthropic-compatible-*` / `anthropic-compatible-cc-*`（cc 家族使用 `/chat_completion?beta=true` + `anthropic-beta` 头）——与原版 `services/provider.ts` 行为一致。
- `parseModel`（`open-sse/services/model.ts`）：`provider/model`、别名解析、裸模型启发式、`[1m]`/`:1m` 扩展上下文后缀——全部实现，且 Rust 版在路由时用注册表校验前缀（比原版纯别名表更宽容）。

## 3. 路由/回退（combo）

| 项 | 原版 | Rust 版 |
|---|---|---|
| 策略全集 | 21（含 fusion/pipeline/quota-share/context-relay 等） | **10 个核心**：priority、round-robin、fill-first、weighted、random、least-used、p2c、cost-optimized、lkgp、auto；`failover`→priority、`usage`→least-used 别名一致；未知→priority |
| `MAX_COMBO_DEPTH` | 3（硬 10） | 3 / 硬 10 ✅ |
| `MAX_GLOBAL_ATTEMPTS` | 30（硬 200） | 30 / 硬 200 ✅ |
| `MAX_FALLBACK_WAIT_MS` | 5000 | 常量保留 ✅ |
| combo 安全超时 | 10 min | 10 min ✅ |
| 失败分类 | `checkFallbackError`/RateLimitReason | ✅ 同分类（auth/payment/not_found/rate_limit/server/network/client） |
| 400 处理 | param-validation 400 = 用户可修，立刻返回且不计入 provider 处罚 | ✅ 同语义 |
| 配额预检 `quotaPreflight` | per-provider quota fetcher | ❌ Rust 版以本地计数器近似（/v1/quotas） |

## 4. 熔断/冷却/退避

| 项 | 原版 | Rust 版 |
|---|---|---|
| `COOLDOWN_MS` | unauthorized/paymentRequired/notFound 2min、notFoundLocal 5s、serviceUnavailable 2s、transientInitial 5s | ✅ 相同 |
| `BACKOFF_CONFIG` | base 1s / max 2min / level 15 | ✅ 相同 |
| `BACKOFF_STEPS_MS` | [60s,120s,300s,600s,1200s] | ✅ 相同（模型级封禁递增） |
| 三 profile（oauth/apikey/local） | threshold 8/12/2、reset 60s/30s/15s、provider breaker 10@15min→5min / 15@30min→10min / 2@5min→1min | ✅ 相同 |
| env 覆盖 | `OMNIROUTE_CIRCUIT_BREAKER_*` | ✅ `*_THRESHOLD`/`*_RESET_MS` |
| `DEFAULT_API_LIMITS` | 60RPM/350ms/6 并发 | ✅ 相同（滑动窗口限流器） |

## 5. 流式翻译

- 原版 hub-and-spoke：任意格式→openai→目标格式，逐 chunk 有状态（`translateResponse`+initState）。
- Rust 版实现同一架构：`UpstreamSource`（上游→openai chunk：openai 透传 / anthropic 事件 / gemini SSE）+ `InboundSink`（openai chunk→入站格式：openai/claude/responses/completions）。
- 超时：connect 30s、请求 600s、首字节就绪 80s、流空闲 600s、心跳 15s——数值一致。
- 心跳帧：原版 `OPENAI_KEEPALIVE_FRAME`，Rust 版用 SSE 注释 `: keepalive`（对客户端等价且安全，记录差异）。
- `jsonBodyToSse`（provider 忽略 stream 时合成 SSE）✅；`forceStream` 折叠回 JSON ✅。

## 6. Token 压缩（RTK / Caveman）

原版的主动上下文压缩（`open-sse/services/compression/*`）现已实现，各引擎对照如下：

| 引擎（mode） | 原版 | Rust 版 |
|---|---|---|
| `lite`（RTK minimal 档） | `collapseWhitespace`（3+ 换行折叠、行尾空白）、`dedupSystemPrompt`（前 200 字符去重键）、`compressToolResults`（>2000 字符按词边界截断 + `...[truncated]`，回看窗口 80）、`removeRedundantContent`（相邻同角色同内容去重）、`replaceImageUrls`（非视觉模型 → `[image: format]`） | ✅ 五项技术全部实现，常量一致 |
| `standard`（Caveman） | 34 条规则短语压缩，强度 lite/full/ultra，角色上下文（all/user/assistant），`skipRules`、`minMessageLength=50`、`compressRoles=["user"]` 默认，保护块 tombstone（代码围栏/行内代码/URL/路径/错误行/堆栈帧），产物清理，句首重新大写，代码主导跳过（≥3 行且 ≥30% 代码行） | ✅ 完整规则表移植（34 条规则，模式/映射表/上下文/强度档位一致） |
| `aggressive` | 工具结果压缩器（fileContent 头20+尾5 / grepSearch 前30条+文件清单 / shellOutput 去 ANSI+后50行+连续去重 / json 首尾截取 / errorMessage 头10+尾3 帧）→ 渐进老化 → 规则摘要器 `[COMPRESSED:summary]` → 低于 5% 收益时降级到 caveman 再到 lite | ✅ 相同工具压缩器 + 抽取式摘要器（intents/files/errors/decision）+ 降级链；差异：渐进老化由摘要步骤近似 |
| `ultra` | Tier-A 启发式 token 剪枝：scoreToken（数字/URL/路径/Error:/围栏强制保留；极性词永不剪枝 #13454；停用词 0.1；≤2 字符 0.2；大写开头 0.8；≥6 字符 0.7），keepRate 0.5、minScore 0.3；SLM 档位可选（失败回启发式） | ✅ 相同评分表 + 剪枝；SLM 档位跳过（启发式即 Rust 版 ultra，与原版回退路径一致） |
| `rtk` | 按命令类型的完整过滤注册表（npm/make/docker/自定义 filter）、raw-output 指针、learn/verify | ⚠️ 简化版：去 ANSI、进度条行过滤、连续重复行去重、头尾 maxLines 截断（默认 200）、文档读取保护（#4559：无命令/错误标记的未知内容保留中段）；按命令的过滤注册表未实现 |
| `stacked` / `omniglyph` / codex-responses | 引擎管道 | ❌ 未实现 |

选择优先级（对照 `resolveBasePlan`）：总开关关闭 → 请求头
`x-omniroute-compression`（`off|default|lite|standard|aggressive|ultra|rtk`；未知值穿透不报错）
→ auto-trigger（估算 token ≥ `auto_trigger_tokens`）→ 配置的 `default_mode`。
压缩为**可选特性**（默认关闭，与原版一致），经 `[compression]` toml 表或
`OMNIROUTE_COMPRESSION` 环境变量开启；响应头
`x-omniroute-compression: <mode>; source=<src>; tokens=<orig>-><comp>; rules=<n>`；
`GET /v1/compression` 返回生效配置。token 估算为 chars/4（对照
`estimateCompressionTokens`）。

## 7. 配置/CLI

- 端口默认 **20128**（`--port` > `PORT` env > toml > 20128）——与原版一致（8317 只是原版 mitm 子系统端口）。
- 凭据文件沿用原版 `provider-credentials.json`（camelCase `apiKey`/`baseUrl` 兼容，扁平 schema 也接受）。
- `.env` 三层 first-wins 加载一致。
- CLI：原版 88 子命令（ Electron tray、MCP stdio、dashboard 管理、backup/update 等）；Rust 版实现核心 **7 个**：serve(默认)/status/stop/models/providers/combos/doctor，通用 `--output json|table/--api-key/--base-url/--port` 对应原版全局 flag。pidfile 停启逻辑同原版 `processSupervisor`。

## 8. Web 仪表盘 / PWA / Electron 桌面壳

原版仪表盘是 Next.js 应用（`src/app/(dashboard)`），外层用 Electron 壳包装
（`electron/main.js`：启动 server → 等待 `/healthz` → BrowserWindow + 系统
托盘 + 自动更新）。Rust 版用**网关自身内嵌的 Web 仪表盘**替代 Next.js 运行时
（无需 Node），并提供同构 Electron 包装：

| 面 | 原版 | Rust 版 |
|---|---|---|
| 仪表盘 UI | Next.js React 应用（设置/分析/日志页） | ✅ 网关直接内嵌单页应用：`/dashboard`（Overview 卡片、Providers 健康、Models 目录过滤、Combos、Compression 运行时编辑、Logs）；`/` 重定向到 `/dashboard` |
| PWA | 可安装仪表盘 | ✅ `manifest.webmanifest`（standalone）+ service worker（`/dashboard/sw.js`，network-first，API 响应永不缓存） |
| Electron 壳 | `electron/main.js` + 托盘 + 自动更新 + 远程登录 | ✅ `electron/`（spawn 网关 → `/healthz` 就绪 → 开窗；托盘 open/restart/quit；关闭隐藏到托盘；崩溃自动重启对照 ServerSupervisor；单实例锁）。差异：无自动更新 / 远程登录 / 凭据检查 |
| 请求历史 | SQLite 请求历史库 | ✅ 内存有界环形缓冲（最近 500 条）+ `GET /v1/logs` |
| 运行统计 | 仪表盘分析 | ✅ `GET /v1/stats`（运行时长、请求/失败计数、进程 RSS 经 /proc/self/status） |
| 压缩管理 UI | 压缩设置页 | ✅ 运行时可编辑压缩配置：`GET/POST /v1/compression`（带校验）+ 仪表盘编辑器；启动值仍来自 toml/env |
| 仪表盘鉴权 | dashboard JWT/session | 差异：使用网关 API key 鉴权（单用户网关）；已标注 |

## 9. 明确未重写（超出核心网关）

- 网页级逆向 executor（`open-sse/executors/*.ts` 数百个：chatgpt-web、claude-web、gemini-web、cursor、antigravity、grok-web、kiro…）
- MCP/A2A 协议服务、WebSocket 路由事件流
- 语义缓存/幂等缓存、服务端 tool-loop、streamRecovery、吞吐 watchdog（token 压缩已实现 — 见 §6）
- SQLite 存储 + STORAGE_ENCRYPTION_KEY 加密列（Rust 版配置来自文件+环境变量）

## 验证

- `cargo test`：66 单元（解析/翻译/策略/熔断/限流/SSE 解析）+ 9 集成（mock upstream 全链路：非流式、SSE、claude⇄openai 双向、failover、目录、鉴权、404）。
- 二进制冒烟：`omniroute serve` 起服后 `/healthz`、`/v1/models`、CLI status/doctor 验证。
