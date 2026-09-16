# 内存与并发性能对比评估：omniroute-rust vs 原版（TypeScript）

> 评估对象：原版 [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute) v3.8.50（npm 发布包，Next.js standalone 生产构建）与本仓库 Rust 重写版（release 构建）。
> 详细原始数据见本文末尾"原始数据"。英文版结论摘要见 [README.en.md](../README.en.md#benchmark-summary)。

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

### 内存（RSS，进程树合计）

| 状态 | omniroute-rust | 原版 (TS) | 倍数 |
|---|---|---|---|
| 空闲（启动后静置） | **7.0 MB** | ~850 MB | **~120×** |
| 负载中（16 并发 JSON） | 22 MB | ~1.08 GB | ~49× |
| 负载中（64 并发 JSON） | 23 MB | ~1.12 GB | ~49× |
| 负载中（SSE 流式 64 并发） | 23 MB | ~1.20 GB | ~53× |

> 原版为完整生产栈：Next.js standalone 服务端 + instrumentation + 模型目录/配额缓存 + 请求历史 SQLite 写入；空闲态即约 850 MB（CLI launcher ~130 MB + Next server ~600 MB + esbuild 常驻进程），随流量增长到 1 GB+。Rust 版为单进程，空闲 7 MB，满负载仅升到 ~23 MB。

### 并发吞吐（同一 mock upstream，0 错误）

| 场景 | 并发 | omniroute-rust | 原版 | 吞吐倍数 |
|---|---|---|---|---|
| `/healthz`（纯网关） | 16 | **3,050 rps** | 35 rps | ~87× |
| `/healthz`（纯网关） | 64 | **4,429 rps** | 38 rps | ~115× |
| chat JSON（代理） | 16 | **3,928 rps** | 32 rps | ~123× |
| chat JSON（代理） | 64 | **5,419 rps** | 32 rps | ~169× |
| chat SSE 流式 | 16 | **317 rps** | 24 rps | ~13× |
| chat SSE 流式 | 64 | **1,131 rps** | 21 rps | ~53× |

### 延迟（p50 / p99，ms）

| 场景 | omniroute-rust p50/p99 | 原版 p50/p99 |
|---|---|---|
| healthz c16 | 3 / 11 | 384 / 559 |
| healthz c64 | 8 / 19 | 1,569 / 2,790 |
| JSON 代理 c16 | 3 / 5 | 417 / 656 |
| JSON 代理 c64 | 7 / 14 | 2,056 / 2,683 |
| SSE c16 | 47 / 53 | 537 / 761 |
| SSE c64 | 51 / 59 | 3,215 / 4,230 |

> 注：SSE 场景两端都受 mock upstream 的 6-chunk 序列（每请求 6 帧事件）限制，rps 反映的是"完整 SSE 事务"而非单 chunk。Rust 版 c64 下 1,131 rps ≈ 每秒 6,800 个 chunk 帧。

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
# 1) 构建 mock upstream 与 loadgen
cargo build --release --example mock_upstream --example loadgen
# 2) 启动 mock upstream（:9900）
./target/release/examples/mock_upstream 9900 &
# 3a) Rust 网关（解除限流）
OMNIROUTE_DATA_DIR=<dir-with-credentials> \
OMNIROUTE_REQUESTS_PER_MINUTE=100000 OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS=0 \
OMNIROUTE_CONCURRENT_REQUESTS=128 \
./target/release/omniroute serve --port 20128 &
# 3b) 原版（npm i -g omniroute；限流经 PATCH /api/resilience 调高）
# 4) 压测
./target/release/examples/loadgen --url http://127.0.0.1:20128/v1/chat/completions \
  --mode json --concurrency 64 --duration 6 --model "openai-compatible-bench/mock-model"
# 5) 内存：压测中途对进程树采样 `ps -o rss=`
```

原始数据（JSON/RSS 采样）存于 `/tmp/bench-results2/`（本次运行时快照）。

## 6. 局限性说明

- 原版是完整生产栈（含仪表盘、DB、遥测），对比为**端到端网关行为**对比，而非剥离框架后的翻译引擎单测。
- 压测模型为极短请求（mock 即时返回），放大了框架开销差异；真实长生成流（分钟级）下两端差异主要体现在**并发连接数的内存成本**（Rust 每连接 ~KB 级 vs 原版每请求历史行写入）。
- WSL2 环境、单机回环测试；网络 RTT 为零，结果不反映跨机房延迟。
