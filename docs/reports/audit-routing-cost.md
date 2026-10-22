# omniroute-rust 路由策略成本/额度感知审计报告

- 审计日期：2026-10-17（Asia/Shanghai）
- 审计方式：只读。未修改 `src/`、`tests/`、任何配置文件；未执行 git 写操作。
- 唯一交付文件：本报告。
- 参考数据：`docs/reports/opencode-go-limits-raw.md`（OpenCode Go / Go Plus 定价与额度原始表，2026-10-17 抓取）。
- 结论速览：**当前没有任何策略按真实「输入价/输出价/月额度」排序**。`cost-optimized` 用 provider 三档硬编码（不读真实定价、不知道 opencode-go）；`auto/*` 打分只有新近/上下文/模态/近期成功四项，无价格项；模型目录不保留单价；额度门控只做 opencode-go 整档跳过，不参与排序；全仓无峰谷/时段逻辑。

---

## 1. 十个策略的排序键与实现位置

策略常量表：`src/router/combo.rs:24-35`（`STRATEGIES`，共 10 个：priority / round-robin / fill-first / weighted / random / least-used / p2c / cost-optimized / lkgp / auto）。
别名归一化：`src/router/combo.rs:73-87`（`normalize_strategy`；未知名一律归 `"priority"`，`src/router/combo.rs:85`）。
统一排序入口：`src/router/combo.rs:176-225`（`order_candidates`）。

| 策略 | 排序键 | 实现位置 |
|---|---|---|
| priority | 无排序，保持 combo providers 声明序 | `src/router/combo.rs:221-222`（`_ => {}`）；候选展开序由 `expand_specs` 决定 `src/router/combo.rs:340-403` |
| round-robin | 全局原子游标 `RR_COUNTER` 对候选数取模后 `rotate_left` | `src/router/combo.rs:181-185`；游标定义 `src/router/combo.rs:102` |
| fill-first | 无排序，保持声明序（与 priority 行为一致，靠声明序实现"先填满前者"） | `src/router/combo.rs:221-222` |
| weighted | 按 `Candidate.weight` 做加权随机取一个；weight 来自 combo 语法 `provider/model=2`，嵌套 combo 权重连乘 | `src/router/combo.rs:201-211`；语法解析 `src/router/combo.rs:228-237`；连乘 `src/router/combo.rs:367,386` |
| random | `shuffle` 全排列 | `src/router/combo.rs:186-189` |
| least-used | `Reverse(circuits.inflight(provider))`（在飞请求少者优先） | `src/router/combo.rs:212-214` |
| p2c | 随机取两候选，`inflight` 高者换到后位（power-of-two-choices） | `src/router/combo.rs:190-200` |
| cost-optimized | `sort_by_key(provider_cost(&c.provider))`（见第 2 节） | `src/router/combo.rs:218-220` |
| lkgp | `Reverse(last_success(provider).elapsed())`（最久未成功者优先） | `src/router/combo.rs:215-217` |
| auto | 不经 `order_candidates` 常规分支；`resolve_candidates_with` 特判 `auto/*` → `auto_candidates` 生成后按 `"priority"` 保序返回 | `src/router/combo.rs:623-628`；候选池构建 `src/router/combo.rs:412-461` |

要点：10 个策略中只有 `cost-optimized` 名义上"看成本"，且只看 provider 档位（下节）；`least-used`/`p2c` 只看在飞负载；`lkgp` 只看健康度；`weighted` 的权重是 combo 声明里的静态数字，与价格/额度无关。

## 2. cost-optimized 的 provider_cost()：三档硬编码

`provider_cost()` 定义：`src/router/combo.rs:161-169`：

- 档 0：`ollama | lmstudio | vllm`（本地）
- 档 1：`groq | deepseek | glm | kimi | zai | ollama-cloud | dashscope`
- 档 2：`openai | anthropic | gemini | openrouter | xai | mistral | together | fireworks`
- 其余 provider（含 **`opencode-go`**）落入默认档 1（`src/router/combo.rs:167`）

事实判定：

- 该函数**不读取任何真实定价数据**——不查 models.dev 元数据、不查 provider 元数据、不查 OpenCode Go 价目表；纯粹是 provider 名 → 整数的静态 match。
- **不知道 opencode-go**：Go 订阅层（Go 价目见 `docs/reports/opencode-go-limits-raw.md` 表 A/B）不在表中，落默认档 1，与 groq/deepseek/glm 等低价档并列；`sort_by_key` 为稳定排序，同档内保持声明序，故对 opencode-go 候选的实际效果≈不排序。
- 同档内无二级排序键，"哪个性价比高"完全由 combo providers 的声明顺序决定。
- 单价变化（如 DeepSeek V4.1 Flash 峰/谷双价，`docs/reports/opencode-go-limits-raw.md:31-34`）无任何感知途径。

## 3. auto/* 虚拟组合的打分函数与 RequestFit

### 3.1 候选池构建

- `auto_candidates`（`src/router/combo.rs:412-461`）：对每个有 key 的 provider，取「装得下本请求且 `rank_score` 最高」的**一个**模型入池（`src/router/combo.rs:448`），每 provider 最多贡献 1 个候选；候选序即 provider 遍历序（`state.providers_with_keys()`）。
- `auto/best-free` 与 `auto/free` 走零成本链：`free_candidates`（`src/router/combo.rs:515-603`），先 Zen 匿名通道已验证名册（`opencode_free::order_roster`，`src/router/combo.rs:560`），再各 provider 的 free id（`is_free_model`，`src/router/combo.rs:491-501`），链上限 `AUTO_FREE_MAX_CANDIDATES = 12`（`src/router/combo.rs:70`）。
- combo 内嵌套 `auto/*` 步骤同样展开（`src/router/combo.rs:361-372`）；展开出免费链的步骤强制用 `"priority"` 保序，防止 `weighted`/`random` 破坏打分序（`src/router/combo.rs:284-291`）。

### 3.2 打分函数 rank_score

`rank_score`（`src/opencode_free.rs:270-313`）四分量加权和：

| 分量 | 权重 | 定义位置 |
|---|---|---|
| newness（本网关首次见到该 id 的新近度，窗口 7 天） | 0.40 | `src/opencode_free.rs:219`（`WEIGHT_NEWNESS`），计算 `src/opencode_free.rs:272-279` |
| context（context_window / 1M 封顶） | 0.30 | `src/opencode_free.rs:221`（`WEIGHT_CONTEXT`），计算 `src/opencode_free.rs:280-286` |
| modalities（image 0.5 + pdf 0.5） | 0.20 | `src/opencode_free.rs:223`（`WEIGHT_MODALITIES`），计算 `src/opencode_free.rs:287-294` |
| verified（近 24h 内本网关 2xx 成功） | 0.10 | `src/opencode_free.rs:225`（`WEIGHT_VERIFIED`），计算 `src/opencode_free.rs:295-300` |

排序出口 `order_models_for`（`src/opencode_free.rs:317-328`）：总分降序，平局按模型名字典序。

**判定：打分函数不含价格项、不含额度项。** 价格唯一参与路径是免费的二元判定（`cost_free`，见第 4 节）；额度完全不参与。

### 3.3 RequestFit 尺寸筛选

- 结构：`RequestFit { prompt_tokens, output_tokens }`（`src/router/combo.rs:114-118`）。
- 判定：`candidate_fits`（`src/router/combo.rs:148-159`）——`caps.context_window >= prompt + min(requested_output, candidate_max_output)`；无 fit 信息时恒真（`src/router/combo.rs:154-156`）。输出预留按候选自身上限收敛：`candidate_output_tokens`（`src/router/combo.rs:130-136`）。
- 构建点（chat_core 侧）与 auto 构建点共用同一套算术，注释明确二者永不分歧（`src/router/combo.rs:108-113`）。

---

## 4. model_catalog 是否保留单价：不保留

- `CatalogEntry` 字段（`src/model_catalog.rs:187-200`）：`context_window / max_output_tokens / modalities / reasoning / free / deprecated` —— **无价格字段**。
- 落盘行 `Row`（`src/model_catalog.rs:202-210`）：`provider / model / context_window / max_output_tokens / modalities / reasoning` + `free / deprecated`（`Row` 定义续行）。
- 唯一消费 `cost` 之处：`cost_free()`（`src/model_catalog.rs:363-371`），把 `cost.input`/`cost.output` 折算成 `Option<bool> free` 后即丢弃；调用点 `src/model_catalog.rs:353`（解析 models.dev 时）与 `:399`（内置快照行）。
- 实证：`~/.omniroute-rust/model-catalog.json` 8,828 行，字段恰为上述集合，opencode-go 32 行**无 cost**；而上游 `models.dev/api.json` 的 `opencode-go.deepseek-v4.1-flash.cost = {input:0.15, output:0.6, cache_read:0.003}`。详见 `docs/reports/live-probe.md` 第二节。

**判定：价格数据抵达本机后被裁剪，路由层无从取得。**

## 5. 额度感知现状：整档门控，不参与排序

- 数据源：`src/opencode_quota.rs`，`GET /zen/go/v1/usage`（新别名 `/inference/go/v1/usage`），解析 `usage.{rolling,weekly,monthly}.{status,percent,resetsAt}`（`src/opencode_quota.rs:37-68,127-167`）；`exhausted()` 判 `status=="rate-limited"` 或 `percent>=100`（`:48`），`remaining_percent()`（`:51-52`）。
- 门控语义：`gated(now)`（`:271`）与 `gate_has_known_reset(now)`（`:283`）。
- 消费点（唯一）：`src/core/chat_core.rs:349-375` —— 仅当 `cand.provider == "opencode-go"` 时生效：
  - 有 `resetsAt` → **硬跳过**（不半开探测，避免每请求白撞一次 429）；
  - 无 `resetsAt` → **软跳过**（保留每请求一次的半开探测，见 `:370-372`）。
  - 跳过原因写入 `skipped` 明细，并向客户端给出按重置倒计时的 `Retry-After`（`:495-503`）。
- 另有 operator 手填的每 provider 账户页：`QuotaOverride { tier, auth_kind, balance, currency, cutoff, rpm, concurrent, note }`（`src/server/combos_admin.rs:127-149`），severity 由「窗口请求数 / cutoff」推算（`:190-200`）。
  **该存储只被 dashboard 读取**（`src/server/dashboard.rs:1337-1460` 与 `:1634`；`state.rs:121,195`），`chat_core` / `router` 全无引用 → **路由不消费**。
- 每 key 成本字段 `cost_usd`（`src/server/security.rs:58`）初始化后**从未累加**（全仓仅 `:300` 赋 0.0，及 dashboard/admin 展示）→ 网关当前没有任何花费累计。

**判定：额度只有「整档是否耗尽」的二元门控，既不区分模型、也不进入排序；每模型额度与已花费额均无运行时表示。**

## 6. 全仓无峰谷/时段逻辑

- 无 `chrono`/时区库依赖；时间一律 `SystemTime::now()` 取 epoch（全仓 18 处，皆为时间戳用途）。
- 唯一「星期」计算是 dashboard 图表的 `weekday_index`（`src/server/dashboard.rs:954,1067`），与路由无关。
- 无任何 `peak`/`off-peak`/时段判定；DeepSeek 双价（`docs/reports/opencode-go-limits-raw.md` 表 A）在代码中不存在。

## 7. 差距清单与最小改动落点

| # | 缺口 | 现状 | 最小改动落点 |
|---|---|---|---|
| G1 | 模型单价未保留 | 仅折算为 `free` 布尔 | `Row`/`CatalogEntry` 增 `cost_input/cost_output/cost_cache_read: Option<f64>`；填充点 `src/model_catalog.rs:346-355`（解析）与 `:399`（快照行）；结构定义 `:187-210`。落盘/加载随 serde 自动生效 |
| G2 | 每模型月额度（美元）无来源 | 上游无此字段，仅官方文档 | 新增本地静态表（如 `src/plan_limits.rs`），键 `(provider, model)`，值 `monthly_limit_usd`；数据源 `docs/reports/opencode-go-pricing.json` |
| G3 | 峰谷时钟 | 无 | 新增 `fn deepseek_peak(now_epoch_secs) -> bool`：`weekday∈Mon–Fri && (01–04 UTC || 06–10 UTC)`；可由 epoch 秒直接推星期，无需引入时区库 |
| G4 | 排序不读价格 | `order_candidates` 的 `cost-optimized` 分支只比 provider 档（`src/router/combo.rs:218-220`） | 改该分支为「按本次请求估算成本升序」：`cost = prompt×in + min(max_out,cap)×out`（缓存读占比未知时可保守按全价，或按 `RequestFit` 估） |
| G5 | `order_candidates` 拿不到请求尺寸 | 签名 `(state, strategy, cands)`（`src/router/combo.rs:176`） | 追加 `fit: Option<RequestFit>` 形参；调用点 4 处：`:291`（`candidates_for_combo`）、`:626`、`:635`、`:652`（`resolve_candidates_with`） |
| G6 | 每模型额度余量 | 仅整档百分比 | 阶段一：用 `opencode_quota::Window::remaining_percent()` 作整档折扣因子（`src/opencode_quota.rs:51`）；阶段二：按响应 usage 累计 per-model 花费（新计数器，`security.rs` 的 `cost_usd` 可复用但需真正累加） |

改动规模估计：G1+G2+G3+G4+G5 合计约 150–250 行（含单测），无架构性变更，符合最小改动原则。

## 8. 零改代码可达成的近似（今天即可用）

| 手段 | 能力 | 局限 |
|---|---|---|
| `priority` + 人工按性价比排序候选 | **可实现静态性价比路由**（正是现有 coding-flash/coding-plan 的形态，只是次序未按价格优化） | 静态，不随峰谷、不随额度余量变化 |
| `auto/best-free`（`auto/free`） | 零成本车道自动排序（`rank_score`，价格为 0 时「性价比」等价于「可用性」） | 只覆盖免费名册 |
| `weighted` | 概率分流 | **不能用于排序**：实现是「随机取一个作为头、丢弃其余」（`src/router/combo.rs:201-211`），会丢掉链尾兜底 |
| `cost-optimized` | 名义成本优先 | 今日等同 `priority`（opencode-go 与多数 provider 同落档 1，稳定排序保声明序） |
| 外部 cron + `/v1/combos/managed` | 峰时/谷时切换两套组合 | 需管理会话；网关无内置定时器，属脆弱方案 |

## 9. 文档与实现不一致（对外语义失真）

- `src/server/dashboard_assets/locales/zh-CN.json` / `en.json` 的 `combos.strategyRecommendations`：
  - `cost-optimized`：「在具备定价元数据时，优先路由到成本更低的模型」/ "Routes to lower-cost models when pricing metadata is available" —— 实现**不读任何定价元数据**（第 2 节）。
  - `auto`：「基于成本、延迟、质量和健康的实时评分进行路由」/ "Route based on real-time scoring for cost, latency, quality, and health" —— 实现四项分量为新近/上下文/模态/近期成功（第 3.2 节），**无成本、无延迟、无质量**。
  - `fill-first`：「配额耗尽策略」——实现与 `priority` 同为保序（`src/router/combo.rs:221-222`），配额耗尽由冷却/额度门控被动触发，非该策略主动实现。
- 策略可选集不一致：前端 `STRATEGY_FALLBACK`（`src/server/dashboard_assets/app.js:1856-1859`）与 locale 键共 14 个策略，含 `reset-aware`、`strict-random`、`context-optimized`、`context-relay`；后端 `STRATEGIES` 仅 10 个（`src/router/combo.rs:24-35`）。
  未实现的四个经 `normalize_strategy` 落入 `_ => "priority"`（`src/router/combo.rs:85`）→ **UI 选了却在后端静默退化为 priority**，且无任何告警。
- `auto` 作为 **strategy id** 在 `order_candidates` 中无分支（落入默认保序），与作为**模型 id 前缀**的 `auto/*` 虚拟组合是两回事，易被误读为「自动多因子路由」。

## 10. 结论

1. **没有任何策略按真实「输入价 / 输出价 / 月额度」排序。**
2. 「名义上像」的只有 `cost-optimized`（provider 三档硬编码，不认识 opencode-go，无二级排序键）；「真会自己排序」的只有 `auto/*`（四因子评分，价格权重为 0）。
3. 额度侧只有 opencode-go 的**整档二元门控**（`src/core/chat_core.rs:349-375`），既不区分模型也不参与排序；operator 手填的 `provider-quotas` 账户页**未被路由消费**。
4. 价格数据「已到手却丢弃」（`src/model_catalog.rs:353,363-371`）、每模型月额度「上游无此字段」、峰谷「代码无此概念」——三处补齐后即可实现按性价比自动路由，规模约 150–250 行。
5. 不改代码的近似：`priority` + 按性价比人工排序 + `auto/best-free` 兜底；峰谷需外部定时切换，属脆弱方案。
