use common::types::AppId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

type AppInstanceMap = HashMap<String, Vec<SocketAddr>>;
type NamespaceInstanceMap = HashMap<String, AppInstanceMap>;

/// Registry of running instances, scoped by namespace.
/// Replaces LocalServiceRegistry with namespace awareness and source-port attribution.
#[derive(Clone, Default)]
pub struct NamespaceRegistry {
    /// namespace → bare_app_name → list of SocketAddr
    /// The inner key is the bare app name WITHOUT version (e.g. "echo-service"),
    /// so service discovery resolves by app name, not specific version.
    instances: Arc<RwLock<NamespaceInstanceMap>>,

    /// Reverse lookup: source_port (TCP ephemeral port allocated to an instance) → AppId.
    /// Populated by the Supervisor when an instance is spawned.
    port_to_app: Arc<RwLock<HashMap<u16, AppId>>>,
}

impl NamespaceRegistry {
    /// Register an instance address for an app.
    pub async fn register(&self, app_id: &AppId, addr: SocketAddr) {
        let mut map = self.instances.write().await;
        map.entry(app_id.namespace().to_string())
            .or_default()
            .entry(app_id.bare_app_name().to_string())
            .or_default()
            .push(addr);
    }

    /// Deregister an instance address.
    pub async fn deregister(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.instances.write().await;
        if let Some(ns) = map.get_mut(app_id.namespace()) {
            let key = app_id.bare_app_name();
            if let Some(addrs) = ns.get_mut(key) {
                addrs.retain(|a| a != addr);
                if addrs.is_empty() {
                    ns.remove(key);
                }
            }
            if ns.is_empty() {
                map.remove(app_id.namespace());
            }
        }
    }

    /// Resolve a bare app name inside a given namespace to its local address.
    pub async fn resolve(&self, namespace: &str, bare_name: &str) -> Option<SocketAddr> {
        let map = self.instances.read().await;
        map.get(namespace)
            .and_then(|ns| ns.get(bare_name))
            .and_then(|addrs| addrs.first().copied())
    }

    /// Resolve an app by its destination port (for cross-namespace checks).
    /// Scans the instances map to find which app is listening on the given port.
    pub async fn resolve_app_by_port(&self, port: u16) -> Option<AppId> {
        let map = self.instances.read().await;
        for (ns, apps) in map.iter() {
            for (bare_app_name, addrs) in apps.iter() {
                if addrs.iter().any(|a| a.port() == port) {
                    return Some(AppId::new_namespaced(ns, bare_app_name, "v1"));
                }
            }
        }
        None
    }

    /// Register which app owns a given source port (for outbound call attribution).
    pub async fn bind_source_port(&self, port: u16, app_id: AppId) {
        self.port_to_app.write().await.insert(port, app_id);
    }

    /// Look up the app that owns a source port.
    pub async fn resolve_source_app(&self, port: u16) -> Option<AppId> {
        self.port_to_app.read().await.get(&port).cloned()
    }

    /// Unregister a source port when an instance stops.
    pub async fn release_source_port(&self, port: u16) {
        self.port_to_app.write().await.remove(&port);
    }

    /// Get all registered service addresses as a flat map (for compatibility).
    /// Keys are in format "{namespace}/{bare_app_name}".
    pub async fn get_all_services(&self) -> HashMap<String, Vec<SocketAddr>> {
        let mut result = HashMap::new();
        let map = self.instances.read().await;
        for (ns, apps) in map.iter() {
            for (bare_name, addrs) in apps.iter() {
                result.insert(format!("{ns}/{bare_name}"), addrs.clone());
            }
        }
        result
    }

    /// Get all services within a specific namespace, keyed by bare app name
    /// (without namespace prefix or version).
    /// Used by the Supervisor for service discovery env var injection.
    pub async fn get_namespace_services(
        &self,
        namespace: &str,
    ) -> HashMap<String, Vec<SocketAddr>> {
        let map = self.instances.read().await;
        map.get(namespace).cloned().unwrap_or_default()
    }
}

/// Backwards-compatible re-export as LocalServiceRegistry.
pub type LocalServiceRegistry = NamespaceRegistry;
