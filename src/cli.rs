//! CLI implementation (parity: `bin/omniroute.mjs` + `bin/cli/commands/*`).
//! Subcommands: serve (default), status, stop, models, providers, combos,
//! doctor. Output: human table or `--output json`.

use crate::config::{resolve_data_dir, Config};
use anyhow::{anyhow, Context};
use std::path::PathBuf;
use std::time::Duration;

pub fn pid_file() -> PathBuf {
    resolve_data_dir().join("omniroute.pid")
}

fn read_pid() -> Option<i32> {
    std::fs::read_to_string(pid_file()).ok()?.trim().parse().ok()
}

fn pid_alive(pid: i32) -> bool {
    // signal 0 checks existence without side effects
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `serve` — start the gateway in the foreground (records a pidfile).
pub async fn serve(port: Option<u16>, host: Option<String>) -> anyhow::Result<()> {
    let mut config = Config::load(port).context("loading config")?;
    if let Some(h) = host {
        config.host = h;
    }
    std::fs::create_dir_all(&config.data_dir).ok();
    let addr = format!("{}:{}", config.host, config.port);
    let app = crate::server::build_router(std::sync::Arc::new(crate::state::AppState::new(config)));
    std::fs::write(pid_file(), std::process::id().to_string()).ok();

    let listener = tokio::net::TcpListener::bind(&addr).await.context(format!("bind {addr}"))?;
    tracing::info!("omniroute-rust v{} listening on http://{}", crate::VERSION, addr);
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server run")?;
    let _ = std::fs::remove_file(pid_file());
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// `status` — pidfile + health probe.
pub async fn status(port: u16) -> anyhow::Result<bool> {
    let pid = read_pid();
    let alive = pid.map(pid_alive).unwrap_or(false);
    let healthy = probe_health(port).await;
    if crate::is_json_output() {
        println!(
            "{}",
            serde_json::json!({
                "pid": pid,
                "alive": alive,
                "healthy": healthy,
                "port": port,
            })
        );
    } else {
        println!(
            "pid: {:?}  alive: {}  healthy({port}): {}",
            pid,
            alive,
            healthy
        );
    }
    Ok(alive && healthy)
}

async fn probe_health(port: u16) -> bool {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/healthz");
    match client.get(&url).timeout(Duration::from_secs(2)).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// `stop` — SIGTERM the pidfile pid, wait for exit.
pub async fn stop() -> anyhow::Result<()> {
    let Some(pid) = read_pid() else {
        return Err(anyhow!("no pidfile at {}", pid_file().display()));
    };
    if !pid_alive(pid) {
        let _ = std::fs::remove_file(pid_file());
        return Err(anyhow!("process {pid} not running"));
    }
    #[cfg(unix)]
    {
        let out = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .context("send SIGTERM")?;
        if !out.success() {
            return Err(anyhow!("failed to signal process {pid}"));
        }
        for _ in 0..50 {
            if !pid_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let _ = std::fs::remove_file(pid_file());
    if !crate::is_json_output() {
        println!("stopped pid {pid}");
    }
    Ok(())
}

fn base_url(base: Option<String>, port: u16) -> String {
    base.unwrap_or_else(|| format!("http://127.0.0.1:{port}"))
}

/// `models` — GET /v1/models from a running gateway.
pub async fn models(base: Option<String>, port: u16, api_key: Option<String>) -> anyhow::Result<()> {
    let url = format!("{}/v1/models", base_url(base, port));
    let client = reqwest::Client::new();
    let mut req = client.get(&url).timeout(Duration::from_secs(10));
    if let Some(k) = api_key.or_else(|| std::env::var("OMNIROUTE_API_KEY").ok()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.context("GET /v1/models")?;
    let status = resp.status();
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({"raw": text}));
    if crate::is_json_output() {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}: {text}"));
    }
    println!("{:<44} {:<12}", "MODEL ID", "PROVIDER");
    if let Some(arr) = v.pointer("/data").and_then(|d| d.as_array()) {
        for m in arr {
            println!(
                "{:<44} {:<12}",
                m.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                m.get("provider").and_then(|x| x.as_str()).unwrap_or("?")
            );
        }
    }
    Ok(())
}

/// `providers` — GET /v1/providers from a running gateway.
pub async fn providers(base: Option<String>, port: u16, api_key: Option<String>) -> anyhow::Result<()> {
    let url = format!("{}/v1/providers", base_url(base, port));
    let client = reqwest::Client::new();
    let mut req = client.get(&url).timeout(Duration::from_secs(10));
    if let Some(k) = api_key.or_else(|| std::env::var("OMNIROUTE_API_KEY").ok()) {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.context("GET /v1/providers")?;
    let text = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
    if crate::is_json_output() {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("{:<20} {:<10} {:<8} {:<10}", "PROVIDER", "FORMAT", "LOCAL", "KEY");
    if let Some(arr) = v.get("providers").and_then(|p| p.as_array()) {
        for p in arr {
            println!(
                "{:<20} {:<10} {:<8} {:<10}",
                p.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                p.get("format").and_then(|x| x.as_str()).unwrap_or("?"),
                p.get("isLocal").and_then(|x| x.as_bool()).unwrap_or(false),
                p.get("hasKey").and_then(|x| x.as_bool()).unwrap_or(false)
            );
        }
    }
    Ok(())
}

/// `combos` — configured combos (local config file, no server needed).
pub async fn combos() -> anyhow::Result<()> {
    let cfg = Config::load(None)?;
    if crate::is_json_output() {
        println!("{}", serde_json::to_string_pretty(&cfg.combos)?);
        return Ok(());
    }
    if cfg.combos.is_empty() {
        println!("no combos configured (define [combos] in omniroute.toml)");
        return Ok(());
    }
    for c in &cfg.combos {
        println!(
            "{} ({}) → {}",
            c.name,
            c.strategy.clone().unwrap_or_else(|| "priority".into()),
            c.providers.join(" → ")
        );
    }
    Ok(())
}

/// `doctor` — validate config/data-dir/credentials.
pub async fn doctor() -> anyhow::Result<()> {
    let data_dir = resolve_data_dir();
    let mut checks: Vec<(String, bool, String)> = Vec::new();

    let writable = std::fs::create_dir_all(&data_dir).is_ok();
    checks.push(("data dir".into(), writable, data_dir.display().to_string()));

    let creds = crate::config::load_credentials_file(&data_dir);
    checks.push((
        "provider-credentials.json".into(),
        creds.is_ok(),
        match &creds {
            Ok(m) => format!("{} providers", m.len()),
            Err(e) => e.to_string(),
        },
    ));

    let cfg = Config::load(None);
    checks.push((
        "config load".into(),
        cfg.is_ok(),
        match &cfg {
            Ok(c) => format!("port {}, {} providers with keys", c.port, c.providers_with_keys().len()),
            Err(e) => e.to_string(),
        },
    ));

    if let Ok(c) = cfg {
        for p in c.providers_with_keys() {
            let e = c.effective_registry().get(&p).map(|x| x.format).unwrap();
            checks.push((format!("provider {p}"), true, format!("{e:?}")));
        }
    }

    if crate::is_json_output() {
        let arr: Vec<serde_json::Value> = checks
            .iter()
            .map(|(name, ok, detail)| serde_json::json!({"check": name, "ok": ok, "detail": detail}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"checks": arr}))?);
        return Ok(());
    }
    for (name, ok, detail) in &checks {
        println!("{:<6} {:<28} {}", if *ok { "OK" } else { "FAIL" }, name, detail);
    }
    if checks.iter().all(|(_, ok, _)| *ok) {
        Ok(())
    } else {
        Err(anyhow!("doctor found problems"))
    }
}

/// `omniroute reset-password [--password <pw> | --password-stdin]`
///
/// Parity with the upstream `bin/reset-password.mjs`:
/// * `--password <pw>` — explicit, non-interactive
/// * `--password-stdin` — the whole stdin stream is the password
/// * piped stdin without flags — first line is the password, an optional second
///   line is the confirmation and must match
/// * minimum length 8
///
/// The record lives in `$DATA_DIR/dashboard-auth.json` (salted SHA-256), so this
/// works with the gateway stopped.
pub fn reset_password(password: Option<String>, password_stdin: bool) -> anyhow::Result<()> {
    use std::io::Read;

    const MIN: usize = 8;
    let data_dir = resolve_data_dir();

    let new_password = match (password, password_stdin) {
        (Some(p), _) => p,
        (None, true) => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).ok();
            buf.trim_end_matches(['\n', '\r']).to_string()
        }
        (None, false) => {
            if crate::stdin_is_tty() {
                // no TTY prompt in this build: require an explicit flag
                anyhow::bail!(
                    "no password provided — pass --password <pw> or --password-stdin (min {MIN} chars)"
                );
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).ok();
            let mut lines = buf.lines();
            let first = lines.next().unwrap_or("").trim_end_matches(['\n', '\r']).to_string();
            if let Some(second) = lines.next() {
                let second = second.trim_end_matches(['\n', '\r']);
                if !second.is_empty() && second != first {
                    anyhow::bail!("passwords do not match");
                }
            }
            first
        }
    };

    if new_password.chars().count() < MIN {
        anyhow::bail!("password too short (min {MIN} chars)");
    }

    crate::server::security::reset_password(&data_dir, &new_password)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", data_dir.join("dashboard-auth.json").display()))?;

    println!("admin password updated ({} bytes written)", new_password.len());
    println!("  record: {}", data_dir.join("dashboard-auth.json").display());
    println!("  restart the gateway for the new password to take effect if it is already running");
    Ok(())
}
