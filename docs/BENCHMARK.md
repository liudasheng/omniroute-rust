# Memory & Concurrency Benchmark: omniroute-rust vs the original (TypeScript)

> Subjects: the original [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute)
> v3.8.50 (published npm package, Next.js standalone production build) vs this
> Rust rewrite (release build). 中文版见 [docs/zh/BENCHMARK.md](zh/BENCHMARK.md)；
> results summary also in [README.md](../README.md#benchmark-summary)。

## 1. Methodology

| Item | Detail |
|---|---|
| Machine | One machine (16 cores / 8 GB, WSL2, Linux 6.18); both gateways run serially, not concurrently |
| Upstream | Both gateways point at the **same** high-performance mock upstream (`examples/mock_upstream.rs`, axum; instant JSON / 6-chunk SSE), eliminating upstream variance |
| Endpoints under test | ① `GET /healthz` (pure gateway overhead, no upstream) ② `POST /v1/chat/completions` JSON non-streaming ③ same endpoint, SSE streaming |
| Load tool | `examples/loadgen.rs` (tokio + reqwest), concurrency 16 / 64, 5–6 s per scenario |
| Memory metric | Sum of process-tree RSS (CLI launcher + child processes); sampled mid-load per scenario, idle = mean of 3 samples |
| Rate limits | Raised **identically** on both sides (original via `PATCH /api/resilience` → 100,000 RPM / 0 ms interval / 128 concurrent; Rust via the same values through env vars) so the benchmark measures the engine, not the quota queue |
| Fairness | Same mock upstream, same logical model (`mock-model`; provider prefixes are implementation-specific), same load client |

## 2. Results

> Re-run **2026-09-20** on the current build (post-responses/provider-protocol fixes),
> with a dedicated bench instance — the live service port (20128) is never used.
> Raw artefacts: `/tmp/omni-bench-results6/` (`*_idle*.rss`, `*_*.rss`,
> `<side>_<scenario>_<concurrency>.json` = loadgen output).

### Memory (RSS, process-tree total)

| State | omniroute-rust | Original (TS) | Ratio |
|---|---|---|---|
| Idle (after boot) | **9.4 MB** | 718 MB | **~76×** |
| `/healthz` 16 concurrent | 12.2 MB | 957 MB | ~78× |
| chat JSON 16 concurrent | 24.9 MB | 983 MB | ~39× |
| chat JSON 64 concurrent | 30.8 MB | 1.10 GB | ~36× |
| SSE 64 concurrent | 31.0 MB | 1.26 GB | ~41× |

Peak measured: **31.0 MB** (Rust) vs **1.26 GB** (original).

### Concurrency throughput (same mock upstream, zero errors on both sides)

| Scenario | Concurrency | omniroute-rust | Original | Ratio |
|---|---|---|---|---|
| `GET /healthz` (pure gateway) | 16 | **12,328 rps** | 299 rps | ~41× |
| `GET /healthz` (pure gateway) | 64 | **16,032 rps** | 373 rps | ~43× |
| chat JSON (proxy) | 16 | **2,147 rps** | 19 rps | ~114× |
| chat JSON (proxy) | 64 | **3,381 rps** | 32 rps | ~106× |
| chat SSE streaming | 16 | **312 rps** | 27 rps | ~12× |
| chat SSE streaming | 64 | **1,077 rps** | 32 rps | ~34× |

### Latency (p50 / p99, ms)

| Scenario | omniroute-rust p50/p99 | Original p50/p99 |
|---|---|---|
| healthz c16 | 0 / 1 | 25 / 103 |
| healthz c64 | 2 / 5 | 80 / 248 |
| JSON proxy c16 | 5 / 14 | 748 / 1,309 |
| JSON proxy c64 | 13 / 32 | 1,792 / 3,084 |
| SSE c16 | 48 / 61 | 526 / 746 |
| SSE c64 | 52 / 66 | 2,082 / 2,789 |

> Note: in SSE mode both sides are bounded by the mock's 6-chunk sequence (6
> events per request); rps counts complete SSE transactions, not single chunks.
> Error counts were **0** for every Rust scenario and every original scenario —
> the throughput gap is not a "sheds load faster" artefact.

## 3. Where the differences come from

1. **Per-request framework cost**: in the original every request traverses the
   Next.js middleware/authz pipeline (`src/proxy.ts` matcher covers all
   paths) — even `/healthz` pays the full chain (~30 ms fixed overhead per
   request → 1.5 s+ queueing at 64 concurrency). In the Rust version, axum
   dispatches straight to the handler.
2. **Per-request database writes**: the original writes every request to its
   SQLite history (request/response/routing diagnostics) — a major factor in
   the 32 rps ceiling. The Rust core is database-free by design (in-memory
   circuit/quota state); zero disk writes on the hot path.
3. **Memory baseline**: the intrinsic cost of Next.js + Node V8 heap +
   instrumentation + catalog caches; Rust has no GC and a static registry.
4. **SSE transactions**: the original pushes every stream frame through PII
   sanitization / compression echo / statistics pipelines; the Rust version
   translates frames with direct conversions.

## 4. Conclusion

Against the original production stack — same upstream, same load tool, same
machine, run serially — the Rust rewrite achieves:

- Memory: idle **~76× lower** (9.4 MB vs 718 MB), under load **~36–41× lower**
  (31.0 MB vs 1.10–1.26 GB)
- Throughput: JSON proxy **~106–114×**, pure gateway path **~41–43×**, SSE
  streaming **~12–34×**
- Latency: JSON proxy p50 is 5–13 ms vs 748–1,792 ms; at 64 concurrent,
  tail latency (p99) is 32 ms vs 3,084 ms

## 5. How to reproduce

```bash
# 1) Build the mock upstream, the load generator and the gateway
cargo build --release --example mock_upstream --example loadgen
cargo build --release

# 2) Start the mock upstream (:9900)
./target/release/examples/mock_upstream 9900 &

# 3) Rust gateway on a BENCH-ONLY port (never the live service port)
#    Use the same API key and the same raised limits as the original.
OMNIROUTE_DATA_DIR=/tmp/omni-bench-rs \
OMNIROUTE_API_KEY=bench-master \
OMNIROUTE_REQUESTS_PER_MINUTE=100000 OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS=0 \
OMNIROUTE_CONCURRENT_REQUESTS=128 \
./target/release/omniroute serve --port 20129 &

# 4) Original (npm 3.8.50) — raise its limits FIRST, otherwise its request queue
#    dominates the result (set a known password with `omniroute reset-password`,
#    log in at POST /api/auth/login, then):
curl -X PATCH http://127.0.0.1:20130/api/resilience \
  -H 'content-type: application/json' -H "cookie: auth_token=<jwt>" \
  -d '{"requestQueue":{"requestsPerMinute":100000,"minTimeBetweenRequestsMs":0,"concurrentRequests":128}}'

# 5) Load
./target/release/examples/loadgen --url http://127.0.0.1:20129/v1/chat/completions \
  --mode json --concurrency 64 --duration 6 \
  --model "openai-compatible-bench/mock-model" --api-key bench-master

# 6) Memory mid-load: sum the process tree (the original is a supervisor + Next
#    server + helper children)
ps -eo pid,ppid,rss --no-headers | awk '{s+=$3} END {print s" KB"}'
```

The numbers above were collected with `examples/loadgen.rs` after checking the
bench ports were free, running Rust on 20129 and the original on 20130 with the
same key/limits, and sampling process-tree RSS mid-load. The original provider
node was configured to point at the same mock upstream.

## 6. Limitations

- The original is the complete production stack (dashboard, DB, telemetry);
  the comparison is **end-to-end gateway behavior**, not an isolated
  translation-engine microbenchmark.
- The benchmark model issues very short requests (the mock replies instantly),
  which amplifies framework-overhead differences; for real minute-long
  generations the durable difference is the **memory cost per concurrent
  connection** (KB-scale in Rust vs a per-request history row in the
  original).
- WSL2 environment, single-machine loopback; network RTT is zero, so results
  do not include cross-host latency.
