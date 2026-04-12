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
        /// The raw .wasm bytes (base64-encoded for JSON transport).
        /// For large binaries, prefer a separate artifact fetch via `artifact_url`.
        wasm_bytes: Vec<u8>,
        expected_hash: Option<String>,
    },
    RemoveApp {
        app_id: AppId,
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
