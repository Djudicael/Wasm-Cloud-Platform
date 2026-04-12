use common::types::AppId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Maps Host header values to AppIds.
/// e.g. "api.myapp.com" → AppId("api-users:v2")
#[derive(Clone, Default)]
pub struct HostRouter {
    routes: Arc<RwLock<HashMap<String, AppId>>>,
}

impl HostRouter {
    pub async fn add_route(&self, host: String, app_id: AppId) {
        self.routes.write().await.insert(host, app_id);
    }

    pub async fn resolve(&self, host: &str) -> Option<AppId> {
        let routes = self.routes.read().await;
        // Exact match first
        if let Some(id) = routes.get(host) {
            return Some(id.clone());
        }
        // Wildcard: strip "www." prefix
        let bare = host.trim_start_matches("www.");
        routes.get(bare).cloned()
    }

    pub async fn remove_route(&self, host: &str) {
        self.routes.write().await.remove(host);
    }

    pub async fn load_from_store<S: IntoIterator<Item = (String, AppId)>>(&self, store: S) {
        let mut routes = self.routes.write().await;
        for (host, app_id) in store {
            routes.insert(host, app_id);
        }
    }
}
