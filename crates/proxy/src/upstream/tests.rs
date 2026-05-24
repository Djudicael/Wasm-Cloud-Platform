#[cfg(test)]
mod upstream_tests {
    use crate::upstream::{UpstreamEndpoint, UpstreamRegistry};
    use common::types::AppId;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn test_upstream_registry_round_robin() {
        let registry = UpstreamRegistry::default();
        let app_id = AppId("test-app".to_string());

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8081);
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8082);
        let addr3 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8083);

        registry
            .add(
                &app_id,
                UpstreamEndpoint {
                    addr: addr1,
                    h2c: false,
                },
            )
            .await;
        registry
            .add(
                &app_id,
                UpstreamEndpoint {
                    addr: addr2,
                    h2c: true,
                },
            )
            .await;
        registry
            .add(
                &app_id,
                UpstreamEndpoint {
                    addr: addr3,
                    h2c: false,
                },
            )
            .await;

        assert_eq!(registry.count(&app_id).await, 3);

        // Call next 6 times, should cycle through addr1, addr2, addr3 twice
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr1,
                h2c: false
            })
        );
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr2,
                h2c: true
            })
        );
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr3,
                h2c: false
            })
        );
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr1,
                h2c: false
            })
        );
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr2,
                h2c: true
            })
        );
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr3,
                h2c: false
            })
        );
    }

    #[tokio::test]
    async fn test_upstream_registry_remove_and_empty() {
        let registry = UpstreamRegistry::default();
        let app_id = AppId("test-app".to_string());

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8081);

        // Empty pool returns None
        assert_eq!(registry.next(&app_id).await, None);

        // Add and verify
        registry
            .add(
                &app_id,
                UpstreamEndpoint {
                    addr: addr1,
                    h2c: true,
                },
            )
            .await;
        assert_eq!(
            registry.next(&app_id).await,
            Some(UpstreamEndpoint {
                addr: addr1,
                h2c: true
            })
        );

        // Remove and verify empty again
        registry.remove(&app_id, &addr1).await;
        assert_eq!(registry.next(&app_id).await, None);
        assert_eq!(registry.count(&app_id).await, 0);
    }
}
