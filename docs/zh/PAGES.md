# 页面审计：原版 dashboard vs omniroute-rust

参照：`diegosouzapw/OmniRoute` v3.8.x，导航唯一真相来源
`src/shared/constants/sidebarVisibility/sections.ts`（10 个 section 约 93 项）。
内嵌 SPA（`src/server/dashboard_assets/app.js`）现已与该文件 **1:1**——相同的
section、分组、顺序、图标与 i18n key——每个非外链条目都有对应 `PAGES.*`
注册（76 个页面）。2026-09 审计：对照 `/tmp/omniroute-src`（各 `page.tsx`
前约 60 行）与内嵌 SPA。英文版见 [../PAGES.md](../PAGES.md)。

状态：**done** = 数据驱动页面 · **partial** = 被其他页面覆盖、静态复刻或只读 ·
**stub** = 已注册页面，渲染原版视觉外壳并明示「尚未移植到 Rust」的空状态
（绝无假数据）· **redirect** = 原版同样只是跳转别名。

## Home / OmniProxy

| 原版路由 | Rust 页面 | 状态 | 说明 |
|---|---|---|---|
| `/home` | Home | done | 独立首页侧边栏入口、快速入门、2fr/1fr 提供者拓扑 + 最近请求布局、3 秒刷新 |
| `/dashboard/endpoint` | Endpoints | done | 活跃端点、本地服务、逐端点模型数、全局 system prompt，tunnel/别名如实置灰 |
| `/dashboard/api-manager` | API Manager | done | 密钥 CRUD + 轮换、过滤、用量/费用列 |
| `/dashboard/providers` | Providers | done | 352 目录、18 分节、compatible 节点、批量测试+结果、详情视图、URL 过滤 |
| `/dashboard/providers/services` | Embedded services | done | 本地执行器如实标不可用 |
| `/dashboard/combos` | Combos | done | 构建向导、kimi/auto-combo 预设、模型搜索、dry-run 测试；权重暂未持久化（如实回落均分） |
| `/dashboard/combos/live` | Combos Studio | done | 实时路由视图 |
| `/dashboard/quota` | Provider Quota | done | cutoff/余额/层级与严重度 |
| `/dashboard/costs/quota-share` | Quota share | done | 跨 key 预算 |
| `/dashboard/context/settings\|combos\|caveman\|rtk\|headroom\|lite\|aggressive\|ultra` | Compression | done | 运行时编辑器 + 逐引擎视图（多个侧边栏条目复用同一页面） |
| `/dashboard/context/session-dedup\|ccr\|llmlingua\|omniglyph`、`/compression/studio\|exclusions` | Compression（别名） | partial | 侧边栏条目已注册，均路由到 Compression 页；逐引擎独立视图仍是空缺（PARITY §6：仅实现 lite/standard/aggressive/ultra/rtk） |
| `/dashboard/cli-code` | CLI Code | partial | 静态复刻原版工具目录（支持 base URL 的 code 工具）；无检测/配置同步钩子 |
| `/dashboard/cli-agents`、`/acp-agents`、`/cloud-agents`、`/conductor`、`/orchestration`、`/tools/agent-bridge` | 占位页 | stub | Agent 运行时/舰队需要 Rust 网关没有的执行器 |
| `/dashboard/tools/traffic-inspector` | Logs（行详情抽屉） | done | 请求环全字段弹窗 |
| `/dashboard/discovery` | Discovery | stub | 模型扫描需要上游目录同步（注册表静态 + 连接） |
| `/dashboard/api-endpoints` | API Endpoints | stub | 列表能力已在 Endpoints 页对齐；无 OpenAPI 试调控制台 |
| `/dashboard/webhooks` | Webhooks | stub | webhook 编辑器需要投递引擎 |
| `/dashboard/log-export` | Log export | done | CSV/JSON + 投递状态 |
| `/dashboard/system/proxy` | Proxy | stub | 上游 mitm 代理子系统，超出范围 |

## Analytics / Costs / Monitoring

| 原版路由 | Rust 页面 | 状态 | 说明 |
|---|---|---|---|
| `/dashboard/analytics`（枢纽） | Usage | partial | 概览分析；evals/search 页签为空缺（下） |
| `/dashboard/analytics/combo-health` | Combo Health | done | |
| `/dashboard/analytics/utilization` | Utilization | done | |
| `/dashboard/cache` | Cache Health | done | 去重统计，语义缓存如实标关闭 |
| `/dashboard/analytics/compression` | Compression analytics | done | 请求环 token 节省 |
| `/dashboard/analytics/search` | Search analytics | stub | 需要 search/RAG 计量 |
| `/dashboard/analytics/evals` | Evals | stub | 需要 eval 脚手架 |
| `/dashboard/provider-stats` | Provider Stats | done | 逐 provider 计数 + 冷却 |
| `/dashboard/costs` | Costs | stub | 请求环无成本核算（`/v1/usage/analytics` 仅 token） |
| `/dashboard/costs/pricing` | Pricing | stub | 需要价格表 |
| `/dashboard/costs/budget` | Budget | stub | 无花费限额子系统 |
| `/dashboard/free-tiers` | Free tiers | done | 151 条目录 + 连接状态；无 token 总数头图（无法核验池预算） |
| `/dashboard/free-provider-rankings` | Free provider rankings | stub | 编辑精选的任务适配排行 |
| `/dashboard/radar` | Route tracing | partial | 路由决策；radar 目录/推荐属推广位 |
| `/dashboard/activity` | Logs | partial | 复用请求环（无独立合规 feed） |
| `/dashboard/logs` | Logs | done | 请求环 + 全字段详情抽屉 |
| `/dashboard/logs/proxy` | Proxy logs | stub | Rust 网关无 mitm 流量 |
| `/dashboard/logs/console` | Console logs | stub | 无服务输出捕获 |
| `/dashboard/logs/timeline` | Timeline | stub | 请求环仅元数据，无逐轮时间线 |
| `/dashboard/conversations` | Conversations | stub | 未存会话正文（请求环仅元数据） |
| `/dashboard/audit` | Audit log | done | 管理动作环 |
| `/dashboard/audit/mcp` | MCP audit | stub | 无 MCP 运行时 |
| `/dashboard/audit/a2a` | A2A audit | stub | 无 A2A 运行时 |
| `/dashboard/health` | Health | done | 探针 + 压缩/鉴权状态 |
| `/dashboard/runtime` | Runtime | done | pid/运行时长/RSS/计数 |
| `/dashboard/resilience/connections` | Resilience（+ Settings·Resilience） | done | 冷却、速率窗口、熔断画像 |

## Dev Tools / Agentic / Other / Configuration

| 原版路由 | Rust 页面 | 状态 | 说明 |
|---|---|---|---|
| `/dashboard/translator` | Translator | done | 格式文档 + 实时转换检查 |
| `/dashboard/playground` | Playground | done | 经网关发真实聊天 |
| `/dashboard/search-tools` | Search tools | stub | 需要带 key 的搜索 provider（透传存在，无工作台） |
| `/dashboard/memory`、`/agent-skills`、`/omni-skills`、`/mcp`、`/a2a`、`/plugins` | 占位页 | stub | memory/MCP/A2A/skill 运行时超出范围 |
| `/dashboard/chaos` | Chaos Mode | stub | 并行 chaos 需要扇出执行器；`chaos_mode_enabled` 仅存于 key 上、未实际执行 |
| `/dashboard/leaderboard`、`/profile`、`/tokens`、`/gamification/admin` | 占位页 | stub | 游戏化装饰，有意排除 |
| `/dashboard/cache/media` | Media | stub | media-provider 流水线超出范围 |
| `/dashboard/batch` | Batch | done | 经 `provider/model` 路由的 batch/文件状态 |
| `/dashboard/batch/files` | Batch files | partial | Files API 透传说明（`POST /v1/files`）；无本地文件登记表可列 |
| `/dashboard/settings/general` | Settings·General | done | 限流、鉴权、上游超时（只读） |
| `/dashboard/settings/appearance` | Settings·Appearance | done | 主题 + 语言 |
| `/dashboard/settings/sidebar` | Settings·Sidebar | done | 按浏览器持久化的导航可见性，新页面自动纳入 |
| `/dashboard/settings/resilience` | Settings·Resilience | done | 冷却画像 |
| `/dashboard/settings/security` | Settings·Security | done | 密码 + 鉴权 |
| `/dashboard/settings/ai` | Settings·AI | stub | AI 运行时依赖 TS 执行器 |
| `/dashboard/settings/modality-bridge` | Settings·Modality bridge | stub | 图像/音频桥接超出范围 |
| `/dashboard/settings/routing` | Global Routing | stub | `/v1/settings` 无路由字段；路由在 Combos/Quota 页 |
| `/dashboard/settings/advanced` | Settings·Advanced | stub | 执行器开关依赖 TS 运行时 |
| `/dashboard/settings/access-tokens` | Access Tokens | stub | API Manager 已覆盖密钥 CRUD；无个人 token 子系统 |
| `/dashboard/settings/feature-flags` | Settings·Feature flags | stub | 无特性开关存储 |
| `/dashboard/settings/cache` | Settings·Cache | stub | 缓存调优未暴露 |
| `/dashboard/changelog` | Changelog | partial | 静态指向上游 releases 页（仓库无 CHANGELOG） |
| `/docs`（外部） | Docs 导航 | done | 指向上游仓库 |
| Issues（外部） | Issues 导航 | done | 上游 GitHub issues，已加入 Help 组 |

原版同样只是跳转别名（两侧都无 UI）：
`auto-combo`、`compression`（bare）、`context`（bare）、`limits`、
`settings`（bare）、`usage`（bare → logs）。两侧都没有 bare 页面的子路由分组：
`gamification`、`resilience`、`system`、`tools`。原版有、Rust 无的未收录功能页：
`relay`（token CRUD）、`onboarding`（已被 providers 新手引导 + 首装提示覆盖）、
`media-providers/[kind]`。
