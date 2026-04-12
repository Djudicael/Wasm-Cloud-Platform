use common::types::AppId;
use std::net::SocketAddr;

/// Registry of upstream Wasm instances.
/// Used by Pingora (or the proxy layer) to route incoming requests.
#[derive(Default)]
pub struct UpstreamRegistry {
    // This will be expanded in the proxy implementation phase.
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new upstream instance address for the given app.
    pub async fn add(&self, app_id: &AppId, addr: SocketAddr) {
        tracing::debug!(app = %app_id.0, %addr, "added to upstream registry");
    }

    /// Remove an upstream instance address for the given app.
    pub async fn remove(&self, app_id: &AppId, addr: &SocketAddr) {
        tracing::debug!(app = %app_id.0, %addr, "removed from upstream registry");
    }
}
