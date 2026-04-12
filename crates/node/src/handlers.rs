use common::types::AppId;
use messaging::events::Event;
use proxy::router::HostRouter;
use proxy::upstream::UpstreamRegistry;
use runtime::WasmRuntime;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use storage::Store;
use supervisor::Supervisor;
use tracing::{error, info};

pub struct EventDispatcher {
    pub supervisor: Arc<Supervisor>,
    pub upstream: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    pub store: Store,
    pub runtime: WasmRuntime,
}

impl EventDispatcher {
    pub async fn handle(&self, event: Event) {
        match event {
            Event::DeployApp {
                app_id,
                config,
                artifact_url,
                expected_hash,
                size_bytes,
            } => {
                self.handle_deploy(app_id, config, artifact_url, expected_hash, size_bytes)
                    .await
            }
            Event::RemoveApp { app_id } => self.handle_remove(app_id).await,
            Event::RouteAdd { route } => {
                self.store.save_route(&route).ok();
                self.host_router
                    .add_route(route.host.clone(), route.app_id.clone())
                    .await;
                info!(host = %route.host, app = %route.app_id.0, "route added");
            }
            Event::RouteRemove { host } => {
                self.store.delete_route(&host).ok();
                self.host_router.remove_route(&host).await;
                info!(host, "route removed");
            }
            Event::InstanceReady {
                app_id,
                addr,
                node_id,
            } => {
                // Only register if it's from a DIFFERENT node
                // (our own instances are registered directly by the Supervisor)
                if node_id != self.our_node_id() {
                    self.upstream.add(&app_id, addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance registered");
                }
            }
            Event::InstanceDead {
                app_id,
                addr,
                node_id,
            } => {
                if node_id != self.our_node_id() {
                    self.upstream.remove(&app_id, &addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance deregistered");
                }
            }
            Event::SecretUpdate {
                app_id,
                key,
                encrypted_value,
            } => {
                info!(app = %app_id.0, key, "received secret rotation");
                // Decrypt with cluster key and re-encrypt with node key
                // (see step 06 for details)
                // For now, we persist the encrypted value directly to simulate the update.
                if let Err(e) = self.store.save_secrets(&app_id, &encrypted_value) {
                    error!(app = %app_id.0, error = %e, "failed to update secret in cache");
                }
            }
            Event::ConfigUpdate { app_id, config } => {
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "config update failed");
                }
            }
            Event::NodeLoad { .. } => {
                // Collected by the metrics module for cross-node routing decisions
            }
        }
    }

    async fn handle_deploy(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
        artifact_url: String,
        expected_hash: Option<String>,
        size_bytes: u64,
    ) {
        let sha256 = match &expected_hash {
            Some(h) => h.clone(),
            None => {
                error!(app = %app_id.0, "deploy event missing expected_hash");
                return;
            }
        };

        info!(
            app = %app_id.0,
            url = %artifact_url,
            size_mb = size_bytes as f64 / 1_048_576.0,
            "deploying artifact"
        );

        // 1. Check local cache first (another node may have already stored it)
        let wasm_bytes = if self.store.raw_wasm_exists(&sha256).unwrap_or(false) {
            info!(sha256, "artifact already in local cache");
            match self.store.load_raw_wasm(&sha256) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    error!(sha256, "artifact vanished between exists and load");
                    return;
                }
                Err(e) => {
                    error!(sha256, error = %e, "failed to load cached artifact");
                    return;
                }
            }
        } else {
            // 2. Fetch from the source node
            info!(url = %artifact_url, "fetching artifact via HTTP");
            match fetch_artifact(&artifact_url, &sha256).await {
                Ok(bytes) => {
                    // 3. Store raw bytes for future use
                    if let Err(e) = self.store.save_raw_wasm(&sha256, &bytes) {
                        error!(sha256, error = %e, "failed to cache raw wasm");
                    }
                    bytes
                }
                Err(e) => {
                    error!(url = %artifact_url, error = %e, "artifact fetch failed");
                    return;
                }
            }
        };

        info!(app = %app_id.0, bytes = wasm_bytes.len(), "artifact ready, compiling");

        // 4. Compile (CPU-intensive — spawn_blocking)
        let runtime = self.runtime.clone();
        let artifact = tokio::task::spawn_blocking(move || runtime.compile(&wasm_bytes)).await;

        match artifact {
            Ok(Ok(artifact_bytes)) => {
                // 5. Store compiled artifact and config
                if let Err(e) = self.store.store_artifact(&app_id, &artifact_bytes) {
                    error!(app = %app_id.0, error = %e, "failed to store artifact");
                    return;
                }
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "failed to store config");
                    return;
                }
                info!(
                    app = %app_id.0,
                    "deploy complete, waiting for first request"
                );
            }
            Ok(Err(e)) => error!(app = %app_id.0, error = %e, "compilation failed"),
            Err(e) => error!(app = %app_id.0, error = %e, "spawn_blocking panic"),
        }
    }

    async fn handle_remove(&self, app_id: AppId) {
        info!(app = %app_id.0, "removing app");
        // Stop all instances first
        // (supervisor.kill_all_for(&app_id) — not shown here)
        self.store.delete_artifact(&app_id).ok();
        // Remove config too (or mark as tombstone)
    }

    fn our_node_id(&self) -> String {
        std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
    }
}

/// Fetch an artifact from a URL and verify its SHA-256 hash.
async fn fetch_artifact(url: &str, expected_sha256: &str) -> Result<Vec<u8>, String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("HTTP GET failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("artifact server returned {}", resp.status()));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read body: {e}"))?
        .to_vec();

    // Verify integrity
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        return Err(format!(
            "SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        ));
    }

    Ok(bytes)
}
