# 运行时数据可得性实测（2026-10-17 21:05–21:20 CST）

> 执行者：lead（原探针员 live-prober 两度中途失败，未产出交付物，任务改由 lead 承接）。
> 全部只读：未改 `src/`、未改任何配置、未改 combos.json、未写 systemd unit。密钥一律不回显明文。

## 一、本机网关（http://127.0.0.1:20128）

| 端点 | 鉴权 | 结果 |
|---|---|---|
| `GET /healthz` | 无 | `ok` |
| `GET /v1/models` | 需 API key | 无 key → `401 invalid_api_key`；带 `OMNIROUTE_RUST_API_KEY` → **128 个模型**（opencode-go 22 个可见） |
| `GET /v1/quotas` | 管理面 | `401 management access denied` |
| `GET /v1/opencode-usage` | 管理面 | `401 management access denied` |
| `GET /v1/opencode-free` | 管理面 | `401 management access denied` |
| `GET /v1/model-catalog` | 管理面 | `401 management access denied` |
| `GET /v1/combo-health` | 管理面 | `401 management access denied` |

说明：`api-keys.json` 内两把 key 的 `role` 均为 `default`，非管理角色，故管理面一律 401（与项目事实 F7 一致）。
管理面须 dashboard 会话（`~/.omniroute-rust/dashboard-auth.json` 仅存 salt + password_sha256，不可逆推），本次未登录。

**价格暴露判定**：`/v1/models` 的模型对象字段为
`api / compat / contextLength / contextWindow / free / freeLane / id / input / maxOutputTokens / maxTokens / modalities / name / provider / reasoning / reasoningEfforts / supportsPdf / supportsReasoning / supportsThinking / supportsVision / thinkingLevels`
—— **无任何价格或额度字段**（全库 128 个模型逐项扫描，`price`/`cost` 命中 0）。

## 二、缓存目录：价格「抓到即丢」的实证

`~/.omniroute-rust/model-catalog.json`（1,629,054 B，`fetched_at_ms=1792231488439`，8,828 行）：

- 每行字段：`provider / model / context_window / max_output_tokens / modalities / reasoning / free / deprecated`
- opencode-go 命中 32 行，**无 cost 字段**——只有 `free` 布尔。

对照上游 `https://models.dev/api.json`（5,756,603 B，232 provider）中 `opencode-go.deepseek-v4.1-flash`：

```json
"cost": { "input": 0.15, "output": 0.6, "cache_read": 0.003 }
"limit": { "context": 1000000, "output": 384000 }
```

即：单价确实随快照抵达本机，但落盘时被裁剪，仅保留「是否免费」判定结果
（`src/model_catalog.rs:363-371` `cost_free()`）。**这是价格进不了路由的第一道断点。**

## 三、上游额度端点实测

用 `~/.omniroute-rust/provider-connections.json` 中 `opencode-go` 连接的 key（指纹 `sk-HBT…AAli`，67 字符）：

| URL | 状态 | 响应 |
|---|---|---|
| `GET https://opencode.ai/inference/go/v1/usage` | 200 | 见下 |
| `GET https://opencode.ai/zen/go/v1/usage`（旧别名） | 200 | 与上完全相同 |

```json
{"usage":{
  "rolling":{"status":"ok","percent":0,"resetsAt":"2026-10-17T14:19:20.000Z"},
  "weekly": {"status":"ok","percent":62,"resetsAt":"2026-10-19T00:00:00.000Z"},
  "monthly":{"status":"ok","percent":31,"resetsAt":"2026-11-15T07:26:22.000Z"}}}
```

换算（Asia/Shanghai）：5 小时窗 0%、22:19 重置；周窗 62%、10-19 08:00 重置；月窗 31%、11-15 15:26 重置。

**关键限制**：该端点是**整档**百分比，**不区分模型**。而官方额度规则是「每模型各自的月额度美元」
（GLM-5.3-Flash $60、DeepSeek V4.1 Flash $80、Kimi K3 $15…）。故「某模型还剩多少额度」在运行时**无法直接取得**，
只能由本地按 token 用量 × 该模型单价自行累计。

## 四、组合现状（`~/.omniroute-rust/combos.json`，2026-10-17 21:12 读取）

| 组合 | strategy | 候选序 |
|---|---|---|
| coding-gpt | priority | `openai-compatible-dev-one-blog-top/gpt-5.6-luna` → `…/gpt-5.6-terra` |
| coding-free | priority | `auto/best-free` → `openrouter/openrouter/free` |
| coding-flash | priority | `opencode-go/deepseek-v4.1-flash` → `opencode-go/space-bunny` → `opencode-go/glm-5.3-flash` → `openai-compatible-dev-one-blog-top/gpt-5.6-luna` → `auto/best-free` |
| coding-plan | priority | `opencode-go/deepseek-v4.1-flash` → `opencode-go/space-bunny` → `qwen-cloud-token-plan/deepseek-v4.1-flash` → `auto/best-free` |

四个组合**全部是 `priority`**，即路由次序完全由人工声明序决定，与价格/额度无关。
（`/v1/combo-health` 的候选冷却明细需管理会话，本次未取。）

`opencode-go` 连接的 `hidden_models`（用户已隐藏，组合与 auto/* 均会排除）含
`mimo-v2.5`、`mimo-v2.5-pro`、`hy3`、`minimax-m3`、`qwen3.8-flash`、`deepseek-v4-pro`、`glm-5.2`、
`muse-spark-1.2/1.3-contributor`、`grok-4.5/4.6` 等；可见的性价比候选为
`deepseek-v4.1-flash`、`mimo-v2.6-flash`、`longcat-2.0`、`glm-5.3-flash`、`space-bunny`、`step-5-preview`、`glm-5.3`、`kimi-k3`。

## 五、结论：按「输入价 / 输出价 / 剩余月额度」排序，运行时拿得到什么

| 需要的数据 | 运行时可得？ | 来源 / 缺口 |
|---|---|---|
| 模型单价（input/output/cache_read） | **半可得** | models.dev 快照已含，但 `model_catalog.json` 落盘时被裁掉；须改 `model_catalog.rs` 保留，或本地静态表 |
| 每模型月额度（美元） | **不可得** | models.dev 无此字段；usage 端点只给整档百分比 → 只能本地静态表（源自官方文档表 A/B） |
| 已用额度百分比 | **可得** | `GET /inference/go/v1/usage`（rolling/weekly/monthly + resetsAt），但**整档、非每模型** |
| 峰谷时钟 | **可得（本地推算）** | DeepSeek Peak = 周一至周五 01:00–04:00 / 06:00–10:00 UTC；无需上游接口，本地时钟即可判定 |
| 请求 token 量（用于估算本次成本） | **可得** | `RequestFit { prompt_tokens, output_tokens }`（`src/router/combo.rs:114-118`） |
| 单次实际用量（用于累计额度消耗） | **可得** | 上游响应 usage 字段（`chat_core` 已解析计数） |

一句话：**四要素中「单价」差一步（裁剪掉了）、「每模型月额度」差一张本地表、「整档百分比」与「峰谷时钟」已经具备。**
