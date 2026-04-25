use common::types::AppId;
use std::net::SocketAddr;
use std::sync::Arc;

/// Result of checking an outbound TCP connection.
#[derive(Debug)]
pub enum ConnectDecision {
    /// Allow the connection to the specified address.
    Allow(SocketAddr),
    /// Deny the connection (returns ECONNREFUSED to the Wasm module).
    Deny { reason: String },
}

/// Transparent network interceptor for East-West traffic.
/// Lives in the Supervisor and is wired into every instance's WasiCtx
/// via the `socket_addr_check` callback.
pub struct NetworkInterceptor {
    pub registry: Arc<super::network::NamespaceRegistry>,
    pub source_app: AppId,
}

impl NetworkInterceptor {
    pub fn new(registry: Arc<super::network::NamespaceRegistry>, source_app: AppId) -> Self {
        NetworkInterceptor {
            registry,
            source_app,
        }
    }

    /// Called for every outbound TCP connect.
    /// Returns the **rewritten** destination address, or denies the connection.
    pub async fn check_connect(
        &self,
        _source_addr: SocketAddr,
        dest_addr: SocketAddr,
    ) -> ConnectDecision {
        // 1. If the destination is on loopback, check if it's a known app port.
        if !dest_addr.ip().is_loopback() {
            // External connection — allow (external NetworkPolicy from Step 33
            // is enforced separately by the PolicyEnforcer).
            return ConnectDecision::Allow(dest_addr);
        }

        // 2. Check if the destination port belongs to a known local app.
        if let Some(target_app) = self.registry.resolve_app_by_port(dest_addr.port()).await {
            // Cross-namespace block
            if target_app.namespace() != self.source_app.namespace() {
                return ConnectDecision::Deny {
                    reason: format!(
                        "cross-namespace connection blocked: {} → {}",
                        self.source_app.namespace(),
                        target_app.namespace()
                    ),
                };
            }

            // Same namespace — allow direct connection.
            return ConnectDecision::Allow(dest_addr);
        }

        // 3. Unknown loopback destination — could be the internal proxy port
        // or some other local service. Allow for now; the internal proxy
        // will apply its own policies.
        ConnectDecision::Allow(dest_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_registry() -> (Arc<super::super::network::NamespaceRegistry>, AppId, AppId) {
        let registry = Arc::new(super::super::network::NamespaceRegistry::default());
        let app_a = AppId::new_namespaced("production", "payments", "v1");
        let app_b = AppId::new_namespaced("production", "api-b", "v1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            registry
                .register(&app_a, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10100))
                .await;
            registry
                .register(&app_b, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101))
                .await;
        });

        (registry, app_a, app_b)
    }

    #[test]
    fn test_same_namespace_allowed() {
        let (registry, app_a, _app_b) = test_registry();
        let interceptor = NetworkInterceptor::new(registry.clone(), app_a.clone());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(async {
            interceptor
                .check_connect(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 50000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await
        });

        match decision {
            ConnectDecision::Allow(addr) => assert_eq!(addr.port(), 10101),
            other => panic!("expected Allow, got {:?}", other),
        }
    }

    #[test]
    fn test_cross_namespace_denied() {
        let registry = Arc::new(super::super::network::NamespaceRegistry::default());
        let app_a = AppId::new_namespaced("ns1", "app-a", "v1");
        let app_b = AppId::new_namespaced("ns2", "app-b", "v1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            registry
                .register(&app_a, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10100))
                .await;
            registry
                .register(&app_b, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101))
                .await;
        });

        let interceptor = NetworkInterceptor::new(registry.clone(), app_a.clone());
        let decision = rt.block_on(async {
            interceptor
                .check_connect(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 50000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
                )
                .await
        });

        match decision {
            ConnectDecision::Deny { reason } => {
                assert!(reason.contains("cross-namespace connection blocked"));
            }
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn test_external_connection_allowed() {
        let registry = Arc::new(super::super::network::NamespaceRegistry::default());
        let app_a = AppId::new_namespaced("default", "app-a", "v1");

        let interceptor = NetworkInterceptor::new(registry, app_a);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(async {
            interceptor
                .check_connect(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 50000),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443),
                )
                .await
        });

        match decision {
            ConnectDecision::Allow(addr) => assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443)),
            other => panic!("expected Allow, got {:?}", other),
        }
    }
}
