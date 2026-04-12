use common::types::AppId;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::RwLock;

/// Thread-safe registry of all live instance addresses, per app.
#[derive(Clone, Default)]
pub struct UpstreamRegistry {
    /// app_id → (round-robin counter, list of addresses)
    pub inner: Arc<RwLock<HashMap<String, (AtomicUsize, Vec<SocketAddr>)>>>,
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new upstream instance address for the given app.
    pub async fn add(&self, app_id: &AppId, addr: SocketAddr) {
        let mut map = self.inner.write().await;
        let entry = map
            .entry(app_id.0.clone())
            .or_insert_with(|| (AtomicUsize::new(0), Vec::new()));
        if !entry.1.contains(&addr) {
            entry.1.push(addr);
            tracing::info!(app = %app_id.0, %addr, "upstream added");
        }
    }

    /// Remove an upstream instance address for the given app.
    pub async fn remove(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.inner.write().await;
        if let Some(entry) = map.get_mut(&app_id.0) {
            entry.1.retain(|a| a != addr);
            tracing::info!(app = %app_id.0, %addr, "upstream removed");
        }
    }

    /// Get the next upstream address using round-robin.
    /// Returns None if no instances are available (cold start needed).
    pub async fn next(&self, app_id: &AppId) -> Option<SocketAddr> {
        let map = self.inner.read().await;
        let (counter, addrs) = map.get(&app_id.0)?;
        if addrs.is_empty() {
            return None;
        }
        let idx = counter.fetch_add(1, Ordering::Relaxed) % addrs.len();
        Some(addrs[idx])
    }

    /// Get the number of live upstream instances for the given app.
    pub async fn count(&self, app_id: &AppId) -> usize {
        let map = self.inner.read().await;
        map.get(&app_id.0).map(|(_, v)| v.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::UpstreamRegistry;
    use common::types::AppId;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn test_upstream_registry_round_robin() {
        let registry = UpstreamRegistry::default();
        let app_id = AppId("test-app".to_string());

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8081);
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8082);
        let addr3 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8083);

        registry.add(&app_id, addr1).await;
        registry.add(&app_id, addr2).await;
        registry.add(&app_id, addr3).await;

        assert_eq!(registry.count(&app_id).await, 3);

        // Call next 6 times, should cycle through addr1, addr2, addr3 twice
        assert_eq!(registry.next(&app_id).await, Some(addr1));
        assert_eq!(registry.next(&app_id).await, Some(addr2));
        assert_eq!(registry.next(&app_id).await, Some(addr3));
        assert_eq!(registry.next(&app_id).await, Some(addr1));
        assert_eq!(registry.next(&app_id).await, Some(addr2));
        assert_eq!(registry.next(&app_id).await, Some(addr3));
    }

    #[tokio::test]
    async fn test_upstream_registry_remove_and_empty() {
        let registry = UpstreamRegistry::default();
        let app_id = AppId("test-app".to_string());

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8081);

        // Empty pool returns None
        assert_eq!(registry.next(&app_id).await, None);

        // Add and verify
        registry.add(&app_id, addr1).await;
        assert_eq!(registry.next(&app_id).await, Some(addr1));

        // Remove and verify empty again
        registry.remove(&app_id, &addr1).await;
        assert_eq!(registry.next(&app_id).await, None);
        assert_eq!(registry.count(&app_id).await, 0);
    }
}
