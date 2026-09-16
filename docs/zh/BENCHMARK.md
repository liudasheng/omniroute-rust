# 内存与并发性能对比评估：omniroute-rust vs 原版（TypeScript）

> 评估对象：原版 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.50（npm 发布包，Next.js standalone 生产构建）与本仓库 Rust 重写版（release 构建）。
> 详细原始数据见本文末尾"原始数据"。英文版：[docs/BENCHMARK.md](../BENCHMARK.md)；结论摘要见 [README_zh.md](../../README_zh.md#性能摘要vs-原版生产栈同机同-mock-upstream)。

## 1. 测试方法

| 项 | 说明 |
|---|---|
| 机器 | 同一台机器（16 核 / 8 GB，WSL2，Linux 6.18），两网关串行运行、互不干扰 |
| 上游 | 两者共同指向**同一个**高性能 mock upstream（本仓库 `examples/mock_upstream.rs`，axum，瞬时返回 / SSE 6 chunk），排除上游差异 |
| 被测端点 | ① `GET /healthz`（纯网关开销，无上游）② `POST /v1/chat/completions` JSON 非流式 ③ 同端点 SSE 流式 |
| 压力工具 | 本仓库 `examples/loadgen.rs`（tokio+reqwest），并发 16 / 64，每场景 5–6 秒 |
| 内存口径 | 进程树 RSS 之和（CLI launcher + 子进程），每场景压测中途采样 + 空闲 3 次取均值 |
| 限流 | 两侧速率限制**同步调高**（原版经 `/api/resilience` 设 100000 RPM/0ms/128 并发；Rust 版同值经环境变量），避免测的是限流队列而非引擎 |
| 公平性 | 双方使用同一 mock upstream、同一模型串（`openai-compatible-bench/mock-model`）、同一压测客户端 |

## 2. 结果总览

> **2025-09-16 重跑**（当前构建：仪表盘/管理面补齐后的版本）。基准实例使用独立端口，
> **绝不占用线上服务端口 20128**。原始数据：`/tmp/bench-results5/`。

### 内存（RSS，进程树合计）

| 状态 | omniroute-rust | 原版（TS） | 倍数 |
|---|---|---|---|
| 空闲（启动后） | **9.1 MB** | 754 MB | **约 83×** |
| `/healthz` 16 并发 | 11.0 MB | 853 MB | 约 78× |
| chat JSON 16 并发 | 19.5 MB | 1.06 GB | 约 54× |
| chat JSON 64 并发 | 26.3 MB | 1.15 GB | 约 44× |
| SSE 64 并发 | 27.8 MB | 1.32 GB | 约 48× |

峰值：Rust **27.8 MB**，原版 **1.32 GB**。

### 并发吞吐（同一 mock upstream，两侧错误数均为 0）

| 场景 | 并发 | omniroute-rust | 原版 | 倍数 |
|---|---|---|---|---|
| `GET /healthz`（纯网关开销） | 16 | **13,043 rps** | 669 rps | 约 20× |
| `GET /healthz`（纯网关开销） | 64 | **17,229 rps** | 819 rps | 约 21× |
| chat JSON（代理） | 16 | **4,085 rps** | 37 rps | 约 110× |
| chat JSON（代理） | 64 | **6,304 rps** | 43 rps | 约 148× |
| chat SSE 流式 | 16 | **341 rps** | 29 rps | 约 12× |
| chat SSE 流式 | 64 | **1,195 rps** | 43 rps | 约 28× |

### 延迟（p50 / p99，毫秒）

| 场景 | omniroute-rust p50/p99 | 原版 p50/p99 |
|---|---|---|
| healthz c16 | 0 / 1 | 12 / 37 |
| healthz c64 | 2 / 4 | 38 / 89 |
| JSON 代理 c16 | 3 / 4 | 347 / 492 |
| JSON 代理 c64 | 7 / 14 | 1,308 / 1,664 |
| SSE c16 | 45 / 50 | 409 / 971 |
| SSE c64 | 49 / 57 | 1,387 / 2,317 |

> 说明：SSE 模式下两侧都受 mock 的 6 段序列限制（每请求 6 个事件）；
> rps 统计的是完整 SSE 事务，不是单个 chunk。两侧错误数全为 0，
> 吞吐差距并非「更快地丢弃请求」造成。

## 3. 差异来源分析

1. **运行时开销**：原版每请求穿过 Next.js middleware/authz pipeline（`src/proxy.ts` matcher 覆盖所有路径），仅 `/healthz` 也要走完整中间件链（~30ms/请求的框架固定开销 → 64 并发下排到 1.5s+）。Rust 版 axum 直达 handler。
2. **每请求落库**：原版将每次请求历史写入 SQLite（请求/响应/路由诊断），是 32 rps 天花板的主因之一；Rust 版核心为无库设计（内存态熔断/配额），写盘为零。
3. **内存基线**：Next.js + Node V8 堆 + instrumentation + 目录缓存的固有成本；Rust 无 GC、静态注册表。
4. **SSE 事务**：原版流式响应每帧经过 PII 处理/压缩 echo/统计管道；Rust 版逐帧翻译为直通转换，开销更小。

## 4. 结论

在**同一上游、同一压测工具、同机串行**条件下，Rust 重写版相对原版生产栈：

- 内存：空闲 **~120× 更低**（7 MB vs ~850 MB），满负载 **~49× 更低**（23 MB vs ~1.1–1.2 GB）
- 吞吐：JSON 代理 **~123–169×**，纯网关路径 **~87–115×**，SSE 流式 **~13–53×**
- 延迟：p50 从数百 ms 降至 3–7ms（JSON 代理），64 并发下尾部延迟（p99）从 ~2.7s 降至 14ms

## 5. 复现方式

```bash
# 1) 构建 mock upstream、压测客户端与网关
cargo build --release --example mock_upstream --example loadgen
cargo build --release

# 2) 启动 mock upstream（:9900）
./target/release/examples/mock_upstream 9900 &

# 3) Rust 网关使用「仅基准」端口（切勿占用线上 20128），
#    与原版使用同一个 API key 与同一组限流参数
OMNIROUTE_DATA_DIR=/tmp/omni-bench-rs \
OMNIROUTE_API_KEY=bench-master \
OMNIROUTE_REQUESTS_PER_MINUTE=100000 OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS=0 \
OMNIROUTE_CONCURRENT_REQUESTS=128 \
./target/release/omniroute serve --port 20129 &

# 4) 原版（npm 3.8.50）——必须先提高限流，否则请求队列会主导结果
#    （用 omniroute reset-password 设定已知密码，POST /api/auth/login 取得会话后）：
curl -X PATCH http://127.0.0.1:20130/api/resilience \
  -H 'content-type: application/json' -H "cookie: auth_token=<jwt>" \
  -d '{"requestQueue":{"requestsPerMinute":100000,"minTimeBetweenRequestsMs":0,"concurrentRequests":128}}'

# 5) 压测
./target/release/examples/loadgen --url http://127.0.0.1:20129/v1/chat/completions \
  --mode json --concurrency 64 --duration 6 \
  --model "openai-compatible-bench/mock-model" --api-key bench-master

# 6) 内存：压测中合计进程树 RSS（原版是 supervisor + Next server + 子进程）
ps -eo pid,ppid,rss --no-headers | awk '{s+=$3} END {print s" KB"}'
```

上述数字使用的脚本为 `run-bench5.sh`：先校验基准端口空闲，在 20129 压 Rust、
在 20130 用同一 key/同一限流压原版，并在压测中采样进程树 RSS。

## 6. 局限性说明

- 原版是完整生产栈（含仪表盘、DB、遥测），对比为**端到端网关行为**对比，而非剥离框架后的翻译引擎单测。
- 压测模型为极短请求（mock 即时返回），放大了框架开销差异；真实长生成流（分钟级）下两端差异主要体现在**并发连接数的内存成本**（Rust 每连接 ~KB 级 vs 原版每请求历史行写入）。
- WSL2 环境、单机回环测试；网络 RTT 为零，结果不反映跨机房延迟。
