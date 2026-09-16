# 页面审计：原版 dashboard vs omniroute-rust

参照：`diegosouzapw/OmniRoute` v3.8.x，导航唯一真相来源
`src/shared/constants/sidebarVisibility/sections.ts`（10 分组约 93 项）。
2026-09 审计：逐个读取原版各 `page.tsx` 前约 60 行，对照内嵌 SPA
（`src/server/dashboard_assets/app.js`，32 个 `PAGES.*`）。英文版见
[../PAGES.md](../PAGES.md)。

状态：**done** = 数据驱动页面 · **partial** = 被其他页面覆盖或只读 ·
**gap** = Rust 无对应实现（注明原因）· **redirect** = 原版同样只是跳转别名。

## Home / OmniProxy

| 原版路由 | Rust 页面 | 状态 | 说明 |
|---|---|---|---|
| `/home` | Home | done | 快速入门、提供者拓扑、最近请求 |
| `/dashboard/endpoint` | Endpoints | done | 活跃端点、本地服务、逐端点模型数、全局 system prompt，tunnel/别名如实置灰 |
| `/dashboard/api-manager` | API Manager | done | 密钥 CRUD + 轮换、过滤、用量/费用列 |
| `/dashboard/providers` | Providers | done | 352 目录、18 分节、compatible 节点、批量测试+结果、详情视图、URL 过滤 |
| `/dashboard/providers/services` | Embedded services | done | 本地执行器如实标不可用 |
| `/dashboard/combos` | Combos | done | 构建器、权重、校验、测试 |
| `/dashboard/combos/live` | Combos Studio | done | 实时路由视图 |
| `/dashboard/quota` | Provider Quota | done | cutoff/余额/层级与严重度 |
| `/dashboard/costs/quota-share` | Quota share | done | 跨 key 预算 |
| `/dashboard/context/settings\|combos\|caveman\|rtk\|headroom\|lite\|aggressive\|ultra` | Compression + 各引擎页 | done | 运行时编辑器 + 逐引擎视图 |
| `/dashboard/context/session-dedup\|ccr\|llmlingua\|omniglyph`、`/compression/studio\|exclusions` | — | gap | 引擎超出范围（PARITY §6：仅实现 lite/standard/aggressive/ultra/rtk） |
| `/dashboard/cli-code`、`/cli-agents`、`/acp-agents`、`/cloud-agents`、`/conductor`、`/orchestration`、`/tools/agent-bridge` | — | gap | Agent 运行时/舰队需要 Rust 网关没有的执行器 |
| `/dashboard/tools/traffic-inspector` | Logs（行详情抽屉） | done | 请求环全字段弹窗 |
| `/dashboard/discovery` | — | gap | 模型扫描需要上游目录同步（注册表静态 + 连接） |
| `/dashboard/api-endpoints` | Endpoints | partial | 列表对齐；无 OpenAPI 试调控制台 |
| `/dashboard/webhooks` | — | gap | webhook 编辑器需要投递引擎 |
| `/dashboard/log-export` | Log export | done | CSV/JSON + 投递状态 |
| `/dashboard/system/proxy` | — | gap | 上游 mitm 代理子系统，超出范围 |

## Analytics / Costs / Monitoring

| 原版路由 | Rust 页面 | 状态 | 说明 |
|---|---|---|---|
| `/dashboard/analytics`（枢纽） | Usage | partial | 概览分析；evals/search 页签为 gap（下） |
| `/dashboard/analytics/combo-health` | Combo Health | done | |
| `/dashboard/analytics/utilization` | Utilization | done | |
| `/dashboard/cache` | Cache Health | done | 去重统计，语义缓存如实标关闭 |
| `/dashboard/analytics/compression` | Compression analytics | done | 请求环 token 节省 |
| `/dashboard/analytics/search`、`/analytics/evals` | — | gap | 需要 search/RAG 与 eval 脚手架 |
| `/dashboard/provider-stats` | Provider Stats | done | 逐 provider 计数 + 冷却 |
| `/dashboard/costs`、`/costs/pricing`、`/costs/budget` | — | gap | 需要价格表（无 `costs/*` 核算） |
| `/dashboard/free-tiers` | Free tiers | done | 151 条目录 + 连接状态；无 token 总数头图（无法核验池预算） |
| `/dashboard/free-provider-rankings` | — | gap | 编辑精选的任务适配排行 |
| `/dashboard/radar` | Route tracing | partial | 路由决策；radar 目录/推荐属推广位 |
| `/dashboard/activity` | Logs | partial | 复用请求环（无独立合规 feed） |
| `/dashboard/logs`、`/logs/proxy\|console\|timeline`、`/conversations` | Logs | partial | 环 + 详情；无 proxy/console/timeline 切分、无会话正文（环仅元数据） |
| `/dashboard/audit`（+`/mcp`、`/a2a`） | Audit log | done | 管理动作环（无 MCP/A2A 侧） |
| `/dashboard/health` | Health | done | 探针 + 压缩/鉴权状态 |
| `/dashboard/runtime` | Runtime | done | pid/运行时长/RSS/计数 |
| `/dashboard/resilience/connections` | Resilience（+ Settings·Resilience） | done | 冷却、速率窗口、熔断画像 |

## Dev Tools / Agentic / Other / Configuration

| 原版路由 | Rust 页面 | 状态 | 说明 |
|---|---|---|---|
| `/dashboard/translator` | Translator | done | 格式文档 + 实时转换检查 |
| `/dashboard/playground` | Playground | done | 经网关发真实聊天 |
| `/dashboard/search-tools` | — | gap | 需要带 key 的搜索 provider（透传存在，无工作台） |
| `/dashboard/memory`、`/agent-skills`、`/omni-skills`、`/mcp`、`/a2a`、`/plugins` | — | gap | memory/MCP/A2A/skill 运行时超出范围 |
| `/dashboard/chaos` | — | gap | 并行 chaos 需要扇出执行器；`chaos_mode_enabled` 仅存于 key 上、未实际执行 |
| `/dashboard/leaderboard`、`/profile`、`/tokens` | — | gap | 游戏化装饰，有意排除 |
| `/dashboard/cache/media` | — | gap | media-provider 流水线超出范围 |
| `/dashboard/batch` | Batch | done | 经 `provider/model` 路由的 batch/文件状态 |
| `/dashboard/settings/general` | Settings·General | done | 限流、鉴权、上游超时（只读） |
| `/dashboard/settings/appearance` | Settings·Appearance | done | 主题 + 语言 |
| `/dashboard/settings/sidebar` | Settings·Sidebar | done | 按浏览器持久化的导航可见性，新页面自动纳入 |
| `/dashboard/settings/resilience` | Settings·Resilience | done | 冷却画像 |
| `/dashboard/settings/security` | Settings·Security | done | 密码 + 鉴权 |
| `/dashboard/settings/ai\|modality-bridge\|routing\|advanced\|access-tokens\|feature-flags\|cache` | — | gap | AI/模态/执行器设置依赖 TS 运行时；access-tokens ≈ API Manager |
| `/dashboard/changelog` | — | gap | 新闻阅读器装饰（发布说明在 git 里） |
| `/docs`（外部） | Docs 导航 | done | 指向上游仓库 |

原版同样只是跳转别名（两侧都无 UI）：
`auto-combo`、`compression`（bare）、`context`（bare）、`limits`、
`settings`（bare）、`usage`（bare → logs）。两侧都没有 bare 页面的子路由分组：
`gamification`、`resilience`、`system`、`tools`。原版有、Rust 无的未收录功能页：
`relay`（token CRUD）、`onboarding`（已被 providers 新手引导 + 首装提示覆盖）、
`media-providers/[kind]`。
