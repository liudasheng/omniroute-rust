//! Load generator used for gateway benchmarking.
//!
//! Usage:
//!   cargo run --release --example loadgen -- \
//!     --url http://127.0.0.1:20128/v1/chat/completions \
//!     --concurrency 32 --duration 8 --mode json|sse \
//!     --api-key master-key --model "openai-compatible-bench/mock-model"
//!
//! Prints: requests, RPS, p50/p95/p99 latency (ms), errors.

use clap::Parser;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    url: String,
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    #[arg(long, default_value_t = 8.0)]
    duration: f64,
    /// json | sse
    /// json | sse | get (GET, no body — for /healthz style probes)
    #[arg(long, default_value = "json")]
    mode: String,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(long, default_value = "openai-compatible-bench/mock-model")]
    model: String,
    /// per-request SSE chunk delay multiplier (sleep between chunks, ms)
    #[arg(long, default_value_t = 0)]
    stream_delay_ms: u64,
}

struct Result1 {
    ok: bool,
    latency: Duration,
}

async fn one(client: &reqwest::Client, args: &Args, url: &str, body: &Value, permit: &Semaphore) -> Result1 {
    let _p = permit.acquire().await.unwrap();
    let started = Instant::now();
    let mut req = if args.mode == "get" {
        client.get(url)
    } else {
        client.post(url).json(body)
    };
    if let Some(k) = &args.api_key {
        req = req.bearer_auth(k);
    }
    let ok = match req.send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                false
            } else if args.mode == "get" {
                true
            } else if args.mode == "sse" {
                let mut stream = resp.bytes_stream();
                let mut done = false;
                while let Some(chunk) = stream.next().await {
                    if let Ok(b) = chunk {
                        if b.windows(6).any(|w| w == b"[DONE]") {
                            done = true;
                            break;
                        }
                    }
                }
                done
            } else {
                true
            }
        }
        Err(_) => false,
    };
    Result1 { ok, latency: started.elapsed() }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let url = args.url.trim_end_matches('/').to_string();
    let body = if args.mode == "get" {
        Value::Null
    } else {
        json!({
            "model": args.model,
            "stream": args.mode == "sse",
            "messages": [{"role": "user", "content": "Say hello."}],
            "max_tokens": 64
        })
    };
    let client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(args.concurrency)
        .build()
        .unwrap();
    let semaphore = Semaphore::new(args.concurrency);
    let deadline = Instant::now() + Duration::from_secs_f64(args.duration);

    let mut handles = Vec::new();
    loop {
        if Instant::now() >= deadline {
            break;
        }
        // spawn in waves: keep concurrency saturated
        let batch = args.concurrency;
        let mut batch_futs = Vec::with_capacity(batch);
        for _ in 0..batch {
            if Instant::now() >= deadline {
                break;
            }
            batch_futs.push(one(&client, &args, &url, &body, &semaphore));
        }
        let results = futures::future::join_all(batch_futs).await;
        for r in results {
            handles.push(r);
        }
    }

    let total = handles.len();
    let ok_count = handles.iter().filter(|r| r.ok).count();
    let mut lat: Vec<u128> = handles.iter().map(|r| r.latency.as_millis()).collect();
    lat.sort_unstable();
    let pct = |p: usize| -> u128 { if lat.is_empty() { 0 } else { lat[(lat.len() - 1).min((p * lat.len()) / 100)] } };
    let elapsed = args.duration;
    let out = json!({
        "mode": args.mode,
        "concurrency": args.concurrency,
        "duration_s": elapsed,
        "requests": total,
        "ok": ok_count,
        "errors": total - ok_count,
        "rps": format!("{:.1}", ok_count as f64 / elapsed),
        "p50_ms": pct(50),
        "p95_ms": pct(95),
        "p99_ms": pct(99),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
