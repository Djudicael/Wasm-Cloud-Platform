use common::types::AppId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Result of resolving a route against a host + path.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    /// The application that should handle this request.
    pub app_id: AppId,
    /// Whether to strip the matched path prefix before forwarding.
    pub strip_prefix: bool,
    /// The path prefix that was matched (for the proxy to strip if needed).
    pub matched_prefix: String,
}

/// A single route entry: maps a (host, path_prefix) pair to an app.
#[derive(Debug, Clone)]
struct RouteEntry {
    app_id: AppId,
    path_prefix: String,
    strip_prefix: bool,
}

/// Maps (host, path_prefix) → AppId with longest-prefix-match resolution.
///
/// Resolution order:
/// 1. Exact host match
/// 2. Strip "www." prefix and retry
/// 3. Among all entries for the matched host, find the longest `path_prefix`
///    that is a prefix of the request path
///
/// Example:
///   Route 1: host="api.myapp.com", path_prefix="/v1" → app-v1
///   Route 2: host="api.myapp.com", path_prefix="/v2" → app-v2
///   Route 3: host="api.myapp.com", path_prefix="/"   → app-default
///
///   GET api.myapp.com/v1/users  → app-v1  (prefix "/v1" matches)
///   GET api.myapp.com/v2/items  → app-v2  (prefix "/v2" matches)
///   GET api.myapp.com/other     → app-default (only "/" matches)
#[derive(Clone, Default)]
pub struct HostRouter {
    /// host → list of route entries (kept sorted by path_prefix length, longest first)
    routes: Arc<RwLock<HashMap<String, Vec<RouteEntry>>>>,
}

impl HostRouter {
    /// Add a route mapping `(host, path_prefix) → app_id`.
    ///
    /// If `path_prefix` is empty it defaults to `"/"`.
    /// If a route with the same `(host, path_prefix)` already exists it is
    /// replaced (upsert semantics).
    pub async fn add_route(
        &self,
        host: String,
        path_prefix: String,
        app_id: AppId,
        strip_prefix: bool,
    ) {
        let prefix = if path_prefix.is_empty() {
            "/".to_string()
        } else {
            path_prefix
        };

        let mut routes = self.routes.write().await;
        let entries = routes.entry(host).or_default();

        // Remove any existing entry with the same prefix (upsert semantics)
        entries.retain(|e| e.path_prefix != prefix);

        entries.push(RouteEntry {
            app_id,
            path_prefix: prefix,
            strip_prefix,
        });

        // Sort by path_prefix length descending (longest first) for efficient lookup
        entries.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));
    }

    /// Remove a route by host and path_prefix.
    pub async fn remove_route(&self, host: &str, path_prefix: &str) {
        let mut routes = self.routes.write().await;
        if let Some(entries) = routes.get_mut(host) {
            entries.retain(|e| e.path_prefix != path_prefix);
            if entries.is_empty() {
                routes.remove(host);
            }
        }
    }

    /// Remove all routes for a given host.
    pub async fn remove_host(&self, host: &str) {
        self.routes.write().await.remove(host);
    }

    /// Resolve a request to its target app by host and path.
    ///
    /// Uses longest-prefix-match on the path among all routes for the matched
    /// host.  Falls back to stripping `"www."` from the host if no exact match
    /// is found.
    pub async fn resolve(&self, host: &str, path: &str) -> Option<ResolvedRoute> {
        let routes = self.routes.read().await;

        // Try exact host match, then www-stripped
        let host_candidates = [host, host.trim_start_matches("www.")];

        for h in &host_candidates {
            if let Some(entries) = routes.get(*h) {
                // Entries are sorted longest-prefix-first, so the first match
                // is the most specific one.
                for entry in entries {
                    if path.starts_with(&entry.path_prefix) {
                        return Some(ResolvedRoute {
                            app_id: entry.app_id.clone(),
                            strip_prefix: entry.strip_prefix,
                            matched_prefix: entry.path_prefix.clone(),
                        });
                    }
                }
            }
        }

        None
    }

    /// Load all routes from the storage layer.
    pub async fn load_routes_from_store(&self, store: &storage::Store) {
        match store.list_routes() {
            Ok(route_list) => {
                let mut map = self.routes.write().await;
                map.clear();
                for r in route_list {
                    let prefix = if r.path_prefix.is_empty() {
                        "/".to_string()
                    } else {
                        r.path_prefix
                    };
                    let entries = map.entry(r.host).or_default();
                    entries.push(RouteEntry {
                        app_id: r.app_id,
                        path_prefix: prefix,
                        strip_prefix: r.strip_prefix,
                    });
                }
                // Sort each host's entries by prefix length (longest first)
                for entries in map.values_mut() {
                    entries.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));
                }
                tracing::info!(count = map.len(), "routes loaded from storage");
            }
            Err(e) => tracing::error!(error = %e, "failed to load routes"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_host_only_routing() {
        let router = HostRouter::default();
        router
            .add_route(
                "api.myapp.com".to_string(),
                "/".to_string(),
                AppId("api:v1".to_string()),
                false,
            )
            .await;

        let resolved = router.resolve("api.myapp.com", "/anything").await;
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().app_id.0, "api:v1");
    }

    #[tokio::test]
    async fn test_path_based_routing() {
        let router = HostRouter::default();
        router
            .add_route(
                "api.myapp.com".to_string(),
                "/v1".to_string(),
                AppId("api-v1:v1".to_string()),
                false,
            )
            .await;
        router
            .add_route(
                "api.myapp.com".to_string(),
                "/v2".to_string(),
                AppId("api-v2:v1".to_string()),
                false,
            )
            .await;
        router
            .add_route(
                "api.myapp.com".to_string(),
                "/".to_string(),
                AppId("api-default:v1".to_string()),
                false,
            )
            .await;

        // /v1/users → api-v1
        let r = router.resolve("api.myapp.com", "/v1/users").await.unwrap();
        assert_eq!(r.app_id.0, "api-v1:v1");

        // /v2/items → api-v2
        let r = router.resolve("api.myapp.com", "/v2/items").await.unwrap();
        assert_eq!(r.app_id.0, "api-v2:v1");

        // /other → default
        let r = router.resolve("api.myapp.com", "/other").await.unwrap();
        assert_eq!(r.app_id.0, "api-default:v1");
    }

    #[tokio::test]
    async fn test_longest_prefix_wins() {
        let router = HostRouter::default();
        router
            .add_route(
                "app.com".to_string(),
                "/api".to_string(),
                AppId("api:v1".to_string()),
                false,
            )
            .await;
        router
            .add_route(
                "app.com".to_string(),
                "/api/v2".to_string(),
                AppId("api-v2:v1".to_string()),
                false,
            )
            .await;

        // /api/v2/endpoint should match /api/v2, not /api
        let r = router.resolve("app.com", "/api/v2/endpoint").await.unwrap();
        assert_eq!(r.app_id.0, "api-v2:v1");

        // /api/v1/endpoint should match /api
        let r = router.resolve("app.com", "/api/v1/endpoint").await.unwrap();
        assert_eq!(r.app_id.0, "api:v1");
    }

    #[tokio::test]
    async fn test_strip_prefix() {
        let router = HostRouter::default();
        router
            .add_route(
                "app.com".to_string(),
                "/api".to_string(),
                AppId("api:v1".to_string()),
                true,
            )
            .await;

        let r = router.resolve("app.com", "/api/users").await.unwrap();
        assert!(r.strip_prefix);
        assert_eq!(r.matched_prefix, "/api");
    }

    #[tokio::test]
    async fn test_www_stripping() {
        let router = HostRouter::default();
        router
            .add_route(
                "myapp.com".to_string(),
                "/".to_string(),
                AppId("app:v1".to_string()),
                false,
            )
            .await;

        let r = router.resolve("www.myapp.com", "/").await.unwrap();
        assert_eq!(r.app_id.0, "app:v1");
    }

    #[tokio::test]
    async fn test_unknown_host_returns_none() {
        let router = HostRouter::default();
        let r = router.resolve("unknown.com", "/").await;
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn test_remove_route() {
        let router = HostRouter::default();
        router
            .add_route(
                "app.com".to_string(),
                "/v1".to_string(),
                AppId("v1:v1".to_string()),
                false,
            )
            .await;
        router
            .add_route(
                "app.com".to_string(),
                "/v2".to_string(),
                AppId("v2:v1".to_string()),
                false,
            )
            .await;

        router.remove_route("app.com", "/v1").await;

        let r = router.resolve("app.com", "/v1/users").await;
        assert!(r.is_none());

        // /v2 should still work
        let r = router.resolve("app.com", "/v2/users").await;
        assert!(r.is_some());
    }

    #[tokio::test]
    async fn test_upsert_semantics() {
        let router = HostRouter::default();
        router
            .add_route(
                "app.com".to_string(),
                "/api".to_string(),
                AppId("old:v1".to_string()),
                false,
            )
            .await;
        router
            .add_route(
                "app.com".to_string(),
                "/api".to_string(),
                AppId("new:v1".to_string()),
                false,
            )
            .await;

        let r = router.resolve("app.com", "/api/users").await.unwrap();
        assert_eq!(r.app_id.0, "new:v1");
    }

    #[tokio::test]
    async fn test_empty_prefix_defaults_to_root() {
        let router = HostRouter::default();
        router
            .add_route(
                "app.com".to_string(),
                "".to_string(), // empty → treated as "/"
                AppId("root:v1".to_string()),
                false,
            )
            .await;

        let r = router.resolve("app.com", "/anything").await.unwrap();
        assert_eq!(r.app_id.0, "root:v1");
    }
}
