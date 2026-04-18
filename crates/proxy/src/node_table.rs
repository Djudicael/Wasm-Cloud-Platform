// crates/proxy/src/node_table.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
}

impl NodeLoadTable {
    pub async fn update(&self, entry: NodeEntry) {
        self.nodes
            .write()
            .await
            .insert(entry.node_id.clone(), entry);
    }

    /// Find the least-loaded node for an app.
    pub async fn least_loaded_node(&self) -> Option<NodeEntry> {
        let nodes = self.nodes.read().await;
        nodes
            .values()
            .filter(|n| !n.is_stale())
            .min_by(|a, b| {
                a.fuel_used_percent
                    .partial_cmp(&b.fuel_used_percent)
                    .unwrap()
            })
            .cloned()
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
        };

        let node2 = NodeEntry {
            node_id: "node-2".to_string(),
            supervisor_addr: "127.0.0.1:9002".parse().unwrap(),
            fuel_used_percent: 20.0,
            active_instances: 2,
            last_seen: now,
        };

        let node3 = NodeEntry {
            node_id: "node-3".to_string(),
            supervisor_addr: "127.0.0.1:9003".parse().unwrap(),
            fuel_used_percent: 10.0,
            active_instances: 1,
            last_seen: now - 40,
        };

        table.update(node1).await;
        table.update(node2).await;
        table.update(node3).await;

        let least_loaded = table.least_loaded_node().await.expect("Should find a node");
        assert_eq!(least_loaded.node_id, "node-2");
    }
}
