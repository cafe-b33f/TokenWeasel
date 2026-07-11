//! Background model polling for backends with `models_poll_secs` set.
//!
//! Each poll fetches the provider's models endpoint, parses the response,
//! and updates the catalog. Unreachable backends get their entries cleared.

use std::sync::Arc;

use reqwest::Client;
use twl_backends::BackendRegistry;

/// Spawn polling tasks for backends with `models_poll_secs`. Tasks run
/// independently and never block request handling.
pub fn spawn_watchdog(registry: Arc<BackendRegistry>) {
    // Use a dedicated client with keep-alive disabled. Upstreams close idle
    // connections faster than the proxy's 90 s pool timeout, so pooling
    // caused alternating poll failures on stale sockets.
    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(0)
        .build()
        .expect("watchdog client");
    let backends = registry.backends();
    for (idx, backend) in backends.iter().enumerate() {
        let Some(poll_secs) = backend.models_poll_secs else {
            continue;
        };

        let client = client.clone();
        let registry = Arc::clone(&registry);
        let upstream = backend.upstream.clone();
        let provider = backend.provider;
        let auth_headers = backend.auth_headers.clone();

        tokio::spawn(async move {
            // Track last-seen model ids so unchanged polls are no-ops.
            let mut last_ids: Option<Vec<String>> = None;
            loop {
                if let Some(entries) =
                    poll_once(&client, idx, &upstream, provider, &auth_headers).await
                {
                    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
                    if last_ids.as_ref() != Some(&ids) {
                        registry.set_backend_models(idx, entries);
                        tracing::info!(
                            backend = idx,
                            model_count = ids.len(),
                            "watchdog: catalog updated"
                        );
                        last_ids = Some(ids);
                    }
                } else if !matches!(&last_ids, Some(ids) if ids.is_empty()) {
                    // Backend unreachable - clear its models so /v1/models
                    // reflects the outage. Set last_ids to Some(empty) to
                    // avoid repeated clears. Then a successful poll restores.
                    registry.set_backend_models(idx, Vec::new());
                    tracing::warn!(
                        backend = idx,
                        "watchdog: backend unreachable, catalog cleared"
                    );
                    last_ids = Some(Vec::new());
                }

                tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
            }
        });
    }
}

/// Single poll attempt for one backend. Returns the parsed model entries,
/// or `None` on any failure (transport, timeout, non-2xx, parse) after
/// logging a warning. The caller decides whether the catalog changed.
async fn poll_once(
    client: &Client,
    backend_idx: usize,
    upstream: &str,
    provider: twl_provider::ProviderKind,
    auth_headers: &[(String, String)],
) -> Option<Vec<twl_provider::ModelEntry>> {
    let endpoint = provider.models_endpoint();
    let url = format!("{upstream}{endpoint}");

    let mut request_builder = client.get(&url);
    for (name, value) in auth_headers {
        request_builder = request_builder.header(name, value);
    }
    // Timeout covers the full poll (request + body read).
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        match request_builder.send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    resp.text()
                        .await
                        .map(String::into_bytes)
                        .map_err(|e| format!("failed to read response body: {e}"))
                } else {
                    Err(format!("non-2xx response: {}", resp.status()))
                }
            }
            Err(e) => Err(format!("request failed: {e}")),
        }
    })
    .await;

    let body = match response {
        Ok(Ok(body)) => body,
        Ok(Err(e)) => {
            tracing::warn!(
                backend = backend_idx,
                endpoint = endpoint,
                error = %e,
                "watchdog: poll failed"
            );
            return None;
        }
        Err(_) => {
            tracing::warn!(
                backend = backend_idx,
                endpoint = endpoint,
                "watchdog: request timed out"
            );
            return None;
        }
    };

    let entries = match provider.parse_models(&body) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                backend = backend_idx,
                endpoint = endpoint,
                error = %e,
                "watchdog: failed to parse models response"
            );
            return None;
        }
    };

    Some(entries)
}
