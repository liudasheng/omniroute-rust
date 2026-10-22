# 压缩子系统现状审计（事实底座）

> 日期：2026-10-22　｜　对象：`src/compression/`（六档既有引擎）
> 后续：[`docs/LRC.md`](../LRC.md)（补缺方案）｜[`lrc-benchmark-2026-10-22.md`](lrc-benchmark-2026-10-22.md)（实测）
> 方法：静态审读代码路径 + 实跑复算；本文不含会话原文、路径或凭据。

---

## ① 模式全集与选择链

### 1.1 模式枚举

| 项 | 值 |
|---|---|
| 枚举 | `Off / Lite / Standard / Aggressive / Ultra / Rtk`（+ `Lrc`，见 LRC 文档） |
| 解析别名 | `standard` 亦接受 `caveman`；大小写/空白归一 |
| 对外声明 | `GET /v1/compression` 的 `modes` 字段 |
| 互斥性 | 模式之间**互斥**，一次请求只跑一个引擎 |

### 1.2 默认值（opt-in，默认全关）

总开关 `enabled`（默认 false）→ 请求头 `x-omniroute-compression`
（`off|default|lite|standard|aggressive|ultra|rtk`，未知值穿透）→ `auto_trigger_tokens` 自动触发 → `default_mode`。
压缩发生在**候选解析之前**，故节省会同时改善「能否装进窗口」的判定。

### 1.3 运行时可变

`GET/POST /v1/compression` 暴露并可改各档阈值（lite 字符上限、caveman 强度、aggressive 阈值、
ultra 单条上限与最短门槛、rtk 行数上限等）；env 覆盖仅限两项。

---

## ② 作用面矩阵（构造性事实）

| 字段 | lite | standard | aggressive | ultra | rtk |
|---|---|---|---|---|---|
| `messages[].content`（string） | ✅ | ✅（默认仅 user） | ✅ | ✅ | ✅（仅 tool/function） |
| `content` parts 数组的 text | 部分 | ✅ | ❌ | ✅ | ❌ |
| `tool_calls[].function.arguments` | ❌ | ❌ | ❌ | ❌ | ❌ |
| 历史 reasoning / thinking | ❌ | ❌ | ❌ | 部分（破坏性） | ❌ |
| `tools` JSON schema | ❌ | ❌ | ❌ | ❌ | ❌ |
| Claude `tool_result` 块 / 顶层 `system` | ❌ | ❌ | ❌ | ❌ | ❌ |
| `/v1/responses`（input 形状） | 整链 skipped | — | — | — | — |

**两处真缺陷**：
1. `aggressive` 的降级链把 `system` 硬编码进 `compress_roles`，绕过 `preserve_system_prompt`；
2. `ultra` 不保护末条 user（直接剪当前指令）。

---

## ③ 缺口清单（G1–G15）

> 风险等级：**高** = 可能破坏会话（丢指令 / 破坏工具参数 / 破坏 JSON 可解析性 / 触发上游 400）；
> **中** = 局部失真需人工核对；**低** = 语义安全。
> 「潜在节省量级」为结构性推断，非实测。

| ID | 缺口 | 现状证据（缺失即证据） | 风险 | 潜在节省量级 |
|---|---|---|---|---|
| **G1** | 历史 tool 结果重复出现 | 无跨消息去重：lite 只删「相邻且 role 相同、content 完全相同」，rtk 只删单条结果内连续重复行；agent 两次 tool 结果之间必夹 assistant 消息 → 「相邻」永不成立 | 低 | **高**（重复条目整条可省） |
| **G2** | 同一文件反复读取 / 近似读取 | 无内容哈希索引、无段落引用；rtk 的 document-like 守卫反而主动放弃行数限制 | 低-中 | **高**（coding agent 以文件读为主） |
| **G3** | assistant 长回复与 reasoning/thinking | 压缩链只写 `content`；`reasoning_content` 零引用；Claude 风格 thinking blocks（数组 parts）无引擎识别 | 中-高 | **高**（当前可处理比例 ≈ 0） |
| **G4** | `tool_calls[].function.arguments` | 全部引擎只覆写 `content`，无 `arguments` 引用 | 高 | **高**（当前 0 处理） |
| **G5** | `tools` JSON schema 体积 | 结构性不触碰；但估算把 tools 计入分母 | 低-中 | **高且固定**（每轮 10–30k tokens 量级） |
| **G6** | system prompt 重复 | lite 的 `dedup_system_prompt` 在默认 `preserve_system_prompt=true` 下整段跳过 | 低 | 中 |
| **G7** | 多轮同构大 JSON | 仅 aggressive 的 json 压缩器可处理「>2000 字符且可解析」；lite 按字符截断 JSON（破坏可解析性） | 中-高 | **极高（单次事件）** |
| **G8** | 图片 / base64 data URL | 占位分支的触发条件恒不成立 → 默认永不触发；估算按 chars/4 严重高估图片 token | 中 | **高（单次事件）** |
| **G9** | 每轮全量重算、无会话态、无摊还 | `apply()` 每请求调用一次，进程无会话概念；同一历史每轮从头重压；幂等标记因客户端保留原文而永不回流 → 幂等保护实际近乎死代码 | 低（当前） | 不直接产生节省，但**决定跨轮去重/引用化在当前架构下不可实现** |
| **G10** | `/v1/responses` 入站完全逃逸压缩 | 无 `messages` 即 skipped；该入口直接交原始体 | 低 | **该入口 100% 未压** |
| **G11** | Claude `tool_result` 块 / 顶层 `system` | lite/rtk 只认 `role=="tool"` 字符串，caveman/ultra 只处理 `type=="text"` → `tool_result` 分桶零处理 | 低（欠压） | **高** |
| **G12** | 保真门禁不成体系 | 保护模式仅 fence/inline/URL/path/Error/stack 六类；diff 行（`+`/`-`/`@@`）无保护；**无「前后不变量校验 + 失败回退原文」** | **高** | 非节省项，是提高压缩率的前置门禁 |
| **G13** | 末条 user 消息在 ultra 下无保护 | aggressive 显式恢复最后 user，ultra 仅跳过 system | **高** | 非节省项 |
| **G14** | 窗口/用量口径低估 tools 与 tool_args | 消息估算只算 content；用量只认四个键名；收益用整包差分且失败请求也计 saved | 中 | 不直接节省，但无分桶账本则无法验证 60–90% 目标 |
| **G15** | aggressive 降级链绕过 system 保护 / 重压已摘要文本 | 降级链硬编码含 `"system"`；caveman 无幂等守卫 | **高** | 破坏性缺陷 |

**缺口计数 15 条**；其中「完全未处理」的大头为 G1 / G2 / G3 / G4 / G5 / G7 / G8 / G10 / G11。

### 量级汇总（结构性上限，非实测）

| 分桶 | 当前处理率 | 结构性上限 |
|---|---|---|
| tool 结果（历史） | lite 截到 2000 字符；rtk 噪声行 + 120 行 cap；standard 0 | 重复项可至 100% |
| tool 结果（Claude `tool_result` 块） | 0 | 100% 未触达 |
| assistant content | 默认 0；aggressive >2048 tokens 摘要 | 高 |
| reasoning_content / thinking | 0 | 100% 未触达 |
| tool_calls.arguments | 0 | 100% 未触达 |
| tools schema | 0（且计入分母） | 每轮固定大额 |
| 图片 / base64 | 0（占位分支为死代码） | 单次 >90% |
| 同构大 JSON | 仅 aggressive | 单次 >90% |
| Responses / Claude 入口 | 0 / 部分 | 全量 |

---

## ④ 与 LRC 的对应关系

| 缺口 | LRC 对策 | 状态 |
|---|---|---|
| G3 / G4 | B2 reasoning 裁剪、B3 参数退休（`retire_args`） | 已实现 |
| G5 | B1 schema 压实（`schema.rs`：Spec / Stub / Prune） | 已实现 |
| G1 / G2 | 跨历史去重（`dedupe_results`）+ 指针化 | 部分实现 |
| G7 / G8 | 专用压缩器 + 视觉能力门控 | 待接 |
| G9 | 幂等 marker（门禁 7）——但需客户端回写压缩结果才真正生效 | 设计已定，架构待改 |
| G10 / G11 | Responses / Claude 入口支持 | 待接 |
| G12 / G13 / G15 | 门禁层（结构不变量 + 失败回退原文）、末条 user 与 system 恒不动 | 已实现 |
| G14 | 分桶账本 `Ledger` + 按内容类型分档的估算器 | 已实现 |

---

## ⑤ 可回归点

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test                      # 全量（含门禁与保真断言）
cargo run --bin lrc_diag -- <messages.json>   # 逐块归因「为何被压 / 为何未被压」
```

回归判据：任一既有档的节省率与保真指标相对本报告基线漂移超过 5 个百分点，
或任一保护片段还原率低于 100%，即视为回归。
