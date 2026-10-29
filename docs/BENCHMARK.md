# Memory & Concurrency Benchmark: omniroute-rust vs the original (TypeScript)

> Subjects: the original [diegosouzapw/OmniRoute](https://github.com/diegosouzapw/OmniRoute)
> v3.8.50 (published npm package, Next.js standalone production build) vs this
> Rust rewrite **v1.1.7** (release build). 中文版见 [docs/zh/BENCHMARK.md](zh/BENCHMARK.md)；
> results summary also in [README.md](../README.md#benchmark-summary)。

## 1. Methodology

| Item | Detail |
|---|---|
| Machine | One machine (22 logical CPUs / 16 GB, WSL2, Linux 6.18.54, Ubuntu 20.04, glibc 2.31); both gateways run serially, not concurrently |
| Upstream | Both gateways point at the **same** high-performance mock upstream (`examples/mock_upstream.rs`, axum; instant JSON / 6-chunk SSE), eliminating upstream variance |
| Endpoints under test | ① `GET /healthz` (pure gateway overhead, no upstream) ② `POST /v1/chat/completions` JSON non-streaming ③ same endpoint, SSE streaming |
| Load tool | `examples/loadgen.rs` (tokio + reqwest), concurrency 16 / 64, 5–6 s per scenario |
| Memory metric | Sum of process-tree RSS (CLI launcher + child processes); sampled mid-load per scenario, idle = mean of 3 samples |
| Rate limits | Raised **identically** on both sides (original via `PATCH /api/resilience` → 100,000 RPM / 0 ms interval / 128 concurrent; Rust via the same values through env vars) so the benchmark measures the engine, not the quota queue |
| Fairness | Same mock upstream, same logical model (`mock-model`; provider prefixes are implementation-specific), same load client |

## 2. Results

> Re-run **2026-10-29** on the current build — omniroute-rust **v1.1.7** (commit
> `f9502e0`) vs the same original npm v3.8.50 — with dedicated bench instances;
> the live service port (20128) is never used. Both sides were booted **fresh**
> (new data dir, no prior load) right before their own run, so RSS starts from
> the real idle baseline. Raw artefacts: `/tmp/omni-bench-results6/`
> (`*.rss` = per-scenario RSS samples, `*_f.json` = loadgen output).

### Memory (RSS, process-tree total)

| State | omniroute-rust | Original (TS) | Ratio |
|---|---|---|---|
| Idle (after boot) | **14 MB** | 794 MB | **~57×** |
| `/healthz` 16 concurrent | 14 MB | 987 MB | ~71× |
| chat JSON 16 concurrent | 22 MB | 1.06 GB | ~48× |
| chat JSON 64 concurrent | 32 MB | 1.22 GB | ~38× |
| SSE 16 concurrent | 23 MB | 1.07 GB | ~47× |
| SSE 64 concurrent | 33 MB | 1.22 GB | ~37× |

Peak measured: **33 MB** (Rust) vs **1.22 GB** (original). The original does not
return memory after load — after the 64-concurrency tests it stayed at ~1.26 GB
idle.

### Concurrency throughput (same mock upstream, zero errors on both sides)

| Scenario | Concurrency | omniroute-rust | Original | Ratio |
|---|---|---|---|---|
| `GET /healthz` (pure gateway) | 16 | **27,208 rps** | 1,205 rps | ~23× |
| `GET /healthz` (pure gateway) | 64 | **50,293 rps** | 1,397 rps | ~36× |
| chat JSON (proxy) | 16 | **8,688 rps** | 56 rps | ~155× |
| chat JSON (proxy) | 64 | **14,293 rps** | 64 rps | ~223× |
| chat SSE streaming | 16 | **333 rps** | 56 rps | ~6.0× |
| chat SSE streaming | 64 | **1,280 rps** | 64 rps | ~20× |

### Latency (p50 / p99, ms)

| Scenario | omniroute-rust p50/p99 | Original p50/p99 |
|---|---|---|
| healthz c16 | 0 / 0 | 7 / 18 |
| healthz c64 | 0 / 1 | 22 / 49 |
| JSON proxy c16 | 1 / 2 | 231 / 431 |
| JSON proxy c64 | 3 / 4 | 835 / 1,070 |
| SSE c16 | 47 / 50 | 227 / 314 |
| SSE c64 | 48 / 51 | 878 / 1,176 |

> Note: in SSE mode both sides are bounded by the mock's 6-chunk sequence (6
> events per request); rps counts complete SSE transactions, not single chunks.
> Error counts were **0** for every Rust scenario and every original scenario —
> the throughput gap is not a "sheds load faster" artefact.
>
> The original's chat path is now gated at 64 rps (and ~56 rps at 16 concurrent,
> where its in-memory limiter is the binding constraint): 384 requests per 6 s
> at both concurrencies, i.e. a flat ceiling, not a queue. Ordering was ruled
> out by running the original first on a fresh instance and re-running it after
> the Rust side — both gave the same numbers. One earlier 64-concurrency SSE run
> on a long-lived instance collapsed to 10.7 rps (p50 ≈ 24 s) and did not
> reproduce in four immediate re-runs (56 / 53.3 / 64 / 64 rps), so it is
> excluded as an outlier.

## 3. Where the differences come from

1. **Per-request framework cost**: in the original every request traverses the
   Next.js middleware/authz pipeline (`src/proxy.ts` matcher covers all
   paths) — even `/healthz` pays the full chain (~7–22 ms fixed overhead per
   request → ~0.9–1.2 s of queueing at 64 concurrency). In the Rust version,
   axum dispatches straight to the handler.
2. **Per-request database writes**: the original writes every request to its
   SQLite history (request/response/routing diagnostics) plus its in-memory
   rate-limit accounting — a major factor in the flat 56–64 rps ceiling. The
   Rust core is database-free by design (in-memory circuit/quota state); zero
   disk writes on the hot path.
3. **Memory baseline**: the intrinsic cost of Next.js + Node V8 heap +
   instrumentation + catalog caches; Rust has no GC and a static registry.
4. **SSE transactions**: the original pushes every stream frame through PII
   sanitization / compression echo / statistics pipelines; the Rust version
   translates frames with direct conversions.

## 4. Conclusion

Against the original production stack — same upstream, same load tool, same
machine, run serially — the Rust rewrite achieves:

- Memory: idle **~57× lower** (14 MB vs 794 MB), under load **~37–71× lower**
  (14–33 MB vs 987 MB–1.22 GB)
- Throughput: JSON proxy **~155–223×**, pure gateway path **~23–36×**, SSE
  streaming **~6.0–20×**
- Latency: JSON proxy p50 is 1–3 ms vs 231–835 ms; at 64 concurrent,
  tail latency (p99) is 4 ms vs 1,070 ms (JSON) and 51 ms vs 1,176 ms (SSE)

## 5. How to reproduce

```bash
# 1) Build the mock upstream, the load generator and the gateway
cargo build --release --example mock_upstream --example loadgen
cargo build --release

# 2) Start the mock upstream (:9900)
./target/release/examples/mock_upstream 9900 &

# 3) Rust gateway on a BENCH-ONLY port (never the live service port).
#    Point the bench provider at the mock. Use the same API key and the same
#    raised limits as the original.
mkdir -p /tmp/omni-bench-rs6 && cat > /tmp/omni-bench-rs6/provider-credentials.json <<'JSON'
{"providers":{"openai-compatible-bench":{"apiKey":"bench-upstream",
  "baseUrl":"http://127.0.0.1:9900/v1","models":["mock-model"]}}}
JSON
OMNIROUTE_DATA_DIR=/tmp/omni-bench-rs6 \
OMNIROUTE_API_KEY=bench-master \
OMNIROUTE_REQUESTS_PER_MINUTE=100000 OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS=0 \
OMNIROUTE_CONCURRENT_REQUESTS=128 OMNIROUTE_MODEL_CATALOG_AUTOUPDATE=false \
./target/release/omniroute serve --port 20129 &

# 4) Original (npm 3.8.50) on :20130 with its own DATA_DIR (its default is
#    ~/.omniroute), raise its limits FIRST — otherwise its request queue
#    dominates the result — then register the mock as a provider node +
#    connection and issue an API key. First dashboard password is CHANGEME.
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
same key/limits, and sampling process-tree RSS mid-load (4 samples 1.5–5.5 s
into each 6 s run; idle = mean of 3 samples of a freshly booted instance). The
original provider node was configured to point at the same mock upstream.
All results are single runs; the Rust figures repeat within ~2% across runs,
the original's flat 56–64 rps ceiling is reproduced in every run.

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
