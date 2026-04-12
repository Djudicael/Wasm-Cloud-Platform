// crates/messaging/src/events.rs
use common::types::{AppConfig, AppId};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // ── Deployment ─────────────────────────────────────────────────
    DeployApp {
        app_id: AppId,
        config: AppConfig,
        /// The URL where the .wasm artifact can be fetched.
        /// Format: "http://<node-ip>:<port>/artifacts/<sha256>"
        artifact_url: String,
        /// SHA-256 hex string of the raw .wasm bytes.
        expected_hash: Option<String>,
        /// Size in bytes (for logging and progress tracking).
        #[serde(default)]
        size_bytes: u64,
    },
    RemoveApp {
        app_id: AppId,
    },

    // ── Routing ────────────────────────────────────────────────────
    RouteAdd {
        route: common::types::Route,
    },
    RouteRemove {
        host: String,
    },

    // ── Instance Lifecycle ─────────────────────────────────────────
    InstanceReady {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },
    InstanceDead {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },

    // ── Configuration ──────────────────────────────────────────────
    SecretUpdate {
        app_id: AppId,
        key: String,
        /// Encrypted value (encrypted with the cluster key, not the node key).
        encrypted_value: Vec<u8>,
    },
    ConfigUpdate {
        app_id: AppId,
        config: AppConfig,
    },

    // ── Load Reporting ────────────────────────────────────────────
    NodeLoad {
        node_id: String,
        cpu_percent: f32,
        fuel_budget_used_percent: f32,
        active_instances: u32,
    },
}

impl Event {
    /// Convert to NATS subject string.
    pub fn subject(&self) -> String {
        match self {
            Event::DeployApp { .. } => "deploy.app.new".to_string(),
            Event::RemoveApp { .. } => "deploy.app.remove".to_string(),
            Event::RouteAdd { .. } => "routes.add".to_string(),
            Event::RouteRemove { .. } => "routes.remove".to_string(),
            Event::InstanceReady {
                app_id, node_id, ..
            } => {
                format!("instance.ready.{}.{}", app_id.0, node_id)
            }
            Event::InstanceDead {
                app_id, node_id, ..
            } => {
                format!("instance.dead.{}.{}", app_id.0, node_id)
            }
            Event::SecretUpdate { app_id, .. } => {
                format!("secrets.update.{}", app_id.0)
            }
            Event::ConfigUpdate { app_id, .. } => {
                format!("config.update.{}", app_id.0)
            }
            Event::NodeLoad { node_id, .. } => {
                format!("node.load.{}", node_id)
            }
        }
    }
}
