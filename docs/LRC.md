# LRC — Layered Reversible Compression（分层可逆压缩）

> 目标：在不破坏会话（不丢指令、不破坏工具调用、不毁 JSON/代码/diff、不引发返工）的前提下，
> 把 agent 会话的**输入 token** 降低 60%–90%。
>
> 实现位置：`src/compression/lrc/`（`mod.rs` 流水线 · `gates.rs` 门禁与估算 · `filter.rs` 工具结果过滤器 · `schema.rs` tools schema 压实）
> 实测数据：[`docs/reports/lrc-benchmark-2026-10-22.md`](reports/lrc-benchmark-2026-10-22.md)
> 策略总纲：[`docs/reports/compression-strategy-2026-10-22.md`](reports/compression-strategy-2026-10-22.md)

---

## 0. 为什么 60–90% 可达（结构性判据）

单请求 prompt 中 **89.1% 的 token 来自 3 步以前**，79.7% 来自 50 步以前；最近 3 步只占 10.9%。
agent 请求的绝大部分体积是**历史**，而历史有三类「更廉价的充分表示」：

1. **已消散的信息**：历史 reasoning、历史 assistant 旁白——不影响后续决策，可丢；
2. **可从外部重建的信息**：历史 tool_call 参数（文件已落盘）、历史 tool 结果（命令可重跑 / 原文可落盘）；
3. **恒定不变的信息**：tools schema（每请求全量重发，实测可达约 4 万 token）、重复注入的 system-reminder。

因此 60–90% 不靠「把文字压得更短」，而靠**换表示**：原文 → 摘要 → 指针（可回捞）。
单靠文本级规则（既有 lite/caveman/ultra/rtk）在真实 agent 载荷上只能做到个位数到十几个百分点，
因为它们只动 `messages[].content` 里的字符串，而这三块恒定/历史大头根本不在那个字段里。

---

## 1. 目标形态：一次请求的分桶流水线

```
请求体
  │
  ├─ S0 分桶（Bucketizer）：system | tools | 历史 tool_call 参数 | 历史 tool 结果
  │                        | 历史 assistant（text/reasoning） | 末条 user | 附件
  │
  ├─ S1 保护层（Preserve）：注解/围栏/diff/patch/路径/URL/行号/报错栈/命令本身/JSON 键值
  │                        → 打 tombstone，压缩只作用于非保护段，还原率必须 100%
  │
  ├─ S2 无损层（Lossless）：ANSI 剥离、CR 进度帧折叠、空白归一、注入块精确去重、
  │                        tools schema 噪声键剥离、行号前缀剥离（不改语义）
  │
  ├─ S3 有损可逆层（Lossy+Reversible）：
  │     B1 tools schema 压实（嵌套描述丢弃 / 顶层描述截断 / 未用命名空间剪枝）
  │     B2 历史 reasoning 裁剪（保留最近 1 轮；provider 能力位决定是否保留签名块）
  │     B3 历史 tool_call 参数退休（write/edit → path+bytes+sha1 前缀；bash → 首行+行数）
  │     B4 历史 tool 结果换表示（按工具能力分档：bash 走过滤器；read 走「绝不压 + 指针」）
  │     B5 历史 assistant 文本摘要（首尾句 + 关键实体）
  │     B6 同构大 JSON / lockfile / base64 专用压缩器
  │     所有有损项都带 marker + 落盘指针（原文存 <data_dir>/compression/<sess>/<hash>）
  │
  ├─ S4 门禁层（Gate, fail-open）：结构不变量校验 → 失败回退该块原文
  │
  └─ S5 记账层（Ledger）：分桶 token 账本 + 分阶段 saved + 回捞次数 + usage 回填
```

### 分桶策略表（实测占比 → 目标压缩率）

| 桶 | 实测占比 | 既有处理 | LRC 策略 | 目标压缩率 | 可逆路径 |
|---|---|---|---|---|---|
| B2 历史 reasoning | 26–30% | 0（全部回放） | 仅保留最近 1 轮，更早删除；Anthropic 签名块与 `requiresThinking` provider 保留 | 90–100% | 丢弃即无信息量 |
| B3 历史 tool_call 参数 | 21–22%（bash 17.0 / edit 2.3 / write 2.1） | 0 | 老参数退休为指针（文件已在盘上；命令可重跑） | 80–95% | 文件/shell 历史 |
| B4 历史 tool 结果 | 29%（bash 20.6 / read 5.7 / job_output 2.7） | lite 截 2000 字、rtk 只压 tool 角色且 120 行 | 按工具能力分档：bash 类过滤器 60–85%；read **永不压**（rtk 的结论：翻页读回成本更高）只用去重/指针；结构化输出（json/grep/test）专用压缩器 | 60–85% | 原文落盘 + 展开端点 |
| B1 tools schema | 9.6–10.2% | 0 | 噪声键剥离 + 嵌套描述丢弃 + 顶层描述截断；未用命名空间剪枝（需客户端配合/阈值回填） | 32–43%（保守），79%（激进） | 结构等价校验，参数名/required 不变 |
| B5 历史 assistant 文本 | 2.7% | aggressive 阈值以上摘要 | 首尾句 + 实体；更早压成一句 | 50–70% | — |
| B6 system / 注入块 | 1.7–3.7% + 注入重复 | lite 的 system 去重默认被关 | **system 恒不动**；重复注入块（AGENTS.md/skills/runtime-context）精确去重，首份保留 | 仅重复部分 | — |
| B7 末条 user / 当步 tool 结果 | 10.9%（年龄 0–2 步） | 全部原样（正解） | **恒不动**；仅做无损规范化 | 0 | — |

---

## 2. 不破坏会话的 10 条硬门禁（fail-open，每条可自动化）

1. **末条 user 消息逐字不动**（ultra 既有实现违例）。
2. **system 消息逐字不动**；aggressive 降级链硬编码含 system 的缺陷必须删（既有实现违例）。
3. **tool_call ↔ tool_result 配对不变**：callId 集合、顺序、条数不变；`old_string` 类锚点字段永不触碰。
4. **结构不变量**：压缩后 JSON 必须可解析；参数名集合/required 集合不变；行数/字节数标记必须与实际一致。
5. **保护片段还原率 100%**：围栏代码、diff hunk（`@@`/`+`/`-`）、补丁、路径、URL、行号引用、报错栈、
   `panicked at`/`segmentation fault`/`assertion failed`/`exit code N`/中文提示语。
6. **never-worse**：任何块压缩后 token 不降反升 → 回退原文；低于阈值（默认 48 token）的块不动
   （marker 比省下的还贵）。
7. **幂等/确定性**：同输入同输出；带 marker 的块不再压；压缩结果不依赖 `message_count`/`tokens_in`
   等逐轮变量（否则每轮前缀都变，prompt cache 全失效——缓存读 0.1x vs 冷 1.0x）。
8. **能力位门控**：Anthropic thinking 签名块、`requiresThinking` provider、图像/PDF parts、
   视觉能力未知时不压。
9. **引用感知**：后续步骤引用过的内容（文件名、行号、错误串）不得整段消失；B 档摘要必须保留这些行。
10. **可回捞**：任何有损块必须能取回原文（落盘 + 展开端点；
    本地部署时模型可用 `read` 直接读落盘路径）。取回 ≥2 次的块永久改回原样（rtk 的 pin 反悔机制）。

---

## 3. 计量（先能测准，再谈 60–90%）

既有实现的三分口径问题：`chars/4` 三处并存；窗口判定只算 `content`（漏 tools/tool_args/reasoning）；
收益用整包差分（分母含 tools，稀释）；失败请求也计 `tokens_saved`。
实测：`chars/4` 对真实 `usage.prompt_tokens` 的中位相对误差 **−19.7%**，中文密集内容偏差可达 2–3 倍（R² 为负）。

改法：

- 分桶账本（system / tools / tool_args / reasoning / tool_result / assistant / user / attachments）；
- 估算器按内容类型（ASCII/CJK/JSON/base64）分档的 chars→tokens 系数，并用上游 `usage` 在线校准；
- 收益 = 分阶段 saved（S2/S3 各桶）+ 回捞成本（`expand_calls`）− 缓存失效成本；
- 失败请求不计收益；`savedRatioPct` 分母用「原始 prompt」。

---

## 4. 实现现状（`src/compression/lrc/`）

### 4.1 档位（Profile）

| 档 | schema | reasoning | 参数退休 | 过滤器 | 去重 | assistant 文本 | 最近保留轮次 |
|---|---|---|---|---|---|---|---|
| `conservative` | Spec | Off | 否 | Light | 否 | 否 | 3 |
| `balanced`（默认） | Spec | Auto | 是 | 默认 | 是 | 否 | 3 |
| `aggressive` | Stub | Auto | 是 | 默认 | 是 | 是 | 3 |
| `extreme` | Prune | On | 是 | head 30 / tail 20 | 是 | 是 | 2 |

`SchemaMode`：`Spec`（仅压实嵌套描述与顶层描述）→ `Stub`（未用工具降为 name + 单行描述 + 扁平参数名）
→ `Prune`（未用工具整块剪除，理论上限）。
`TrimReasoning`：`Off` / `Auto`（有签名块则不裁） / `On`。

### 4.2 关键组件

| 文件 | 职责 |
|---|---|
| `mod.rs` | `apply(body, cfg) -> Option<Outcome>` 主流水线；`Profile` / `LrcConfig` / `Ledger` / `Outcome`；`DumpStore` 原文落盘与回捞 |
| `gates.rs` | 保护片段提取（`extract_protected`）、保护存活校验（`protected_survives`）、token 估算（`estimate_tokens_mixed`） |
| `filter.rs` | 工具结果过滤器（`FilterCfg` / `FilterMode`）：bash 输出按 head/tail 行保留、去重、噪声剥离 |
| `schema.rs` | tools schema 压实（Spec / Stub / Prune），保证 name / required / 参数名与类型不变 |

### 4.3 接线

- 压缩模式新增 `lrc`：`CompressionMode::Lrc`，请求头 `x-omniroute-compression: lrc` 即可选中。
- 运行时配置：`GET/POST /v1/compression` 暴露 `lrc_profile`、`lrc_schema`、`lrc_reasoning`、`lrc_filter`、
  `lrc_retire_args`、`lrc_dedupe_results`、`lrc_trim_assistant_text`、`lrc_args_min_tokens`、
  `lrc_text_min_tokens`、`lrc_recent_turns`、`lrc_head_lines`、`lrc_tail_lines`、`lrc_dump`、`lrc_dump_dir`、
  `lrc_dump_max_files`；`lrc_profile` 非法值在写入时被拒并给出错误明细。
- 离线诊断工具：`cargo run --bin lrc_diag -- <messages.json>`，逐块归因「为何被压 / 为何未被压」。
- 原文落盘（`lrc_dump` + `lrc_dump_dir`）供有损块回捞。

---

## 5. 实施路线（最小改动优先，灰度可控）

| 阶段 | 内容 | 风险 | 预期收益 | 状态 |
|---|---|---|---|---|
| P0 | 分桶账本 + 估算器校准 + 幂等 marker + 末条 user/system/配对门禁（纯测量+加固） | 极低 | 0（但让后续可验收） | 已落地（`Ledger`、`gates.rs`） |
| P1 | S2 无损层（ANSI/CR/空白/注入去重 + schema 噪声键 + never-worse） | 低 | 5–10% | 已落地（`schema.rs` Spec、`filter.rs` Light） |
| P2 | B2 reasoning 裁剪 + B3 参数退休 + B4 bash 过滤器 | 中 | 25–40% | 已落地（`TrimReasoning`、`retire_args`、`filter.rs`） |
| P3 | 落盘指针 + 展开端点 + pin 反悔 + read 去重 + 同构 JSON/lockfile 专用器 | 中 | +15–25% | 部分落地（`DumpStore` 已备，展开端点待接） |
| P4 | tools schema 剪枝（需客户端配合）+ Responses/Claude 入口支持 + dashboard 分桶报表 | 中高 | +5–10% | 部分落地（Prune 已有，入口与报表待接） |

灰度：`[compression] profile = off|lite|balanced|aggressive`，按 API key / 模型 / 请求头灰度；
每次上线先跑 A/B（同一批真实会话重放）看成功率、工具调用错误率、任务完成率。

---

## 6. 与 rtk-ai/rtk 的关系

- **直接可搬**：CCR 可逆压缩（预览 + 哈希 marker + 原文 store + 取回 ≥2 次即 pin）、按工具能力分档阈值
  （read = 永不压）、never-worse 双门、幂等 marker、缓存稳定性宪法（逐轮变量规则拒绝加载）、
  分阶段记账、shape 分类 → 4 策略（RLE/structural/json-skeleton/headtail）。
- **搬不了**：命令重写（hook 侧）、注入 `--format json` 再解析、recall 可执行指针（网关侧只能用「注入工具/端点」）。
- **rtk 未做而 LRC 能做**：历史 reasoning 裁剪（Anthropic 方言必须保留签名块，OpenAI 方言不必）、
  tools schema 压实、tool_call 参数退休（rtk-proxy 只做 Anthropic 的 Write/Edit）、Responses/Claude 入口。
- **口径提醒**：rtk 的 60–90% 是 **bash 输出字节**，不是账单；它自己也写明稀释链，且从不宣称端到端降幅。

---

## 7. 相关文档

| 文档 | 内容 |
|---|---|
| [`docs/reports/lrc-benchmark-2026-10-22.md`](reports/lrc-benchmark-2026-10-22.md) | 真实语料实测：各档节省率、门禁通过率、语料构成 |
| [`docs/reports/compression-strategy-2026-10-22.md`](reports/compression-strategy-2026-10-22.md) | 压缩策略总纲：对标 rtk、既有六档体检、上限论证 |
| [`docs/BENCHMARK.md`](BENCHMARK.md) / [`docs/zh/BENCHMARK.md`](zh/BENCHMARK.md) | 网关吞吐/延迟基准 |
| [`docs/PARITY.md`](PARITY.md) | 与上游 OmniRoute 的功能对等表 |
