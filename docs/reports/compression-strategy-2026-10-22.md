# omniroute-rust 压缩策略优化方案（对标 rtk-ai/rtk，目标 60%–90% token 节省）

> 日期：2026-10-22　｜　工作区：`omniroute-rust`
> 设计稿：[`docs/LRC.md`](../LRC.md)　｜　实测数据：[`docs/reports/lrc-benchmark-2026-10-22.md`](lrc-benchmark-2026-10-22.md)
> 配套文档：[`compression-audit-2026-10-22.md`](compression-audit-2026-10-22.md)（现状审计）·[`rtk-upstream-notes-2026-10-22.md`](rtk-upstream-notes-2026-10-22.md)（上游借鉴）·[`redundancy-measurement-2026-10-22.md`](redundancy-measurement-2026-10-22.md)（冗余度量）

## 摘要

1. **现状**：网关 6 档压缩（off/lite/standard/aggressive/ultra/rtk）默认关闭，且**只改 `messages[].content` 字符串**；
   实测在真实 agent 语料上 p50 节省 21%–42%、加权 27%–47%，但**被改动消息中 20%–72% 发生保真破坏**
   （lite 70.2%、aggressive 72.1%、standard 58.2%、rtk 33.1%、ultra 19.9%）。
2. **瓶颈不在规则强度，在作用面**：真实请求里 90.4% 的 token 来自 3 步以前的历史；历史 reasoning 26.5%、
   历史 tool_call 参数 21.3%、bash 结果 22.2%、tools schema 9.6% —— 这四块**现有引擎全部碰不到或只能破坏性删**。
3. **rtk 的可借鉴点**：它把"删内容"变成"可回捞"（CCR：预览+哈希 marker+原文 store+取回≥2 次即改回原样），
   按工具能力分档（Read 永不压、Bash 300 tok 起、写入体退休为指针），并有 never-worse 双门与
   「逐轮变量规则拒绝加载」的缓存稳定性宪法；其 60–90% 严格指 *bash 输出字节*，从不宣称账单降幅。
4. **方案（LRC：分层可逆压缩）**：分桶 + 保护层 + 无损层 + 有损可逆层 + 门禁 + 分桶记账。
   原型实测：保守 13.0%、中性 41.8%、激进 54.3%（加权），保真门禁全通过；极限档见 §5。
5. **结论**：**60%–70% 可在"不破坏会话"的硬门禁下稳定达到；80%–90% 需要接受三件事**——
   历史 reasoning 整块删除、tools schema 命名空间剪枝（需客户端配合）、tool 结果退化为纯指针（依赖回捞通道）。
   只靠"更狠地删"是达不到的：零回捞、零风险的纯删除上界实测只有 **15.3%**（无损 1.5%）。

---

## 1. 现状体检

### 1.1 六档选择链

总开关 `enabled`（默认 false，off 为硬杀）→ 请求头 `x-omniroute-compression`
（`off|default|lite|standard|aggressive|ultra|rtk`，未知值穿透）→ `auto_trigger_tokens` 自动触发 → `default_mode`。
压缩发生在**候选解析之前**（`src/core/chat_core.rs:202`），所以节省会同时改善「能否装进窗口」的判定。

### 1.2 作用面矩阵（构造性事实）

| 字段 | lite | standard | aggressive | ultra | rtk |
|---|---|---|---|---|---|
| `messages[].content`（string） | ✅ | ✅（默认仅 user） | ✅ | ✅ | ✅（仅 tool/function） |
| `content` parts 数组的 text | 部分 | ✅ | ❌ | ✅ | ❌ |
| `tool_calls[].function.arguments` | ❌ | ❌ | ❌ | ❌ | ❌ |
| 历史 reasoning / thinking | ❌ | ❌ | ❌ | 部分（破坏性） | ❌ |
| `tools` JSON schema | ❌ | ❌ | ❌ | ❌ | ❌ |
| Claude `tool_result` 块 / 顶层 `system` | ❌ | ❌ | ❌ | ❌ | ❌ |
| `/v1/responses`（input 形状） | 整链 skipped | — | — | — | — |

两处真缺陷：`aggressive` 的降级链把 `system` 硬编码进 `compress_roles`（绕过 `preserve_system_prompt`）；
`ultra` 不保护末条 user（直接剪当前指令）。

### 1.3 现有五档实测（586 步真实载荷）

| 档 | p50 节省 | 加权节省 | 改动消息中破坏率 |
|---|---|---|---|
| aggressive | 42.2% | 47.2% | **72.1%** |
| lite | 41.2% | 46.0% | **70.2%** |
| rtk | 27.3% | 31.2% | 33.1% |
| ultra | 24.8% | 31.3% | 19.9% |
| standard(caveman) | 20.9% | 27.3% | 58.2% |

「破坏」= 被改动的消息里丢了报错行 / 文件路径 / 代码围栏 / diff hunk / URL 之一。
即：**现有档不是省不下来，而是省的方式会毁会话**——这正是 60–90% 目标要换打法的原因。

---

## 2. 真实语料解剖（决定优化上限）

语料：本机 DSH `~/.dsh/sessions/**/session.v4.jsonl.zstd` 取最近 100 会话（13,517 步 / 818 轮 / 70.7 MB zstd）。
口径：M1 = `ceil(utf16/4)`（与 `src/compression/estimate.rs` 一致）；真实 = provider `usage`（input + cache_read）。

| 指标 | 值 |
|---|---|
| 载荷总量 | M1 3.00 B；真实 3.74 B |
| 单请求 | M1 p50 191.4k / p90 427.0k；真实 p50 248.1k / p90 543.1k |
| chars/4 偏差 | 真实 ≈ M1 × **1.30**（p50）；tiktoken(cl100k) ≈ 真实 × 1.09 |
| 会话内增长 | 首步→末步 中位 ×5.3、p90 ×9.9 |
| **年龄分布** | 最近 3 步 **9.6%**；3–8 步 1.4%；9–20 步 2.5%；21–50 步 5.4%；**50 步以上 81.0%** |
| prompt cache | cache_read 占 (input+cache_read) **86.6%–99.2%**（按 provider） |
| 稳定前缀 | 与上一请求逐字节相同的前缀 p50 **99.69%**、增量 p50 **0.31%** |

### 2.1 构成（M1 加权）

| 类别 | 占比 | 说明 |
|---|---|---|
| 助理历史 reasoning | 26.5% | 独立复算上限 33.2%；**是否回放依赖 provider/模型** |
| tool 结果·bash | 22.2% | 全文回放 |
| tool_call 参数（历史） | 21.3% | bash 命令串 17.0%、edit 2.3%、write 2.1% |
| tools JSON schema | 9.6% | 每请求恒定；本机当前 40,371 tok/请求（81 工具，含 30 个 MCP） |
| wire 骨架 | 7.3% | JSON 结构开销 |
| tool 结果·read | 4.9% | 整文件回放 |
| 助理历史文本 | 2.6% | |
| 注入类（AGENTS/skills/runtime/time…） | 3.1% | 每轮重注入 |
| system | 0.9% | |
| 其他 | 1.6% | job_output / grep / agent 回执 |

### 2.2 两个必须写进方案的实测约束

1. **reasoning 回放是分 provider 的上限项**：用「真实 usage vs 重建（含/不含 reasoning）」判定，
   51.4% 的步、54.4% 的 token 判为回放；按模型：coding-plan 71.6%、deepseek-v4.1-flash 65.6%、
   coding-free 24.1%、glm-5.3-flash 0%。→ 删历史 reasoning 只能对「会回放」的 provider 生效。
2. **「纯删除」的天花板很低**（`notes-redundancy.md`）：逐字重复加权仅 **0.93%**（p90 2.10%）、
   近似重复 **0.48%**、注入快照去重 **0.55%**；三条合计 **1.48%（无损）～15.32%（有损但保全安全判据）**。
   → 60–90% 必须引入「换表示 + 可回捞」，而不是继续加删除规则。

---

## 3. rtk-ai/rtk 能借鉴什么、不能借鉴什么

### 3.1 它的两条通路

| 通路 | 做什么 | 与我们相关性 |
|---|---|---|
| `rtk` CLI（hook 重写命令） | 把 `git status` 改写成 `rtk git status`，压缩 **bash 输出** | 低（我们不在 shell 侧） |
| `rtk-proxy`（改请求体） | 拦截 agent→provider 请求，改 tool_result / tool_use / 图像 | **高**，但**只覆盖 Anthropic 方言**；OpenAI `/v1/chat/completions` 只记账不压 |

### 3.2 值得搬的五件

1. **CCR 可逆压缩**：块 → head/tail 预览 + 内容哈希 marker；原文入 session store（7 天）；
   注入 `rtk_retrieve` 工具、代理内部就地解题（客户端无感）；同一块被取回 ≥2 次 → **永久改回原样**（pin，止损）。
2. **按工具能力的阈值分档**：`Read` **永不压**（恢复成本 = 翻页读回，高于单轮节省）；Bash/WebFetch/Agent 300 tok 起；
   未知工具 1500；写入体退休 400。→ 我们实测也印证：read 结果被后续引用的比例最高（33.21% 的字符被 ≥2 步后引用）。
3. **never-worse 双门**：变换后 token 不降反升 → 回退原文；< 512 token 的块不动（marker 比内容贵）。
4. **缓存稳定性宪法（E010）**：proxy 规则若依赖 `tokens_in`/`message_count` 之类逐轮变量，
   **校验期直接拒绝加载**。理由量化：缓存读 0.1x vs 冷 1.0x，历史重渲染一次就把多轮节省全吐回去。
   我们实测稳定前缀 99.69%、cache 命中 99.47% —— 这条对我们是**生死线**，必须照搬为硬约束。
5. **分阶段记账**：`saved_{normalize,ingest_filter,ccr}`、`ccr_retrieve_calls`（压缩的真实代价）、
   `cost_without`（反事实）、`ctx_{tool_result,tool_use,duplicate,image}` 成分分桶。

### 3.3 它做不到、而我们可以做的

| 项 | 为什么 rtk 不行 | 我们的机会 |
|---|---|---|
| 历史 reasoning 裁剪 | Anthropic 方言要求签名块原样回传 | OpenAI 方言（deepseek/glm/longcat 系）无此约束 → 可删 oldest reasoning（占载荷 26.5%，回放模型上更高） |
| tools schema 压实 | schema 由客户端持有，proxy 不改 | 网关可在转发前规范化 schema（噪声键/嵌套描述/顶层描述截断），实测省 schema 的 8.2%–42.9% |
| tool_call 参数退休 | rtk-proxy 只处理 Anthropic 的 `Write/Edit` | OpenAI 方言的 `tool_calls[].function.arguments`（占载荷 21.3%）可退休为 path+bytes+sha1 |
| Responses / Claude 入口 | 不适用 | 我们 `/v1/responses` 整链 skipped、Claude `tool_result` 零处理 |

---

## 4. LRC 方案（分层可逆压缩）

### 4.1 分层

```
S0 分桶：system | tools | 历史 tool_call 参数 | 历史 tool 结果 | 历史 assistant(text/reasoning) | 末条 user | 附件
S1 保护：围栏/注解/diff/patch/路径/URL/行号/报错栈/命令本身 → tombstone，还原率必须 100%
S2 无损：ANSI、CR 进度帧、空白归一、schema 噪声键、行号前缀剥离、never-worse
S3 有损可逆（按桶）：
   B1 tools schema 压实 / 未用命名空间剪枝
   B2 历史 reasoning：仅留最近 1 轮（provider 能力位门控）
   B3 历史 tool_call 参数：→ path + bytes + sha1 前缀（文件在盘上，命令可重跑）
   B4 历史 tool 结果：按工具能力分档（bash 过滤器 60–85%；read 永不压，仅去重/指针）
   B5 历史 assistant 文本：首尾句 + 实体
   B6 同构 JSON / lockfile / base64 专用器
S4 门禁：结构不变量校验，失败回退该块原文（fail-open）
S5 记账：分桶 token 账本 + 分阶段 saved + 回捞次数 + usage 回填
```

### 4.2 分桶策略与实测目标

| 桶 | 占载荷 | 策略 | 目标压缩率 | 可逆路径 |
|---|---|---|---|---|
| B2 reasoning | 26.5% | 仅留最近 1 轮；更早删除（provider 门控） | 85–100% | 无信息量 |
| B3 tool_call 参数 | 21.3% | 老参数退休为指针 | 80–95% | 文件在盘 / 命令可重跑 |
| B4 tool 结果 | 29.0%（bash 22.2 + read 4.9 + 其他） | 过滤器 + 中龄摘要 + 老龄指针；read 只去重 | 40–60%（保全关键片段） | 原文落盘 + 展开端点 |
| B1 tools schema | 9.6% | 噪声键 8.2% / 嵌套描述 17.6% / 顶层截断 32.3%；命名空间剪枝 79% | 32–43%（保守）/ 79%（激进） | 参数名与 required 不变 |
| B5 assistant 文本 | 2.6% | 首尾句 + 实体 | 50–70% | — |
| 注入类 / system | 4.0% | system 恒不动；重复快照去重 | 仅重复部分 | — |
| 末条 user / 当步结果 | 9.6% | **恒不动**（仅无损规范化） | 0 | — |

### 4.3 原型实测（30 会话 / 1,372 步 / 2.147 亿 token，chars/4）

| 层/档 | 加权节省 | 中位 |
|---|---|---|
| S4f 工具输出过滤器（全量） | 26.7% | 24.7% |
| S3B 中龄结构化摘要 | 34.3% | 32.0% |
| S3C 老龄指针化 | 46.7% | 41.6% |
| S1 tools schema 压实（spec） | 7.4% | 9.4% |
| S2 精确去重 | 0.2% | 0.0% |
| **保守（S1+S2+S4 轻）** | **13.0%** | 14.3% |
| **中性（+S3B）** | **41.8%** | 43.0% |
| **激进（+S3C）** | **54.3%** | 51.8% |
| 激进·无门禁（对照） | 59.9% | 60.0% |
| 极限（+reasoning 删除+schema 剪枝+纯指针） | *见 [实测报告 §4.1](lrc-benchmark-2026-10-22.md#4-lrc-各档实测)* | — |

门禁（S5）在三档下全部通过：保护片段还原率 100%、tool_call↔result 配对不变、压缩后 JSON 可解析、
「后续步骤引用过的内容整段消失」命中 0。激进档相对无门禁档只少 5.6pp 节省，说明**门禁的代价很小**。

---

## 5. 为什么 60–90% 可达，以及 80% 以上要付什么

### 5.1 算式桥（以 M1 加权计）

```
可压总量 = 老内容（age≥3，90.4%）
         = reasoning 26.5% × r1 + args 21.3% × r2 + tool结果 29.0% × r3
         + schema 9.6% × r4 + assistant 2.6% × r5 + 注入 3.1% × r6
中性档取 r = (0.5, 0.8, 0.45, 0.4, 0.6, 0.3) → 13.3+17.0+13.1+3.8+1.6+0.9 ≈ 49%（含当步不可压部分后实测 41.8%）
激进档取 r = (0.95, 0.9, 0.6, 0.43, 0.7, 0.5) → 25.2+19.2+17.4+4.1+1.8+1.6 ≈ 69%（实测 54.3%，差额来自 tier A/B 保真约束）
极限档（r3→0.8，schema 剪枝 0.79）→ 约 75–82%
```

### 5.2 三档风险对照

| 档 | 实测/推算节省 | 破坏率 | 回捞需求 | 需要谁配合 |
|---|---|---|---|---|
| 保守（无损+轻过滤器） | 13% | ≈0 | 无 | 网关独立可做 |
| 中性（+摘要） | 42% | <1%（门禁后） | 低（摘要保留关键片段） | 网关独立 |
| 激进（+指针化+reasoning 删除） | 54%（实测）/ ≈65%（含 schema 剪枝） | <1% | 中（原文落盘+展开端点） | 网关独立（read 除外） |
| 极限（纯指针+schema 命名空间剪枝） | 75–85% | 需 A/B 验证 | 高 | 需要客户端配合（按需工具集） |

**80–90% 需要牺牲什么**：① 历史 reasoning 全部删除（对回放型 provider 有效，占 26.5%）；
② tool 结果退化为纯指针，模型想看细节必须回捞（每次回捞 = 一轮上游往返，成本要计入）；
③ tools schema 按命名空间剪枝（未使用工具在模型看来"不存在"，需客户端按需装配或网关做"缺失工具回填"）。

### 5.3 与缓存的关系（最容易被忽略的坑）

- 实测 87%–99% 的 prompt token 是 cache read，稳定前缀 99.69% → **现状本来就在吃缓存红利**。
- 因此压缩必须是**块内在、确定性、append-only** 的渲染：同一块在任何一轮都渲染成同一字节，
  否则前缀每次变化 → 缓存全失效 → 成本按 1.0x 重算，**压缩赚 20%，缓存亏 5–10 倍**。
- 反过来，只要渲染恒定，压缩后的前缀同样被缓存，节省按比例落到账单上（不是只落到"字节数"）。

---

## 6. 不破坏会话的 10 条硬门禁

1. 末条 user 逐字不动（ultra 当前违例）。
2. system 逐字不动（aggressive 降级链当前违例）。
3. `tool_call ↔ tool_result` 配对不变：callId 集合/顺序/条数不变；`old_string` 类锚点永不触碰。
4. 结构不变量：压缩后 JSON 可解析；工具参数名集合与 required 集合不变；标记里的行数/字节数必须真实。
5. 保护片段还原率 100%（围栏、diff hunk、补丁、路径、URL、行号、报错栈、
   `panicked at`/`segfault`/`assertion failed`/`exit code N`/中文提示）。
6. never-worse：压缩后不降反升 → 回退原文；< 512 token 块不动。
7. 幂等 + 确定性：带 marker 不再压；渲染不得依赖逐轮变量（否则毁缓存）。
8. 能力位门控：Anthropic 签名块、requires-thinking provider、图像/PDF parts 不压。
9. 引用感知：后续步骤引用过的内容（文件名/行号/错误串）不得整段消失。
10. 可回捞：有损块必须能取回原文（落盘 + `/v1/compression/expand`；取回 ≥2 次即改回原样）。

---

## 7. 计量与灰度

现状问题：`chars/4` 三分口径并存（对真值低估 30%）；窗口判定只算 `content`（漏 tools/args/reasoning，
导致 fit 通过却上游 400，本机 journal 有实证）；收益用整包差分（分母含 tools）；失败请求也计 `tokens_saved`。

改法：
1. 分桶账本 + 上游 `usage` 回填校准（含 `prompt_cache_hit_tokens` / `cache_read_input_tokens`）；
2. 收益 = Σ 分阶段 saved − 回捞成本（expand 次数 × 平均往返）− 缓存失效成本；
3. 失败请求不计收益，`savedRatioPct` 分母用原始 prompt；
4. dashboard 增「Compression」分桶页：逐桶 before/after、逐方法命中数、回捞率、缓存命中率。

灰度：`[compression] profile = off|lite|balanced|aggressive|extreme`，按 API key / 模型 / 请求头灰度；
上线前用真实会话重放跑 A/B（成功率、工具调用错误率、任务完成率、平均轮数）。

---

## 8. 实施路线

| 阶段 | 内容 | 风险 | 收益 | 工作量 |
|---|---|---|---|---|
| P0 | 分桶账本 + 估算器校准 + 幂等 marker + 末条 user/system/配对门禁 | 极低 | 0（可验收前提） | 1–2 天 |
| P1 | S2 无损层（ANSI/CR/空白/schema 噪声键/注入去重）+ never-worse | 低 | 5–10% | 2–3 天 |
| P2 | B2 reasoning 裁剪（provider 门控）+ B3 参数退休 + B4 bash 过滤器（rtk 移植） | 中 | 25–40% | 1 周 |
| P3 | 落盘指针 + expand 端点 + pin 反悔 + read 去重 + 同构 JSON/lockfile 专用器 | 中 | +15–25% | 1–2 周 |
| P4 | tools schema 剪枝（客户端配合）+ Responses/Claude 入口 + 分桶 dashboard | 中高 | +5–10% | 1 周 |

---

## 9. 反模式（实测/审计得出）

1. **uniform 压缩**：对 system 与末条 user 一视同仁（ultra 剪指令、aggressive 改系统提示）→ 指令失真。
2. **无会话/无位置概念**：只按"消息文本"压，历史 reasoning / tool_call 参数 / schema 永远碰不到。
3. **无幂等**：同一块被多引擎反复压 → 二次损坏（现有 `[COMPRESSED:` 守卫只在 aggressive/ultra 有）。
4. **破坏 diff/JSON 可解析性**：lite 按字符截断 JSON、caveman 删 diff 行里的虚词 → 修错文件。
5. **逐轮变量驱动压缩**：渲染随轮变化 → 缓存全失效（rtk 的 E010 就是为这条设的）。
6. **只报 tokens_saved 不校验**：省了 token 却引发返工，实测现有档破坏率 20–72%。

---

## 10. 存疑与来源

| 项 | 说明 |
|---|---|
| reasoning 回放判定 | 由「真实 usage vs 重建」间接推断，非 wire 抓包；正确性依赖重建模型（R² 0.98） |
| 极限档 80–90% | 需 A/B 质量验证；原型门禁通过 ≠ 任务质量无损 |
| chars/4 绝对值 | 系统性低估 30–35%，所有"token 数"应视为量级；比例结论同口径下有效 |
| rtk 的 60–90% | 上游自报、无端到端实测；且仅指 bash 输出字节，不是账单 |
| 缓存命中率 | 来自 provider `usage` 字段，不同 provider 语义略有差异（已按字段名分别取） |

来源等级：① 本机实测（语料/原型/基线）——A；② 本仓库源码审计——A；
③ rtk 上游源码与文档（快照 commit 5ab62913）——B（未执行其二进制）；
④ 推断/推算——C（文中已标注"推算"）。
