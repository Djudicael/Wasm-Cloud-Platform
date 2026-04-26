// crates/proxy/src/node_table.rs
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEntry {
    pub node_id: String,
    pub supervisor_addr: SocketAddr,
    pub fuel_used_percent: f32,
    pub active_instances: u32,
    pub last_seen: u64,
    pub health_status: common::health::NodeHealthStatus,
}

impl NodeEntry {
    pub fn is_stale(&self) -> bool {
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            now.as_secs() - self.last_seen > 30
        } else {
            false
        }
    }
}

#[derive(Clone, Default)]
pub struct NodeLoadTable {
    pub nodes: Arc<RwLock<HashMap<String, NodeEntry>>>,
    /// Nodes currently marked unhealthy (e.g., under memory/I/O pressure).
    /// These are excluded from `least_loaded_node` and routing decisions.
    unhealthy: Arc<RwLock<HashSet<String>>>,
}

impl NodeLoadTable {
    pub async fn update(&self, entry: NodeEntry) {
        self.nodes
            .write()
            .await
            .insert(entry.node_id.clone(), entry);
    }

    /// Find the least-loaded node for an app.
    /// Excludes stale nodes and nodes marked as unhealthy (e.g., under pressure).
    pub async fn least_loaded_node(&self) -> Option<NodeEntry> {
        let nodes = self.nodes.read().await;
        let unhealthy = self.unhealthy.read().await;
        nodes
            .values()
            .filter(|n| {
                !n.is_stale()
                    && !unhealthy.contains(&n.node_id)
                    && n.health_status != common::health::NodeHealthStatus::Unhealthy
            })
            .min_by(|a, b| {
                let cmp = a.fuel_used_percent.partial_cmp(&b.fuel_used_percent);
                cmp.unwrap_or(std::cmp::Ordering::Equal) // Treat NaN as equal (skip)
            })
            .cloned()
    }

    /// Update a node's health status from a NATS health event.
    pub async fn update_health(&self, node_id: &str, status: common::health::NodeHealthStatus) {
        let mut table = self.nodes.write().await;
        if let Some(node) = table.get_mut(node_id) {
            node.health_status = status;
        }
    }

    /// Mark a node as unhealthy (e.g., under memory/I/O pressure).
    /// The node will be excluded from routing decisions until `mark_healthy` is called.
    pub async fn mark_unhealthy(&self, node_id: &str) {
        let was_healthy = !self.unhealthy.read().await.contains(node_id);
        if was_healthy {
            self.unhealthy.write().await.insert(node_id.to_string());
            tracing::warn!(
                node = node_id,
                "node marked unhealthy — excluded from routing"
            );
        }
    }

    /// Mark a node as healthy again (e.g., pressure recovered).
    /// The node will be included in routing decisions again.
    pub async fn mark_healthy(&self, node_id: &str) {
        let removed = self.unhealthy.write().await.remove(node_id);
        if removed {
            tracing::info!(node = node_id, "node marked healthy — restored in routing");
        }
    }

    /// Check if a node is currently marked as unhealthy.
    pub async fn is_unhealthy(&self, node_id: &str) -> bool {
        self.unhealthy.read().await.contains(node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_node_load_table() {
        let table = NodeLoadTable::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let node1 = NodeEntry {
            node_id: "node-1".to_string(),
            supervisor_addr: "127.0.0.1:9001".parse().unwrap(),
            fuel_used_percent: 50.0,
            active_instances: 5,
            last_seen: now,
            health_status: common::health::NodeHealthStatus::Healthy,
        };

        let node2 = NodeEntry {
            node_id: "node-2".to_string(),
            supervisor_addr: "127.0.0.1:9002".parse().unwrap(),
            fuel_used_percent: 20.0,
            active_instances: 2,
            last_seen: now,
            health_status: common::health::NodeHealthStatus::Healthy,
        };

        let node3 = NodeEntry {
            node_id: "node-3".to_string(),
            supervisor_addr: "127.0.0.1:9003".parse().unwrap(),
            fuel_used_percent: 10.0,
            active_instances: 1,
            last_seen: now - 40,
            health_status: common::health::NodeHealthStatus::Healthy,
        };

        table.update(node1).await;
        table.update(node2).await;
        table.update(node3).await;

        let least_loaded = table.least_loaded_node().await.expect("Should find a node");
        assert_eq!(least_loaded.node_id, "node-2");
    }

    #[tokio::test]
    async fn test_mark_unhealthy_excludes_from_routing() {
        let table = NodeLoadTable::default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let node1 = NodeEntry {
            node_id: "node-1".to_string(),
            supervisor_addr: "127.0.0.1:9001".parse().unwrap(),
            fuel_used_percent: 20.0,
            active_instances: 2,
            last_seen: now,
            health_status: common::health::NodeHealthStatus::Healthy,
        };

        let node2 = NodeEntry {
            node_id: "node-2".to_string(),
            supervisor_addr: "127.0.0.1:9002".parse().unwrap(),
            fuel_used_percent: 50.0,
            active_instances: 5,
            last_seen: now,
            health_status: common::health::NodeHealthStatus::Healthy,
        };

        table.update(node1).await;
        table.update(node2).await;

        // node-1 is least loaded
        let least = table.least_loaded_node().await.unwrap();
        assert_eq!(least.node_id, "node-1");

        // Mark node-1 as unhealthy
        table.mark_unhealthy("node-1").await;
        assert!(table.is_unhealthy("node-1").await);
        assert!(!table.is_unhealthy("node-2").await);

        // Now node-2 should be the least loaded (node-1 excluded)
        let least = table.least_loaded_node().await.unwrap();
        assert_eq!(least.node_id, "node-2");

        // Mark node-1 as healthy again
        table.mark_healthy("node-1").await;
        assert!(!table.is_unhealthy("node-1").await);

        // node-1 is back in the routing table
        let least = table.least_loaded_node().await.unwrap();
        assert_eq!(least.node_id, "node-1");
    }

    #[tokio::test]
    async fn test_mark_unhealthy_idempotent() {
        let table = NodeLoadTable::default();
        table.mark_unhealthy("node-1").await;
        table.mark_unhealthy("node-1").await; // Should not panic or double-insert
        assert!(table.is_unhealthy("node-1").await);
    }

    #[tokio::test]
    async fn test_mark_healthy_nonexistent() {
        let table = NodeLoadTable::default();
        table.mark_healthy("nonexistent").await; // Should not panic
        assert!(!table.is_unhealthy("nonexistent").await);
    }
}
