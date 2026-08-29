#[cfg(test)]
mod test_helpers {
    use crate::{events::Event, NatsBus};
    use common::container_runtime::{reserve_host_port, NatsContainer};
    use common::types::{AppConfig, AppId};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use tokio_stream::StreamExt;

    #[test]
    fn secret_delete_subject_is_node_targeted() {
        let event = Event::SecretDelete {
            app_id: AppId::new("default/app", "v1"),
            key: "API_KEY".to_string(),
            target_node_id: "node-2".to_string(),
        };
        assert_eq!(event.subject(), "secrets.delete.default/app:v1.node-2");
        let encoded = serde_json::to_vec(&event).unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("secret-value"));
        assert!(matches!(
            serde_json::from_slice::<Event>(&encoded).unwrap(),
            Event::SecretDelete { target_node_id, .. } if target_node_id == "node-2"
        ));
    }

    #[tokio::test]
    async fn test_pub_sub_deploy_app() {
        let _container = NatsContainer::start(4224).expect("Failed to start NATS container");
        let url = _container.url.clone();
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");

        let (tx, mut rx) = mpsc::channel(1);

        // 1. Subscribe to the exact subject
        bus.subscribe("deploy.app.new", move |event| {
            let tx = tx.clone();
            async move {
                tx.send(event).await.unwrap();
            }
        })
        .await
        .unwrap();

        // Give the NATS server a few milliseconds to register the subscription
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 2. Publish an event
        let app_id = AppId::new("test-app", "v1");
        let event = Event::DeployApp {
            app_id: app_id.clone(),
            config: AppConfig::default_for(app_id),
            artifact_url: "http://example.com/test.wasm".to_string(),
            artifact_transfer_manifests: vec![],
            expected_hash: None,
            size_bytes: 0,
        };

        bus.publish(&event).await.unwrap();

        // 3. Wait for the event to arrive at the subscriber
        let received = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out waiting for NATS message")
            .expect("Channel closed");

        // 4. Verify the contents
        match received {
            Event::DeployApp {
                app_id: recv_id,
                artifact_url,
                ..
            } => {
                assert_eq!(recv_id.0, "test-app:v1");
                assert_eq!(artifact_url, "http://example.com/test.wasm");
            }
            _ => panic!("Received unexpected event variant"),
        }
    }

    #[tokio::test]
    async fn test_jetstream_durable_replay() {
        let _container = NatsContainer::start(4223).expect("Failed to start NATS container");
        let url = _container.url.clone();
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");

        // 1. Set up JetStream (creates the DEPLOY stream if it doesn't exist)
        bus.setup_jetstream().await.unwrap();

        // 2. Publish an event BEFORE the durable consumer is created
        let app_id = AppId::new("durable-app", "v1");
        let event = Event::DeployApp {
            app_id: app_id.clone(),
            config: AppConfig::default_for(app_id.clone()),
            artifact_url: "http://example.com/durable.wasm".to_string(),
            artifact_transfer_manifests: vec![],
            expected_hash: None,
            size_bytes: 0,
        };

        bus.publish(&event).await.unwrap();

        // Give JetStream a moment to persist the message
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (tx, mut rx) = mpsc::channel(10);

        // Generate a random consumer name so this test doesn't conflict with previous runs
        let consumer_name = format!("test_consumer_{}", uuid::Uuid::new_v4().simple());

        // 3. Create the durable consumer
        bus.subscribe_durable("DEPLOY", &consumer_name, Some("deploy.>"), move |event| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(event).await; // Ignore send errors if test already finished
                Ok::<(), common::error::PlatformError>(())
            }
        })
        .await
        .unwrap();

        // 4. Wait for the replay of the missed message
        let received = timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("Timed out waiting for JetStream to replay the message")
            .expect("Channel closed");

        match received {
            Event::DeployApp {
                app_id: recv_id,
                artifact_url,
                ..
            } => {
                assert_eq!(recv_id.0, "durable-app:v1");
                assert_eq!(artifact_url, "http://example.com/durable.wasm");
            }
            _ => panic!("Received unexpected event variant from JetStream"),
        }
    }

    #[tokio::test]
    async fn test_jetstream_handler_error_redelivery() {
        let _container = NatsContainer::start(4225).expect("Failed to start NATS container");
        let url = _container.url.clone();
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");
        bus.setup_jetstream().await.unwrap();

        let app_id = AppId::new("retry-app", "v1");
        let event = Event::DeployApp {
            app_id: app_id.clone(),
            config: AppConfig::default_for(app_id),
            artifact_url: "http://example.com/retry.wasm".to_string(),
            artifact_transfer_manifests: vec![],
            expected_hash: None,
            size_bytes: 0,
        };
        bus.publish(&event).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (tx, mut rx) = mpsc::channel(10);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let consumer_name = format!("retry_consumer_{}", uuid::Uuid::new_v4().simple());

        bus.subscribe_durable("DEPLOY", &consumer_name, Some("deploy.>"), move |event| {
            let tx = tx.clone();
            let attempts = attempts_for_handler.clone();
            async move {
                let current = attempts.fetch_add(1, Ordering::SeqCst);
                if current == 0 {
                    Err(common::error::PlatformError::messaging(
                        "transient handler failure",
                    ))
                } else {
                    let _ = tx.send(event).await;
                    Ok(())
                }
            }
        })
        .await
        .unwrap();

        let received = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out waiting for redelivery")
            .expect("Channel closed");

        assert!(attempts.load(Ordering::SeqCst) >= 2);
        match received {
            Event::DeployApp {
                app_id: recv_id, ..
            } => {
                assert_eq!(recv_id.0, "retry-app:v1");
            }
            _ => panic!("Received unexpected event variant from JetStream redelivery"),
        }
    }

    #[tokio::test]
    async fn test_poison_handler_failure_is_quarantined_after_retry_exhaustion() {
        let host_port = reserve_host_port().expect("reserve host port");
        let _container = NatsContainer::start(host_port).expect("Failed to start NATS container");
        let url = format!("nats://127.0.0.1:{host_port}");
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");
        bus.setup_jetstream().await.unwrap();

        let mut quarantine_sub = bus
            .client()
            .subscribe("quarantine.>".to_string())
            .await
            .expect("failed to subscribe to quarantine subject");

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let consumer_name = format!("poison_consumer_{}", uuid::Uuid::new_v4().simple());
        bus.subscribe_durable("DEPLOY", &consumer_name, Some("deploy.>"), move |_event| {
            let attempts = attempts_for_handler.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err::<(), common::error::PlatformError>(common::error::PlatformError::messaging(
                    "permanent handler failure",
                ))
            }
        })
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let app_id = AppId::new("poison-app", "v1");
        let event = Event::DeployApp {
            app_id: app_id.clone(),
            config: AppConfig::default_for(app_id),
            artifact_url: "http://example.com/poison.wasm".to_string(),
            artifact_transfer_manifests: vec![],
            expected_hash: None,
            size_bytes: 0,
        };
        bus.publish(&event).await.unwrap();

        let quarantine_msg = timeout(Duration::from_secs(10), quarantine_sub.next())
            .await
            .expect("timed out waiting for quarantined message")
            .expect("quarantine subscription ended");
        let record: serde_json::Value = serde_json::from_slice(&quarantine_msg.payload)
            .expect("quarantine payload should be JSON");

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(record["stream"], "DEPLOY");
        assert_eq!(record["consumer"], consumer_name);
        assert_eq!(record["original_subject"], "deploy.app.new");
        assert_eq!(record["delivered"], 3);
        assert!(record["reason"]
            .as_str()
            .unwrap()
            .contains("permanent handler failure"));
        assert!(record["payload"].as_array().unwrap().len() > 10);
    }

    #[tokio::test]
    async fn test_malformed_message_does_not_block_other_filtered_consumers() {
        let _container = NatsContainer::start(4226).expect("Failed to start NATS container");
        let url = _container.url.clone();
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");
        bus.setup_jetstream().await.unwrap();

        let (secret_tx, mut secret_rx) = mpsc::channel(4);
        let malformed_attempts = Arc::new(AtomicUsize::new(0));
        let malformed_attempts_for_handler = malformed_attempts.clone();

        let secret_consumer = format!("secret_consumer_{}", uuid::Uuid::new_v4().simple());
        bus.subscribe_durable(
            "CONTROL",
            &secret_consumer,
            Some("secrets.update.>"),
            move |event| {
                let secret_tx = secret_tx.clone();
                async move {
                    let _ = secret_tx.send(event).await;
                    Ok::<(), common::error::PlatformError>(())
                }
            },
        )
        .await
        .unwrap();

        let malformed_consumer = format!("malformed_consumer_{}", uuid::Uuid::new_v4().simple());
        bus.subscribe_durable(
            "CONTROL",
            &malformed_consumer,
            Some("config.update.>"),
            move |_event| {
                let malformed_attempts = malformed_attempts_for_handler.clone();
                async move {
                    malformed_attempts.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), common::error::PlatformError>(())
                }
            },
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        bus.client()
            .publish(
                "config.update.bad-app:v1".to_string(),
                b"{not-json".to_vec().into(),
            )
            .await
            .unwrap();

        let secret_event = Event::SecretUpdate {
            app_id: AppId::new("good-app", "v1"),
            key: "API_KEY".to_string(),
            target_node_id: None,
            secret: secrets::SecretTransportEnvelope::plaintext_utf8("super-secret"),
        };
        bus.publish(&secret_event).await.unwrap();

        let received = timeout(Duration::from_secs(5), secret_rx.recv())
            .await
            .expect("Timed out waiting for valid secret update delivery")
            .expect("Secret consumer channel closed");

        match received {
            Event::SecretUpdate { app_id, key, .. } => {
                assert_eq!(app_id.0, "good-app:v1");
                assert_eq!(key, "API_KEY");
            }
            other => panic!("Expected SecretUpdate event, got {other:?}"),
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            malformed_attempts.load(Ordering::SeqCst),
            0,
            "malformed payload should not reach the typed handler"
        );
    }

    #[tokio::test]
    async fn test_control_event_is_handled_exactly_once_by_intended_filtered_consumer() {
        let host_port = reserve_host_port().expect("reserve host port");
        let _container = NatsContainer::start(host_port).expect("Failed to start NATS container");
        let url = format!("nats://127.0.0.1:{host_port}");
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");
        bus.setup_jetstream().await.unwrap();

        let intended_hits = Arc::new(AtomicUsize::new(0));
        let sibling_hits = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel(1);

        let secret_consumer = format!("control_secret_{}", uuid::Uuid::new_v4().simple());
        let intended_hits_for_handler = intended_hits.clone();
        bus.subscribe_durable(
            "CONTROL",
            &secret_consumer,
            Some("secrets.update.>"),
            move |event| {
                let intended_hits = intended_hits_for_handler.clone();
                let tx = tx.clone();
                async move {
                    intended_hits.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(event).await;
                    Ok::<(), common::error::PlatformError>(())
                }
            },
        )
        .await
        .unwrap();

        for filter in [
            "instance.ready.>",
            "instance.dead.>",
            "config.update.>",
            "gateway.config.>",
        ] {
            let sibling_consumer = format!(
                "control_sibling_{}_{}",
                filter.replace('.', "_").replace('>', "all"),
                uuid::Uuid::new_v4().simple()
            );
            let sibling_hits_for_handler = sibling_hits.clone();
            bus.subscribe_durable("CONTROL", &sibling_consumer, Some(filter), move |_event| {
                let sibling_hits = sibling_hits_for_handler.clone();
                async move {
                    sibling_hits.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), common::error::PlatformError>(())
                }
            })
            .await
            .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = Event::SecretUpdate {
            app_id: AppId::new("exact-control", "v1"),
            key: "TOKEN".to_string(),
            target_node_id: None,
            secret: secrets::SecretTransportEnvelope::plaintext_utf8("value"),
        };
        bus.publish(&event).await.unwrap();

        let received = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out waiting for control event delivery")
            .expect("Control consumer channel closed");
        match received {
            Event::SecretUpdate { app_id, key, .. } => {
                assert_eq!(app_id.0, "exact-control:v1");
                assert_eq!(key, "TOKEN");
            }
            other => panic!("Expected SecretUpdate event, got {other:?}"),
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(intended_hits.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_node_event_is_handled_exactly_once_by_intended_filtered_consumer() {
        let host_port = reserve_host_port().expect("reserve host port");
        let _container = NatsContainer::start(host_port).expect("Failed to start NATS container");
        let url = format!("nats://127.0.0.1:{host_port}");
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");
        bus.setup_jetstream().await.unwrap();

        let intended_hits = Arc::new(AtomicUsize::new(0));
        let sibling_hits = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel(1);

        let snapshot_consumer = format!("node_snapshot_{}", uuid::Uuid::new_v4().simple());
        let intended_hits_for_handler = intended_hits.clone();
        bus.subscribe_durable(
            "NODE",
            &snapshot_consumer,
            Some("cluster.snapshot.>"),
            move |event| {
                let intended_hits = intended_hits_for_handler.clone();
                let tx = tx.clone();
                async move {
                    intended_hits.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(event).await;
                    Ok::<(), common::error::PlatformError>(())
                }
            },
        )
        .await
        .unwrap();

        for filter in ["node.load.>", "cluster.node_joined.>"] {
            let sibling_consumer = format!(
                "node_sibling_{}_{}",
                filter.replace('.', "_").replace('>', "all"),
                uuid::Uuid::new_v4().simple()
            );
            let sibling_hits_for_handler = sibling_hits.clone();
            bus.subscribe_durable("NODE", &sibling_consumer, Some(filter), move |_event| {
                let sibling_hits = sibling_hits_for_handler.clone();
                async move {
                    sibling_hits.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), common::error::PlatformError>(())
                }
            })
            .await
            .unwrap();
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = Event::StateSnapshot {
            for_node_id: "fresh-node".to_string(),
            bootstrap_session_id: "session-1".to_string(),
            bootstrap_nonce: "nonce-1".to_string(),
            configs: vec![],
            routes: vec![],
            encrypted_secrets: vec![],
            gateway_configs: vec![],
            api_keys: vec![],
            artifact_fetches: vec![],
            artifact_hashes: vec![],
        };
        bus.publish(&event).await.unwrap();

        let received = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Timed out waiting for node event delivery")
            .expect("Node consumer channel closed");
        match received {
            Event::StateSnapshot {
                for_node_id,
                bootstrap_session_id,
                ..
            } => {
                assert_eq!(for_node_id, "fresh-node");
                assert_eq!(bootstrap_session_id, "session-1");
            }
            other => panic!("Expected StateSnapshot event, got {other:?}"),
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(intended_hits.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_gateway_config_update_serialization() {
        use common::protocol::MessageEnvelope;
        use common::types::{EndpointAuth, EndpointRule, GatewayRouteConfig};

        let gw_config = GatewayRouteConfig {
            endpoints: vec![EndpointRule {
                path: "/echo".to_string(),
                methods: vec!["GET".to_string()],
                auth: EndpointAuth::None,
                required_scopes: vec![],
                rate_limit: None,
            }],
            ..Default::default()
        };

        let event = Event::GatewayConfigUpdate {
            app_id: AppId("echo-service:v1".to_string()),
            config: gw_config.clone(),
        };

        let envelope = MessageEnvelope::new("test-node", event.clone());
        let json = serde_json::to_string(&envelope).unwrap();
        println!("Serialized GatewayConfigUpdate: {}", json);

        let deserialized: MessageEnvelope<Event> = serde_json::from_str(&json).unwrap();
        match deserialized.payload {
            Event::GatewayConfigUpdate { app_id, config } => {
                assert_eq!(app_id.0, "echo-service:v1");
                assert_eq!(config, gw_config);
            }
            _ => panic!(
                "Expected GatewayConfigUpdate, got {:?}",
                deserialized.payload
            ),
        }
    }

    #[test]
    fn test_event_discriminants() {
        use common::types::GatewayRouteConfig;
        use secrets::SecretTransportEnvelope;
        let events: Vec<(&str, Event)> = vec![
            (
                "RemoveApp",
                Event::RemoveApp {
                    app_id: AppId("a".to_string()),
                },
            ),
            (
                "RouteAdd",
                Event::RouteAdd {
                    route: common::types::Route {
                        host: "".to_string(),
                        app_id: AppId("a".to_string()),
                        path_prefix: "".to_string(),
                        strip_prefix: false,
                        created_at: 0,
                        updated_at: 0,
                    },
                },
            ),
            (
                "RouteRemove",
                Event::RouteRemove {
                    host: "".to_string(),
                },
            ),
            (
                "InstanceReady",
                Event::InstanceReady {
                    app_id: AppId("a".to_string()),
                    addr: "127.0.0.1:1".parse().unwrap(),
                    node_id: "".to_string(),
                },
            ),
            (
                "InstanceDead",
                Event::InstanceDead {
                    app_id: AppId("a".to_string()),
                    addr: "127.0.0.1:1".parse().unwrap(),
                    node_id: "".to_string(),
                },
            ),
            (
                "SecretUpdate",
                Event::SecretUpdate {
                    app_id: AppId("a".to_string()),
                    key: "".to_string(),
                    target_node_id: None,
                    secret: SecretTransportEnvelope::plaintext_utf8(""),
                },
            ),
            (
                "GatewayConfigUpdate",
                Event::GatewayConfigUpdate {
                    app_id: AppId("a".to_string()),
                    config: GatewayRouteConfig::default(),
                },
            ),
            (
                "GatewayConfigRemove",
                Event::GatewayConfigRemove {
                    app_id: AppId("a".to_string()),
                },
            ),
            (
                "NodeLoad",
                Event::NodeLoad {
                    node_id: "".to_string(),
                    cpu_percent: 0.0,
                    fuel_budget_used_percent: 0.0,
                    active_instances: 0,
                    proxy_address: "127.0.0.1:8080".to_string(),
                },
            ),
            (
                "NodeJoined",
                Event::NodeJoined {
                    node_id: "".to_string(),
                    bootstrap_session_id: "session".to_string(),
                    bootstrap_nonce: "nonce".to_string(),
                    artifact_server_url: "".to_string(),
                    public_key_bytes: vec![],
                    protocol_version: 1,
                    binary_version: "".to_string(),
                },
            ),
            (
                "StateSnapshot",
                Event::StateSnapshot {
                    for_node_id: "".to_string(),
                    bootstrap_session_id: "session".to_string(),
                    bootstrap_nonce: "nonce".to_string(),
                    configs: vec![],
                    routes: vec![],
                    encrypted_secrets: vec![],
                    gateway_configs: vec![],
                    api_keys: vec![],
                    artifact_fetches: vec![],
                    artifact_hashes: vec![],
                },
            ),
        ];
        for (name, event) in events {
            println!("{} -> {:?}", name, std::mem::discriminant(&event));
        }
    }
}
