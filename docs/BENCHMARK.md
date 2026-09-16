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
| Fairness | Same mock upstream, same model string (`openai-compatible-bench/mock-model`), same load client |

## 2. Results

> Re-run **2025-09-16** on the current build (dashboard/management-parity release),
> with a dedicated bench instance — the live service port (20128) is never used.
> Raw artefacts: `/tmp/bench-results5/` (`*_idle.json`, `rss_*.txt`,
> `<side>_<scenario>_<concurrency>.json` = loadgen output).

### Memory (RSS, process-tree total)

| State | omniroute-rust | Original (TS) | Ratio |
|---|---|---|---|
| Idle (after boot) | **9.1 MB** | 754 MB | **~83×** |
| `/healthz` 16 concurrent | 11.0 MB | 853 MB | ~78× |
| chat JSON 16 concurrent | 19.5 MB | 1.06 GB | ~54× |
| chat JSON 64 concurrent | 26.3 MB | 1.15 GB | ~44× |
| SSE 64 concurrent | 27.8 MB | 1.32 GB | ~48× |

Peak measured: **27.8 MB** (Rust) vs **1.32 GB** (original).

### Concurrency throughput (same mock upstream, zero errors on both sides)

| Scenario | Concurrency | omniroute-rust | Original | Ratio |
|---|---|---|---|---|
| `GET /healthz` (pure gateway) | 16 | **13,043 rps** | 669 rps | ~20× |
| `GET /healthz` (pure gateway) | 64 | **17,229 rps** | 819 rps | ~21× |
| chat JSON (proxy) | 16 | **4,085 rps** | 37 rps | ~110× |
| chat JSON (proxy) | 64 | **6,304 rps** | 43 rps | ~148× |
| chat SSE streaming | 16 | **341 rps** | 29 rps | ~12× |
| chat SSE streaming | 64 | **1,195 rps** | 43 rps | ~28× |

### Latency (p50 / p99, ms)

| Scenario | omniroute-rust p50/p99 | Original p50/p99 |
|---|---|---|
| healthz c16 | 0 / 1 | 12 / 37 |
| healthz c64 | 2 / 4 | 38 / 89 |
| JSON proxy c16 | 3 / 4 | 347 / 492 |
| JSON proxy c64 | 7 / 14 | 1,308 / 1,664 |
| SSE c16 | 45 / 50 | 409 / 971 |
| SSE c64 | 49 / 57 | 1,387 / 2,317 |

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

- Memory: idle **~120× lower** (7 MB vs ~850 MB), under load **~49× lower**
  (23 MB vs ~1.1–1.2 GB)
- Throughput: JSON proxy **~123–169×**, pure gateway path **~87–115×**, SSE
  streaming **~13–53×**
- Latency: JSON proxy p50 drops from hundreds of ms to 3–7 ms; at 64
  concurrent, tail latency (p99) drops from ~2.7 s to 14 ms

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

The harness used for the numbers above is `run-bench5.sh` (checks the bench
port is free, benches Rust on 20129, then the original on 20130 with the same
key/limits, sampling process-tree RSS mid-load).

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
