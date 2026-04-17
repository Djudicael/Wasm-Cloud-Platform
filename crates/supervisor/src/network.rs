use common::types::AppId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Registry of all running instances and their host addresses.
#[derive(Clone, Default)]
pub struct LocalServiceRegistry {
    /// app_id -> list of (instance_id, socket_addr)
    entries: Arc<RwLock<HashMap<String, Vec<SocketAddr>>>>,
}

impl LocalServiceRegistry {
    pub async fn register(&self, app_id: &AppId, addr: SocketAddr) {
        let mut map = self.entries.write().await;
        map.entry(app_id.0.clone()).or_default().push(addr);
    }

    pub async fn deregister(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.entries.write().await;
        if let Some(addrs) = map.get_mut(&app_id.0) {
            addrs.retain(|a| a != addr);
            if addrs.is_empty() {
                map.remove(&app_id.0);
            }
        }
    }

    /// Get the best address for an app (round-robin or least-loaded).
    pub async fn resolve(&self, app_id: &AppId) -> Option<SocketAddr> {
        let map = self.entries.read().await;
        map.get(&app_id.0)?.first().copied()
    }

    /// Get all registered service addresses as a map.
    pub async fn get_all_services(&self) -> HashMap<String, Vec<SocketAddr>> {
        let map = self.entries.read().await;
        map.clone()
    }
}
