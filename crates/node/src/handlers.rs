use common::types::AppId;
use messaging::events::Event;
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
    pub store: Store,
    pub runtime: WasmRuntime,
}

impl EventDispatcher {
    pub async fn handle(&self, event: Event) {
        match event {
            Event::DeployApp {
                app_id,
                config,
                wasm_bytes,
                expected_hash,
            } => {
                self.handle_deploy(app_id, config, wasm_bytes, expected_hash)
                    .await
            }
            Event::RemoveApp { app_id } => self.handle_remove(app_id).await,
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
        wasm_bytes: Vec<u8>,
        expected_hash: Option<String>,
    ) {
        info!(app = %app_id.0, bytes = wasm_bytes.len(), "deploying app");

        if let Some(expected) = expected_hash {
            let mut hasher = Sha256::new();
            hasher.update(&wasm_bytes);
            let actual = format!("{:x}", hasher.finalize());
            if actual != expected {
                error!(
                    app = %app_id.0,
                    expected,
                    actual,
                    "SECURITY: Wasm binary hash mismatch! Rejecting deploy."
                );
                return;
            }
        }

        // 1. Compile (CPU-intensive — spawn_blocking)
        let runtime = self.runtime.clone();
        let wasm_bytes_clone = wasm_bytes.clone();
        let artifact =
            tokio::task::spawn_blocking(move || runtime.compile(&wasm_bytes_clone)).await;

        match artifact {
            Ok(Ok(artifact_bytes)) => {
                // 2. Store artifact and config
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
