use crate::server::admin::{connection_probe_creds, models_url_for, parse_models_list_with_metadata};
use reqwest::Method;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Background task: sync provider `synced_models` from upstream `/models`.
///
/// Runs an immediate round on startup, then a steady cadence.
/// Each round iterates over all enabled provider connections and attempts
/// to refresh their `synced_models` from the upstream `/models` endpoint.
pub fn spawn_background(state: Arc<crate::state::AppState>, ttl_seconds: u64) {
    tokio::spawn(async move {
        // initial attempt
        if let Err(e) = sync_all(&state).await {
            warn!("provider sync: initial round failed: {e}");
        } else {
            info!("provider sync: initial round complete");
        }

        // Seconds, not hours: a sub-hour cadence is what makes this loop
        // observable in a test or a local smoke run.
        let ttl = Duration::from_secs(ttl_seconds.max(1));
        let mut attempts = 0;
        loop {
            tokio::time::sleep(ttl).await;
            match sync_all(&state).await {
                Ok(count) => {
                    info!("provider sync: periodic round complete ({count} connections synced)");
                    attempts = 0;
                }
                Err(e) => {
                    attempts += 1;
                    warn!("provider sync: periodic round failed (attempt {attempts}): {e}");
                }
            }
        }
    });
}

/// Sync all enabled provider connections.
/// Returns the number of connections successfully synced.
async fn sync_all(state: &Arc<crate::state::AppState>) -> Result<usize, String> {
    let connections: Vec<_> = state
        .provider_connections
        .all_unmasked()
        .into_iter()
        // Do NOT require a hand-declared `model_list`: discovering the upstream
        // roster is the whole point, and dashboard-created connections start
        // with an empty list.
        .filter(|c| c.enabled)
        .collect();

    if connections.is_empty() {
        return Ok(0);
    }

    let mut synced = 0;
    for conn in connections {
        match sync_one(state, conn.clone()).await {
            Ok(cnt) => {
                info!(
                    "provider sync: {} ({}) synced {} models",
                    conn.provider, conn.id, cnt
                );
                synced += 1;
            }
            Err(e) => {
                warn!("provider sync: {} ({}) failed: {}", conn.provider, conn.id, e);
            }
        }
    }
    Ok(synced)
}

/// Sync a single provider connection's models from upstream.
async fn sync_one(
    state: &Arc<crate::state::AppState>,
    conn: crate::server::providers_admin::ProviderConnection,
) -> Result<usize, String> {
    if !conn.enabled {
        return Err("connection disabled".to_string());
    }
    let Some(entry) = state.registry.get(&conn.provider) else {
        return Err(format!("provider '{}' not registered", conn.provider));
    };
    let (key, base) = connection_probe_creds(state, &conn);
    let Some(base) = base else {
        return Err(format!("no upstream base for '{}'", conn.provider));
    };

    let (_, hdrs) = match crate::upstream::executor::build_upstream_request(
        &state.config,
        &state.registry,
        &entry,
        &conn.provider,
        "model",
        false,
        key,
        Some(base.clone()),
    ) {
        Ok(v) => v,
        Err(e) => return Err(format!("build request failed: {e}")),
    };

    let resp = match state
        .upstream
        .execute(&models_url_for(&base), Method::GET, hdrs, None, 30_000)
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(format!("network error: {e}")),
    };

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("JSON decode failed: {e}"))?;

    let (mut models, metadata) = parse_models_list_with_metadata(entry.format, &body);
    models.sort();
    models.dedup();
    models.truncate(500);

    // Upsert the connection with updated synced_models and metadata.
    let mut updated_conn = conn.clone();
    updated_conn.synced_models = models.clone();
    updated_conn.model_metadata = metadata;
    updated_conn.synced_at_ms = crate::server::security::now_ms();
    // Refresh the live registry/credential overlay too, so a dynamic
    // `*-compatible-*` connection routes the newly discovered ids without
    // waiting for a restart or an unrelated dashboard edit.
    crate::server::admin::apply_connection(state, &mut updated_conn);
    state.provider_connections.upsert(updated_conn);

    Ok(models.len())
}

/// Convenience for tests: run one full sync cycle synchronously.
#[cfg(test)]
pub async fn sync_all_once(state: &Arc<crate::state::AppState>) -> Result<usize, String> {
    sync_all(state).await
}