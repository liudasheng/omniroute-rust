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
| `GET /v1/models`、`/v1` | ✅ | ✅ | `{object:"list", data:[{id,provider,contextWindow,maxTokens,contextLength,...}]}` |
| `POST /v1/embeddings|rerank|moderations` | ✅ | ✅ | 单 provider 透传（`provider/model` 前缀） |
| `POST /v1/images/*`, `/v1/audio/*`, `/v1/videos`, `/v1/ocr`, `/v1/batches` | ✅ | ✅ | 单 provider 原样透传；chat 图片输入可进入 combo，combo 目录暴露 `supportsVision`/`modalities` |
| `GET /healthz /readyz /livez /api/health(/ping)` | ✅ | ✅ | 同形状（`ok\n` / JSON） |
| 未知路径 | JSON 404 `unknown_route` | ✅ 相同 | 绝不返回 HTML |
| `/v1/combos(/test)`, `/v1/providers`, `/v1/quotas` | ✅ | ✅ | combos 只读 + test 干跑；providers/quotas 从内存熔断态聚合 |
| 管理面（dashboard 会话、CRUD、分析） | ✅ | ✅ | Rust 版：会话登录 + API 密钥 + provider 连接 + 分析/审计/日志导出（无数据库：JSON 文件 + 内存环）。见 §11 |
| `错误形状` | `{error:{message,type,code}}` | ✅ 相同 | `errorConfig.ts#ERROR_TYPES` 映射一致 |

### 多模态图片输入（chat 内）

chat 消息中的图片输入三种上游格式均支持（对照原版 content-block 翻译）：

| 方向 | 映射 |
|---|---|
| openai → openai 系 | `image_url` part 原样透传 |
| openai → claude 系 | `image_url` → claude image block：`data:` URL → `{type:"base64", media_type, data}`；http(s) → `{type:"url"}` |
| claude → openai 系 | claude image block → `image_url`（base64 source → data URL；url source → url） |
| openai → gemini | `data:` URL → `inlineData {mimeType, data}`；http(s) URL → `fileData {fileUri, mimeType}` |
| openai-responses → chat | `input_image` → `image_url` |

Combo 模型 ID 也会暴露多模态能力：`/v1/models` 为 provider 和 combo 返回
`supportsVision` 与 `modalities`。当 chat 请求包含图片时，路由会在回退前
过滤 combo 候选，只选择支持视觉的候选，避免图片请求落到纯文本 provider。
目录中的 `contextWindow`/`maxTokens` 也按具体模型计算（保留兼容旧客户端的
`contextLength`）；combo 对外取候选中的最大窗口，实际路由再按输入+输出大小
过滤过小候选，因此全部为 1M 模型的 coding 组合不会再被固定成旧的 128K。
provider 与 combo 还会暴露 text/image/PDF 输入、vision/PDF 能力和思考等级。
Provider connection 可主动刷新上游 `/models`；同步到的非敏感能力字段会覆盖本地
推断规则，不会保存凭据。
目录只包含已连接且启用的 provider 及其可见模型；启用的 managed combo 和内置
自动路由只有在至少解析出一个已连接候选时才会发布。

## 2. Provider 体系

- 原版 `providerRegistry.ts` ≈ 240 个 provider（OAuth/网页逆向 executor：antigravity/grok-web/deepseek-web/cursor/bedrock/vertex…）。
- Rust 版静态注册 **146 个 provider**：24 个手写高频条目（anthropic/openai/gemini/glm/zai/kimi/deepseek/openrouter/groq/xai/mistral/together/fireworks/perplexity/minimax/siliconflow/dashscope/doubao/ollama/ollama-cloud/lmstudio/opencode/opencode-zen/opencode-go）另加 122 个批量提取的纯 HTTP API-key 条目（openai/openai-responses/claude/gemini 四格式、默认 executor、可表示的 key 头、base URL 逐字取自原版，含别名、claude `anthropic-version` 头与 `chatPath` 覆盖），URL/认证头/URL 后缀（`?beta=true`）与原版 registry 一致：
  - anthropic：`https://api.anthropic.com/v1/messages?beta=true` + `x-api-key` + `anthropic-version: 2023-06-01`
  - gemini：`{base}/models/{m}:streamGenerateContent?alt=sse` + `x-goog-api-key`
  - kimi：`forceStream`（上游强制 SSE，网关折叠回 JSON）
- chat 路径拼接复刻 `normalizeOpenAIChatUrl`：自带路径的 base 直接用、`.../v1` 补 `/chat/completions`、其余补 `/v1/chat/completions`（同时修掉了 glm/perplexity/doubao 这类全路径 base 的 double-path 问题，三者 base 已换回原版原文）。
- 有意不导入：oauth/cookie/网页 executor、stdio/websocket 传输、自定义 key 头（oneminai/ideogram）、非 HTTP 格式（antigravity/cursor/kiro/clova/magnific-image/custom）、无 base URL 条目，以及多 URL 故障转移（只取单个 base）。
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

## 7. Thinking 与客户端兼容

网关默认保留客户端推理字段，并在转发到 Claude/Gemini 时转换常见的
OpenAI 兼容字段。`OMNIROUTE_THINKING_MODE` 支持 `passthrough`（默认）、
`auto`/`adaptive`（移除客户端推理字段和历史 thinking 块）以及配合
`OMNIROUTE_THINKING_BUDGET` 的 `custom`。客户端上下文空间较小时可使用
`OMNIROUTE_THINKING_MODE=auto OMNIROUTE_COMPRESSION=lite`；当前策略也会由
`GET /v1/settings` 返回。模型目录同时提供 `supportsThinking`、
`reasoningEfforts`、`thinkingLevels` 等推理元数据。
如果客户端维护独立的模型 profile，需要在客户端本地映射这些等级；发现协议只
标准化上下文/输出元数据，并没有统一的 effort 选择器字段。

## 8. 配置/CLI

- 端口默认 **20128**（`--port` > `PORT` env > toml > 20128）——与原版一致（8317 只是原版 mitm 子系统端口）。
- 凭据文件沿用原版 `provider-credentials.json`（camelCase `apiKey`/`baseUrl` 兼容，扁平 schema 也接受）。
- `.env` 三层 first-wins 加载一致。
- CLI：原版 88 子命令（ Electron tray、MCP stdio、dashboard 管理、backup/update 等）；Rust 版实现核心 **7 个**：serve(默认)/status/stop/models/providers/combos/doctor，通用 `--output json|table/--api-key/--base-url/--port` 对应原版全局 flag。pidfile 停启逻辑同原版 `processSupervisor`。

## 9. Web 仪表盘 / PWA / Electron 桌面壳

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
| 仪表盘鉴权 | dashboard JWT/session | ✅ 账号登录：首装默认密码 **CHANGEME**（与原版一致），`POST /v1/auth/login|logout|change-password`，API key 管理可给客户端授权 |

## 10. 明确未重写（超出核心网关）

- 网页级逆向 executor（`open-sse/executors/*.ts` 数百个：chatgpt-web、claude-web、gemini-web、cursor、antigravity、grok-web、kiro…）
- MCP/A2A 协议服务、WebSocket 路由事件流
- 语义缓存/幂等缓存、服务端 tool-loop、streamRecovery、吞吐 watchdog（token 压缩已实现 — 见 §6）
- SQLite 存储 + STORAGE_ENCRYPTION_KEY 加密列（Rust 版配置来自文件+环境变量）

## 验证

- `cargo test`：131 单元（解析/翻译/策略/熔断/限流/SSE 解析）+ 16 集成（mock upstream 全链路：非流式、SSE、claude⇄openai 双向、failover、目录、鉴权、404）。
- 二进制冒烟：`omniroute serve` 起服后 `/healthz`、`/v1/models`、CLI status/doctor 验证。

## 11. 仪表盘与管理面（Rust 实现）

Rust 版仪表盘复刻了原版侧边栏信息架构
（`src/shared/constants/sidebarVisibility/sections.ts`）与
`DashboardLayout` / `Sidebar` / `LanguageSelector` 组件，但不运行 Next.js：
网关直接在 `/dashboard` 内嵌单页应用。

### 鉴权
| 项 | Rust 实现 |
|---|---|
| 首装默认密码 | **`CHANGEME`**（与原版首次部署默认值一致） |
| 存放 | `$DATA_DIR/dashboard-auth.json`，加盐 SHA-256，权限 600，每次校验重读 → 重置无需重启 |
| 环境变量覆盖 | `OMNIROUTE_ADMIN_PASSWORD`（每次启动生效） |
| 端点 | `POST /v1/auth/login`（7 天会话 + HttpOnly Cookie）、`/logout`、`GET /auth/me`、`POST /auth/change-password` |
| 找回 | `omniroute reset-password [--password X \| --password-stdin \| 管道 stdin]`（对齐 `bin/reset-password.mjs`） |
| 默认密码提示 | `/auth/me` 返回 `using_default_password`，界面顶部强制改密横幅 |

### 管理端点
| 端点 | 用途 |
|---|---|
| `GET/POST /v1/api-keys`、`PATCH/DELETE /v1/api-keys/{id}` | 多密钥管理；角色 default/admin；`sk-or-*`；密钥仅创建时显示一次；按 `createKeySchema` 支持模型范围/用量限制/chaos 字段 |
| `GET/POST /v1/provider-connections`、`PATCH/DELETE /{id}`、`POST /{id}/test` | 连接 CRUD，运行时注册进注册表 + 1-token 连通性探活（探活与路由聊天会带上已存 key/base，空字段回落注册表默认值） |
| `GET /v1/provider-catalog` | 352-provider 目录（`freeTier`/`ide`/`serviceKinds`/官网按原版 `src/shared/constants/providers/**` 重新提取，分区标记与原版 ID 集合一致）叠加实时统计（`total/connected/error/allDisabled`）、注册表+连接模型（供按模型搜索）、动态 `compatibleNodes`，以及如实的 `expirations`/`blockedProviders`/`openRouterStats` 空值 |
| `POST /v1/providers/test-batch` `{mode, providerId?, connectionIds?}` | 对齐 `/api/providers/test-batch`：mode 支持 all/provider/oauth/free/no-auth/apikey/compatible/web-cookie/search/audio/local/upstream-proxy/cloud-agent/ide/selected（除 `selected` 外只测启用连接）；返回 `{mode, results[], summary{total,passed,failed}, testedAt}` |
| `GET /v1/stats`、`/v1/stats/providers`、`/v1/quotas`、`/v1/combo-health` | 运行时 + 逐 provider/逐 combo 分析 |
| `GET /v1/logs`（支持 `provider`/`model`/`status`/`class`/`errors`/`stream`）、`GET /v1/logs/export?format=csv\|json` | 请求分析 + 导出 |
| `GET /v1/audit` | 管理动作审计环（登录、密钥、provider、改密、服务操作） |
| `GET/POST /v1/compression`、`GET /v1/settings` | 压缩运行时配置 + 限流/鉴权视图 |
| `POST /v1/admin/service/restart\|stop` | 侧边栏服务按钮（适配 systemd：restart 走 abort、stop 走 exit 0） |

### 侧边栏覆盖度
侧边栏与原版 `sections.ts` **1:1**：10 个 section（Home · OmniProxy ·
Analytics · Costs · Monitoring · Dev Tools · Agentic Features · Other
Features · Configuration · Help）、8 个分组（Compression Context · Tools ·
Integrations · Logs · Audit · System · Gamification · Batch）、94 个条目按
原版顺序排列，id/图标/i18nKey/labelFallback/subtitleFallback 全部对齐。

共 76 个页面（34 个数据驱动 + 42 个占位；完整审计见 [PAGES.md](PAGES.md)）。
数据驱动：首页（快速入门、提供者拓扑、最近请求）· Endpoints · API Manager ·
Providers · Combos · Combos Studio · Provider Quota · Quota share ·
Compression（设置 + Caveman/RTK/Headroom/Ultra/Aggressive/Lite）· Playground ·
Translator · Batch · Traffic inspector（日志行详情）· Usage · Combo Health ·
Utilization · Cache Health · Route tracing · Compression analytics ·
Provider Stats · Free tiers · Activity · Logs · Log export · Audit log ·
Health · Runtime · Resilience · Settings（General/Appearance/Sidebar/
Resilience/Security）· CLI code（原版 CLI 工具目录静态复刻）· Changelog（静态）。
占位：42 个原版上游专属模块（agent 舰队、gamification、MCP/A2A/插件运行时、
成本核算、部分设置子页等）由统一的 `upstreamPlaceholder` 工厂渲染——原版
布局、明示「Rust 后端尚未移植」、零假数据。所有页面内容区与原版一致占满
全宽（无 1120px 限宽）。

UI 对齐细节：66 套原版语言包（`src/i18n/messages/*`）+
`LanguageSelector` 选择器、自托管 Material Symbols Outlined 字体、原版深色
**与浅色** 令牌（`globals.css`）、方格纸背景、可折叠分组（持久化于
`sidebar-expanded-sections`）、逐项确定性图标配色
（`getDeterministicIconAccent` 移植）、220px 侧边栏、Ctrl+K 快速导航。

### 有意保留的差异（Rust 版无对应实现）
仪表盘侧边栏已列出原版全部模块（1:1），但以下仍是**后端**层面的空缺，
通过诚实占位页呈现：OAuth/网页反向执行器（antigravity、grok-web、cursor…）·
MCP stdio 引擎 · A2A · cloud agents/conductor · 游戏化/Token/排行榜 ·
media-provider 流水线 · 代理池/webhooks 编辑器 · 特性开关与缓存管理 ·
`costs/*` 成本核算（无价格表）· `analytics/evals`、`analytics/search` ·
Electron 桌面壳（Rust 版仅 PWA）。

### Providers 页面对齐说明
Providers 页复刻原版结构：首提供者引导卡、汇总卡（provider/模型双搜索、
All/Configured/Compact 显示模式、新手引导、文件导入+模板、全局 Test-all）、
带 configured/total 计数的分类 chips、媒体（service-kind）过滤 chips，以及原版
分节顺序（Compatible → OAuth → IDE → Web/Cookie → Free → API-key/LLM →
No-auth → Upstream-proxy → Web-fetch → Aggregators → Enterprise → Cloud-agent
→ Local → Search → Embedding → Image → Audio → Video），各分节 Test-all 与
测试结果弹窗（`mode/results/summary`）、带 connected/error/disabled 状态 +
启用开关 + 1-token 测试的提供者卡片、详情视图（连接 CRUD、模型、测试，对应
`/dashboard/providers/[id]`），以及
`?search=&model=&mode=&cat=&media=` URL 过滤同步。诚实差异：OAuth 登录 /
浏览器 Cookie 会话 / IDE 钥匙串导入 / 过期跟踪 / OpenRouter 热度 /
风险条款文案只做信息态展示（卡片点进详情页可照常保存 key 或 base URL）；
网关实际只执行 API-key / compatible / local 连接。
