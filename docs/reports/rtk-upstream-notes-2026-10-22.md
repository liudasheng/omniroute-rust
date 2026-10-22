# 上游情报：rtk-ai/rtk 压缩架构剖析（对网关的借鉴清单）

> 日期：2026-10-22　｜　用途：为 [`docs/LRC.md`](../LRC.md) 的设计取舍提供外部依据
> 关系：LRC 借鉴 rtk 的**方法论与门禁**，不复制其实现；口径不同，不可直接比数字。

---

## 0. 必须先钉死的前提（否则「60–90%」会被误读）

| 事实 | 含义 |
|---|---|
| rtk 的百分比是 **bash 输出字节**的削减比例，**不是 token 账单比例** | 拿它的 % 推断网关账单降幅是错的 |
| 稀释链路（上游自述）：bash 输出 → 输入 token 的一部分 → 输入 token 只是账单的一部分（还有输出 token） | 同样的链路在网关侧更长，折扣更深 |
| CLI 侧 token 全部为 `ceil(bytes/4)` 估算，**不携带 tokenizer** | 百分比可信（同一估算器作用于前后），绝对 token 数只是量级 |
| proxy 侧改用 tiktoken `o200k_base` 做阈值判定，但权威计费数字仍取上游 `usage` | 网关侧应采同一纪律：判定用估算，报账用 usage |

> **对网关的意义**：拿 rtk 的数字做目标，只能拿「同类命令、同口径字节比」比；
> 不能拿它的 % 直接推断本网关的账单降幅。

---

## 1. 架构：两条互相独立的通路

| 通路 | 干的事 | 载体 | 拦截点 |
|---|---|---|---|
| **A. Hook 命令重写 + 输出过滤** | `git status` → `rtk git status`，压缩 **bash 输出** | CLI（Apache-2.0）+ engine（BUSL-1.1） | agent 的 PreToolUse / plugin 事件 |
| **B. Proxy 请求体压缩** | 拦截 agent→provider 的 **HTTP 请求体**，改 tool_result / tool_use / 图像 / 缓存断点 | proxy（BUSL-1.1） | agent 的 `ANTHROPIC_BASE_URL` |

两者刻意分成两个二进制、两套许可证、两个进程，可单用可并用。
**网关（omniroute-rust）的位置相当于 B，但做得更宽**：B 只做 Anthropic 语义的 Write/Edit 退休，
本网关须覆盖 OpenAI / Claude / Gemini 三方言的入口与响应。

---

## 2. 可直接借鉴的机制（已落入 LRC）

| 机制 | rtk 做法 | LRC 对应 |
|---|---|---|
| CCR 可逆压缩 | 预览 + 哈希 marker + 原文 store + 取回 ≥2 次即 pin（改回原样） | `DumpStore` 落盘 + `retire_args` 指针化 + 门禁 10「可回捞」 |
| 按工具能力分档 | Read **永不压**；Bash 300 tok 起压 | `filter.rs` 的 `FilterMode`（Light/默认）+ read 走「绝不压」 |
| never-worse 双门 | 压后估算 ≥ 压前 → 回退原件 | 门禁 6；`< 512` 字节的块不动 |
| 幂等 marker | 已带 marker 的块不再压；跨两个压缩点靠 stamp 识别 | 门禁 7；marker 标记 |
| 缓存稳定性宪法 | 含逐轮变量（时间戳、计数）的规则拒绝加载 | 门禁 7：结果不依赖 `message_count`/`tokens_in` |
| 分阶段记账 | 按层记 saved | `Ledger` 分桶账本 |
| shape 分类 → 多策略 | RLE / structural / json-skeleton / head-tail | `schema.rs`（Spec/Stub/Prune）+ `filter.rs`（head/tail） |
| 诚实标注 | 所有有损 method 必写 marker，proxy 见 marker 即跳过 | 同 |

---

## 3. 绝不压缩 / 绝不改写（白纸黑字）

| 对象 | 规则 | 理由 |
|---|---|---|
| **`Read` 的 tool_result** | `read_min_tokens()` 默认 `usize::MAX` = 永不压 | head+tail 挖掉中间后 agent 会翻页读回，恢复成本随轮次涨得比单轮节省快 |
| **`Read` 出现在 Edit 链上** | 若输出被格式转换过，追加 `additionalContext` 警告「重读后再构造 old_string」 | 锚点错位会导致改写失败与返工 |
| **`old_string` 等编辑锚点** | `field.starts_with("old_")` → Anchor，永不触碰 | 锚点丢了必然返工 |
| **未识别的 agent / 工具 / 字段** | default-deny（Opaque），宁少赚不破坏 | 未知即不可知 |
| **`git diff` / `git show`** | 显式 `passthrough`，diff 必须逐字节到 agent | diff 是模型判断的依据 |
| **grep 命中内容** | 不放 `ansi`/`base64-elide`：「grep 命中的字节是真实文件内容，改写等于伪造文件」 | 内容保真 > 节省 |
| **grep 上下文行（`-C`）** | 任一行解析不成 `path:line:content` → 整块 passthrough | 格式不确定即整块放弃 |
| **`.md` 文件的 Read** | `md-slim` 只用于 web 链，不进 Read 链 | Read 出来的 md 是编辑锚点内容 |
| **图像 / 混合 MCP 响应** | 含图像、嵌入资源、混合数组一律原样 | 视觉能力未知时不压（门禁 8） |
| **写重定向的 `cat/head/tail`** | 判为写而非读 → 不改写 | 写路径内容须完整 |
| **`gh --json/--jq/--template`** | 显式放弃重写 | 输出形状已被显式指定 |

---

## 4. 三层守卫

| 守卫 | 语义 |
|---|---|
| **never-worse（CLI 侧）** | 过滤后估算 ≥ 原件 → 输出原件；两个例外白名单（CRLF-only diff 提示、干净跑批摘要） |
| **never-worse（引擎链）** | 输出 == 输入视为无效、输出变大视为无效，均回退 |
| **pipeline `[guards]`** | `never_worse = true`、`min_size = 512`（小于 512 的块不动，省不了还添 marker） |
| **panic 隔离** | `catch_unwind`：任何 panic → 当作没压，原件照发 |
| **fail-open hook** | 任何错误不写 stdout、`exit 0`；管线文件坏了退回「不压」，只写状态文件 |
| **幂等守卫** | 已带 marker 的块不再压；跨压缩点靠 stamp 识别，避免二次伤害 |
| **诚实标注** | 所有有损块必写 marker（`[xN]`、`[N lines elided]`、`[cap a→b tokens · recall <h>]`），见 marker 即跳过 |

LRC 对应实现：门禁 6（never-worse + 阈值）、门禁 7（幂等）、
S4 门禁层（结构不变量校验失败即回退该块原文）、S5 账本层。

---

## 5. 如何证明「压缩后信息未丢」

| 手段 | rtk 做法 | LRC 对应 |
|---|---|---|
| **忠实性差分测试** | 同参数下 rtk 与原生工具 stdout/exit code 逐字节一致 | `tests/integration.rs` 中的结构不变量断言 |
| **fixture 必须是真实输出** | 「用真实命令输出建 fixture，不用合成数据」 | 门禁用例基于真实语料形态 |
| **每 filter ≥20% 降幅断言** | 测试内用同一估算器算降幅并断言下限 | 各档节省率实测（见 benchmark 报告） |
| **recall 反向可复原件级验证** | 每个截断点可回捞，且统计「真回捞率」 | `DumpStore` 原文落盘 + 回捞计数 |
| **决策审计日志** | 每次压缩决策记一行：shape / 策略 / 省略范围 / tokens before/after | `Ledger` 分桶账本 + `Outcome` |

---

## 6. rtk 未做而 LRC 能做的

1. **历史 reasoning 裁剪**：Anthropic 方言必须保留签名块，OpenAI 方言不必；rtk-proxy 不碰 reasoning。
2. **tools schema 压实**：rtk 无对应物；本项占真实请求 9.6%–10.6%。
3. **tool_call 参数退休**：rtk-proxy 只做 Anthropic 的 Write/Edit，本网关须覆盖 bash/read/edit/write 全类。
4. **三方言入口**：OpenAI / Claude / Gemini 的请求体形状与响应形状都要能进出同一流水线。
5. **分桶记账与 dashboard 分桶报表**：rtk 只给单次决策日志，不给跨请求的分桶趋势。

---

## 7. 口径与边界提醒

- rtk 的 60–90% 是 **bash 输出字节**，不是账单；它自己也写明稀释链，且从不宣称端到端降幅。
- CLI 侧的供应链门禁（filter 文件 trust、hook 脚本完整性校验）在网关侧**无对应物**：
  网关不执行用户提供的过滤器，信任边界只有「管理员可改配置」。
- rtk 的许可证分层（CLI Apache-2.0 / engine BUSL-1.1）意味着**不可直接搬用其 engine 代码**；
  本项目只借鉴方法论，实现自写（`src/compression/lrc/`）。
