// crates/proxy/src/node_table.rs
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct NodeEntry {
    pub node_id: String,
    pub supervisor_addr: SocketAddr, // address of the supervisor's API
    pub fuel_used_percent: f32,
    pub active_instances: u32,
    pub last_seen: std::time::Instant,
}

#[derive(Clone, Default)]
pub struct NodeLoadTable {
    nodes: Arc<RwLock<HashMap<String, NodeEntry>>>,
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
        // Remove stale entries (not seen in 30s)
        nodes
            .values()
            .filter(|n| n.last_seen.elapsed().as_secs() < 30)
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
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn test_node_load_table() {
        let table = NodeLoadTable::default();

        let node1 = NodeEntry {
            node_id: "node-1".to_string(),
            supervisor_addr: "127.0.0.1:9001".parse().unwrap(),
            fuel_used_percent: 50.0,
            active_instances: 5,
            last_seen: Instant::now(),
        };

        let node2 = NodeEntry {
            node_id: "node-2".to_string(),
            supervisor_addr: "127.0.0.1:9002".parse().unwrap(),
            fuel_used_percent: 20.0, // Least loaded
            active_instances: 2,
            last_seen: Instant::now(),
        };

        let node3 = NodeEntry {
            node_id: "node-3".to_string(),
            supervisor_addr: "127.0.0.1:9003".parse().unwrap(),
            fuel_used_percent: 10.0, // Even less loaded, but stale
            active_instances: 1,
            last_seen: Instant::now() - Duration::from_secs(40), // Stale
        };

        table.update(node1).await;
        table.update(node2).await;
        table.update(node3).await;

        let least_loaded = table.least_loaded_node().await.expect("Should find a node");
        assert_eq!(least_loaded.node_id, "node-2"); // node-3 is stale, so node-2 is picked
    }
}
