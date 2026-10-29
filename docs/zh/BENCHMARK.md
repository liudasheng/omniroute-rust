# 内存与并发性能对比评估：omniroute-rust vs 原版（TypeScript）

> 评估对象：原版 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.50（npm 发布包，Next.js standalone 生产构建）与本仓库 Rust 重写版 **v1.1.7**（release 构建）。
> 详细原始数据见本文末尾"原始数据"。英文版：[docs/BENCHMARK.md](../BENCHMARK.md)；结论摘要见 [README_zh.md](../../README_zh.md#性能摘要vs-原版生产栈同机同-mock-upstream)。

## 1. 测试方法

| 项 | 说明 |
|---|---|
| 机器 | 同一台机器（22 逻辑核 / 16 GB，WSL2，Linux 6.18.54，Ubuntu 20.04 glibc 2.31），两网关串行运行、互不干扰 |
| 上游 | 两者共同指向**同一个**高性能 mock upstream（本仓库 `examples/mock_upstream.rs`，axum，瞬时返回 / SSE 6 chunk），排除上游差异 |
| 被测端点 | ① `GET /healthz`（纯网关开销，无上游）② `POST /v1/chat/completions` JSON 非流式 ③ 同端点 SSE 流式 |
| 压力工具 | 本仓库 `examples/loadgen.rs`（tokio+reqwest），并发 16 / 64，每场景 5–6 秒 |
| 内存口径 | 进程树 RSS 之和（CLI launcher + 子进程），每场景压测中途采样 + 空闲 3 次取均值 |
| 限流 | 两侧速率限制**同步调高**（原版经 `/api/resilience` 设 100000 RPM/0ms/128 并发；Rust 版同值经环境变量），避免测的是限流队列而非引擎 |
| 公平性 | 双方使用同一 mock upstream、同一逻辑模型（`mock-model`，provider 前缀按实现不同）、同一压测客户端 |

## 2. 结果总览

> **2026-10-29 重跑**：Rust 版 **v1.1.7**（commit `f9502e0`）对同一原版 npm v3.8.50。
> 基准实例使用独立端口，**绝不占用线上服务端口 20128**；两侧在各自压测前均**全新启动**
> （新数据目录、无历史负载），RSS 自真实空闲基线起算。原始数据：`/tmp/omni-bench-results6/`
> （`*.rss` 为逐场景 RSS 采样，`*_f.json` 为 loadgen 输出）。

### 内存（RSS，进程树合计）

| 状态 | omniroute-rust | 原版（TS） | 倍数 |
|---|---|---|---|
| 空闲（启动后） | **14 MB** | 794 MB | **约 57×** |
| `/healthz` 16 并发 | 14 MB | 987 MB | 约 71× |
| chat JSON 16 并发 | 22 MB | 1.06 GB | 约 48× |
| chat JSON 64 并发 | 32 MB | 1.22 GB | 约 38× |
| SSE 16 并发 | 23 MB | 1.07 GB | 约 47× |
| SSE 64 并发 | 33 MB | 1.22 GB | 约 37× |

峰值：Rust **33 MB**，原版 **1.22 GB**。原版压测后不归还内存——64 并发场景跑完后空闲仍约 1.26 GB。

### 并发吞吐（同一 mock upstream，两侧错误数均为 0）

| 场景 | 并发 | omniroute-rust | 原版 | 倍数 |
|---|---|---|---|---|
| `GET /healthz`（纯网关开销） | 16 | **27,208 rps** | 1,205 rps | 约 23× |
| `GET /healthz`（纯网关开销） | 64 | **50,293 rps** | 1,397 rps | 约 36× |
| chat JSON（代理） | 16 | **8,688 rps** | 56 rps | 约 155× |
| chat JSON（代理） | 64 | **14,293 rps** | 64 rps | 约 223× |
| chat SSE 流式 | 16 | **333 rps** | 56 rps | 约 6.0× |
| chat SSE 流式 | 64 | **1,280 rps** | 64 rps | 约 20× |

### 延迟（p50 / p99，毫秒）

| 场景 | omniroute-rust p50/p99 | 原版 p50/p99 |
|---|---|---|
| healthz c16 | 0 / 0 | 7 / 18 |
| healthz c64 | 0 / 1 | 22 / 49 |
| JSON 代理 c16 | 1 / 2 | 231 / 431 |
| JSON 代理 c64 | 3 / 4 | 835 / 1,070 |
| SSE c16 | 47 / 50 | 227 / 314 |
| SSE c64 | 48 / 51 | 878 / 1,176 |

> 说明：SSE 模式下两侧都受 mock 的 6 段序列限制（每请求 6 个事件）；
> rps 统计的是完整 SSE 事务，不是单个 chunk。两侧错误数全为 0，
> 吞吐差距并非「更快地丢弃请求」造成。
>
> 原版 chat 链路本次表现为恒定 64 rps 天花板（16 并发时约 56 rps，此为服务端内存态限流所致）：
> 两种并发下都是 6 秒 384 请求，即触顶而非排队。为排除运行顺序影响，已用全新原版实例先跑、
> 跑完 Rust 后再复跑，数值一致。另有一次 64 并发 SSE 在长时间运行实例上塌到 10.7 rps
> （p50 ≈ 24 s），紧接着 4 次复测均未复现（56 / 53.3 / 64 / 64 rps），按离群值剔除。

## 3. 差异来源分析

1. **运行时开销**：原版每请求穿过 Next.js middleware/authz pipeline（`src/proxy.ts` matcher 覆盖所有路径），仅 `/healthz` 也要走完整中间件链（~7–22ms/请求的框架固定开销 → 64 并发下排到 ~0.9–1.2s）。Rust 版 axum 直达 handler。
2. **每请求落库**：原版将每次请求历史写入 SQLite（请求/响应/路由诊断），并叠加内存态限流记账，是 56–64 rps 恒定天花板的主因；Rust 版核心为无库设计（内存态熔断/配额），写盘为零。
3. **内存基线**：Next.js + Node V8 堆 + instrumentation + 目录缓存的固有成本；Rust 无 GC、静态注册表。
4. **SSE 事务**：原版流式响应每帧经过 PII 处理/压缩 echo/统计管道；Rust 版逐帧翻译为直通转换，开销更小。

## 4. 结论

在**同一上游、同一压测工具、同机串行**条件下，Rust 重写版相对原版生产栈：

- 内存：空闲 **约低 57×**（14 MB vs 794 MB），满负载 **约低 37–71×**（14–33 MB vs 987 MB–1.22 GB）
- 吞吐：JSON 代理 **约 155–223×**，纯网关路径 **约 23–36×**，SSE 流式 **约 6.0–20×**
- 延迟：JSON 代理 p50 为 1–3ms（原版为 231–835ms）；64 并发下 p99 为 4ms（原版 1,070ms），SSE 为 51ms（原版 1,176ms）

## 5. 复现方式

```bash
# 1) 构建 mock upstream、压测客户端与网关
cargo build --release --example mock_upstream --example loadgen
cargo build --release

# 2) 启动 mock upstream（:9900）
./target/release/examples/mock_upstream 9900 &

# 3) Rust 网关使用「仅基准」端口（切勿占用线上 20128），把基准 provider 指向 mock，
#    并与原版使用同一个 API key 与同一组限流参数
mkdir -p /tmp/omni-bench-rs6 && cat > /tmp/omni-bench-rs6/provider-credentials.json <<'JSON'
{"providers":{"openai-compatible-bench":{"apiKey":"bench-upstream",
  "baseUrl":"http://127.0.0.1:9900/v1","models":["mock-model"]}}}
JSON
OMNIROUTE_DATA_DIR=/tmp/omni-bench-rs6 \
OMNIROUTE_API_KEY=bench-master \
OMNIROUTE_REQUESTS_PER_MINUTE=100000 OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS=0 \
OMNIROUTE_CONCURRENT_REQUESTS=128 OMNIROUTE_MODEL_CATALOG_AUTOUPDATE=false \
./target/release/omniroute serve --port 20129 &

# 4) 原版（npm 3.8.50）用独立 DATA_DIR 起在 :20130（默认 ~/.omniroute），
#    必须先提高限流，否则请求队列会主导结果；再把 mock 注册为 provider node +
#    connection 并签发 API key。首个仪表盘密码为 CHANGEME。
DATA_DIR=/tmp/omni-bench-ts2/data omniroute serve --port 20130 --no-open --no-tray &
curl -s -c /tmp/cj.txt -X POST http://127.0.0.1:20130/api/auth/login \
  -H 'content-type: application/json' -d '{"password":"CHANGEME"}'
curl -X PATCH http://127.0.0.1:20130/api/resilience -b /tmp/cj.txt \
  -H 'content-type: application/json' \
  -d '{"requestQueue":{"requestsPerMinute":100000,"minTimeBetweenRequestsMs":0,"concurrentRequests":128}}'
#    -> {"node":{"id":"openai-compatible-chat-<uuid>",...}}
curl -X POST http://127.0.0.1:20130/api/provider-nodes -b /tmp/cj.txt \
  -H 'content-type: application/json' \
  -d '{"name":"Mock Bench","prefix":"openai-compatible-bench",
       "baseUrl":"http://127.0.0.1:9900/v1","apiType":"chat"}'
curl -X POST http://127.0.0.1:20130/api/providers -b /tmp/cj.txt \
  -H 'content-type: application/json' \
  -d '{"provider":"openai-compatible-chat-<uuid>","name":"Mock Bench","apiKey":"bench-upstream"}'
curl -X POST http://127.0.0.1:20130/api/keys -b /tmp/cj.txt \
  -H 'content-type: application/json' -d '{"name":"bench-master"}'   # -> sk-...

# 5) 压测
./target/release/examples/loadgen --url http://127.0.0.1:20129/v1/chat/completions \
  --mode json --concurrency 64 --duration 6 \
  --model "openai-compatible-bench/mock-model" --api-key bench-master

# 6) 内存：压测中合计进程树 RSS（原版是 supervisor + Next server + 子进程）
ps -eo pid,ppid,rss --no-headers | awk '{s+=$3} END {print s" KB"}'
```

上述数字使用同一套 `examples/loadgen.rs`：先校验基准端口空闲，在 20129 压 Rust、
在 20130 用同一 key/同一限流压原版，并在压测中采样进程树 RSS（每次 6 秒运行内
自 1.5–5.5 秒取 4 个样本；空闲取全新启动实例的 3 次均值）。所有结果为单次运行；
Rust 侧多次运行间波动约 2% 以内，原版 56–64 rps 的恒定天花板每轮均复现。

## 6. 局限性说明

- 原版是完整生产栈（含仪表盘、DB、遥测），对比为**端到端网关行为**对比，而非剥离框架后的翻译引擎单测。
- 压测模型为极短请求（mock 即时返回），放大了框架开销差异；真实长生成流（分钟级）下两端差异主要体现在**并发连接数的内存成本**（Rust 每连接 ~KB 级 vs 原版每请求历史行写入）。
- WSL2 环境、单机回环测试；网络 RTT 为零，结果不反映跨机房延迟。
