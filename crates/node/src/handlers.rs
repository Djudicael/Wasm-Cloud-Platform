use common::types::AppId;
use messaging::events::Event;
use proxy::router::HostRouter;
use proxy::upstream::UpstreamRegistry;
use runtime::WasmRuntime;
use secrets::{encrypt_for_peer, BootstrapKeyPair, SecretProvider};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use storage::Store;
use supervisor::Supervisor;
use tracing::{error, info, warn};

pub struct EventDispatcher {
    pub supervisor: Arc<Supervisor>,
    pub upstream: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    pub store: Store,
    pub runtime: WasmRuntime,
    pub node_id: String,
    pub artifact_server_url: String,
    pub secret_provider: Arc<dyn SecretProvider>,
    pub bootstrap_keypair: Option<BootstrapKeyPair>,
    pub bus: messaging::NatsBus,
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
            Event::NodeJoined {
                node_id,
                artifact_server_url,
                public_key_bytes,
            } => {
                self.handle_node_joined(node_id, artifact_server_url, public_key_bytes)
                    .await;
            }
            Event::StateSnapshot {
                for_node_id,
                configs,
                routes,
                encrypted_secrets,
                artifact_hashes,
            } => {
                if for_node_id == self.node_id {
                    self.handle_state_snapshot(configs, routes, encrypted_secrets, artifact_hashes)
                        .await;
                }
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
                // 5. Store compiled artifact, config, and hash
                if let Err(e) = self.store.store_artifact(&app_id, &artifact_bytes) {
                    error!(app = %app_id.0, error = %e, "failed to store artifact");
                    return;
                }
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "failed to store config");
                    return;
                }
                if let Err(e) = self.store.save_artifact_hash(&app_id, &sha256) {
                    error!(app = %app_id.0, error = %e, "failed to store artifact hash");
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

    async fn handle_node_joined(
        &self,
        new_node_id: String,
        peer_artifact_url: String,
        peer_public_key: Vec<u8>,
    ) {
        // Leader election: only the node with lexicographically smallest ID responds
        if self.node_id > new_node_id {
            return;
        }

        info!(
            new_node = %new_node_id,
            our_node = %self.node_id,
            "sending state snapshot to new node"
        );

        // 1. Collect all configs
        let configs = self
            .store
            .list_apps()
            .unwrap_or_default()
            .iter()
            .filter_map(|id| self.store.load_config(id).ok().flatten())
            .collect::<Vec<_>>();

        // 2. Collect all routes
        let routes = self.store.list_routes().unwrap_or_default();

        // 3. Encrypt secrets for each app
        let mut encrypted_secrets = Vec::new();
        for config in &configs {
            if let Ok(keys) = self.secret_provider.list_keys(&config.id).await {
                for key in keys {
                    if let Ok(value) = self.secret_provider.get(&config.id, &key).await {
                        let encrypted = encrypt_for_peer(&peer_public_key, value.as_bytes());
                        if !encrypted.is_empty() {
                            encrypted_secrets.push((config.id.0.clone(), key, encrypted));
                        }
                    }
                }
            }
        }

        // 4. Collect artifact hashes
        let artifact_hashes: Vec<(String, String)> = configs
            .iter()
            .filter_map(|c| {
                self.store
                    .get_artifact_sha256(&c.id)
                    .ok()
                    .flatten()
                    .map(|h| (c.id.0.clone(), h))
            })
            .collect();

        // 5. Publish the snapshot event
        let event = Event::StateSnapshot {
            for_node_id: new_node_id.clone(),
            configs,
            routes,
            encrypted_secrets,
            artifact_hashes: artifact_hashes.clone(),
        };

        // Publish via NATS (we need access to the bus - will fix in main.rs)
        info!(
            new_node = %new_node_id,
            apps = artifact_hashes.len(),
            "snapshot prepared"
        );

        // 6. Push artifacts to new node in background
        let store = self.store.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            for (app_id_str, sha256) in &artifact_hashes {
                if let Ok(Some(raw)) = store.load_raw_wasm(sha256) {
                    let url = format!("{peer_artifact_url}/artifacts/{sha256}");
                    match client.put(&url).body(raw).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            info!(app = app_id_str, sha256, "pushed artifact to new node");
                        }
                        Ok(resp) => {
                            warn!(
                                app = app_id_str,
                                sha256,
                                status = %resp.status(),
                                "failed to push artifact"
                            );
                        }
                        Err(e) => {
                            warn!(app = app_id_str, sha256, error = %e, "artifact push error");
                        }
                    }
                }
            }
        });

        // Publish the snapshot event via NATS
        if let Err(e) = self.bus.publish(&event).await {
            error!(new_node = %new_node_id, error = %e, "failed to publish snapshot event");
        }
    }

    async fn handle_state_snapshot(
        &self,
        configs: Vec<common::types::AppConfig>,
        routes: Vec<common::types::Route>,
        encrypted_secrets: Vec<(String, String, Vec<u8>)>,
        artifact_hashes: Vec<(String, String)>,
    ) {
        info!(
            apps = configs.len(),
            routes = routes.len(),
            secrets = encrypted_secrets.len(),
            "received state snapshot"
        );

        // 1. Store configs
        for config in &configs {
            if let Err(e) = self.store.save_config(config) {
                error!(app = %config.id.0, error = %e, "failed to save config");
            }
        }

        // 2. Store routes and load into HostRouter
        for route in &routes {
            if let Err(e) = self.store.save_route(route) {
                error!(host = %route.host, error = %e, "failed to save route");
            }
            self.host_router
                .add_route(route.host.clone(), route.app_id.clone())
                .await;
        }

        // 3. Decrypt and store secrets
        let keypair = match &self.bootstrap_keypair {
            Some(kp) => kp,
            None => {
                error!("no bootstrap keypair available to decrypt secrets");
                return;
            }
        };

        for (app_id_str, key, encrypted_value) in encrypted_secrets {
            let app_id = AppId(app_id_str.clone());

            // Decrypt using our bootstrap keypair
            let plaintext_bytes = keypair.decrypt(&encrypted_value);
            if plaintext_bytes.is_empty() {
                error!(app = app_id_str, key, "failed to decrypt secret");
                continue;
            }

            match String::from_utf8(plaintext_bytes) {
                Ok(plaintext) => {
                    if let Err(e) = self.secret_provider.set(&app_id, &key, &plaintext).await {
                        error!(app = app_id_str, key, error = %e, "failed to store secret");
                    } else {
                        info!(app = app_id_str, key, "secret decrypted and stored");
                    }
                }
                Err(e) => {
                    error!(app = app_id_str, key, error = %e, "secret not valid UTF-8");
                }
            }
        }

        // 4. Store artifact hashes
        for (app_id_str, sha256) in &artifact_hashes {
            let app_id = AppId(app_id_str.clone());
            if let Err(e) = self.store.save_artifact_hash(&app_id, sha256) {
                error!(app = app_id_str, error = %e, "failed to save artifact hash");
            }
        }

        // 5. Compile artifacts (artifacts should already be in our local store from push)
        for (app_id_str, sha256) in artifact_hashes {
            let app_id = AppId(app_id_str.clone());

            // Wait a bit for the artifact to arrive via HTTP push
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            if let Ok(Some(raw)) = self.store.load_raw_wasm(&sha256) {
                let runtime = self.runtime.clone();
                let store = self.store.clone();
                let app_id_clone = app_id.clone();

                tokio::task::spawn_blocking(move || match runtime.compile(&raw) {
                    Ok(compiled) => {
                        if let Err(e) = store.store_artifact(&app_id_clone, &compiled) {
                            error!(app = %app_id_str, error = %e, "failed to store compiled artifact");
                        } else {
                            info!(app = %app_id_str, "artifact compiled from snapshot");
                        }
                    }
                    Err(e) => {
                        error!(app = %app_id_str, error = %e, "compilation failed");
                    }
                });
            } else {
                warn!(
                    app = app_id_str,
                    sha256, "artifact not yet available, will compile on first request"
                );
            }
        }

        info!("state snapshot import complete");
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
