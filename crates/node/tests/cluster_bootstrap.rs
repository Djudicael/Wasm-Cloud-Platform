// crates/node/tests/cluster_bootstrap.rs
//! Integration tests for cluster bootstrap and new node state synchronization.

use common::{
    artifact_transfer::{ArtifactTransferAuthority, BootstrapArtifactFetchAuthorization},
    types::{AppConfig, AppId, FuelQuota, MemoryPages},
};
use e2e::NatsContainer;
use messaging::events::Event;
use secrets::{
    BootstrapKeyPair, SecretProvider, SecretTransportEntry, SecretTransportEnvelope,
    SecretTransportPayload,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use storage::Store;
use tokio::time::sleep;

static NATS_PORT_COUNTER: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

fn is_loopback_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .map(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Helper to create a test AppConfig
fn create_test_app_config(app_id: &str) -> AppConfig {
    AppConfig {
        id: AppId(app_id.to_string()),
        fuel_quota: FuelQuota(500_000_000),
        memory_limit: MemoryPages(16), // 1MB
        max_instances: 2,
        idle_timeout_secs: 60,
        wasm_bind_port: 8080,
        env_vars: HashMap::new(),
        secret_keys: vec![],
        extended_limits: None,
        health_check_path: None,
        db_max_connections: None,
        rate_limit: None,
        tenant_id: None,
        policy: None,
        namespace: "default".to_string(),
        placement: common::types::PlacementPolicy::EveryNode,
        local_dependencies: Vec::new(),
    }
}

fn allocate_nats_port() -> u16 {
    use std::sync::atomic::Ordering;
    let base = 24000 + ((std::process::id() as u16) % 1000);
    base + NATS_PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

async fn start_test_nats() -> Result<NatsContainer, String> {
    NatsContainer::start(allocate_nats_port()).await
}

#[tokio::test]
async fn test_fresh_node_publishes_node_joined() {
    let nats = start_test_nats().await.unwrap();
    let nats_url = nats.url.clone();
    sleep(Duration::from_millis(100)).await;

    // Create a fresh node (empty storage)
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("node0.db");
    let store = Store::open(&db_path).unwrap();

    // Verify storage is empty
    assert!(store.list_apps().unwrap().is_empty());

    // Connect to NATS
    let bus = messaging::NatsBus::connect(&nats_url).await.unwrap();
    bus.setup_jetstream().await.unwrap();

    // Subscribe to node_joined events
    let join_events: Vec<Event> = Vec::new();

    bus.subscribe("cluster.node_joined.>", move |event| {
        let _events = join_events.clone();
        async move {
            if let Event::NodeJoined {
                node_id,
                bootstrap_session_id,
                bootstrap_nonce,
                artifact_server_url,
                public_key_bytes,
                protocol_version: _,
                binary_version: _,
            } = event
            {
                println!(
                    "Received NodeJoined: node_id={}, session={}, nonce={}, url={}, pubkey_len={}",
                    node_id,
                    bootstrap_session_id,
                    bootstrap_nonce,
                    artifact_server_url,
                    public_key_bytes.len()
                );
            }
        }
    })
    .await
    .unwrap();

    // Generate bootstrap keypair
    let keypair = BootstrapKeyPair::generate();
    let public_key_bytes = keypair.public_bytes();

    // Publish NodeJoined event
    let join_event = Event::NodeJoined {
        node_id: "node-0".to_string(),
        bootstrap_session_id: "session-node-0".to_string(),
        bootstrap_nonce: "nonce-node-0".to_string(),
        artifact_server_url: "http://127.0.0.1:8080".to_string(),
        public_key_bytes: public_key_bytes.clone(),
        protocol_version: common::protocol::PROTOCOL_VERSION,
        binary_version: common::protocol::BINARY_VERSION.to_string(),
    };

    bus.publish(&join_event).await.unwrap();
    sleep(Duration::from_millis(200)).await;

    println!("✓ Fresh node successfully published NodeJoined event");
}

#[tokio::test]
async fn test_existing_node_skips_bootstrap() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("node1.db");
    let store = Store::open(&db_path).unwrap();

    // Add an app to storage (simulating existing node)
    let config = create_test_app_config("test-app:v1");
    store.save_config(&config).unwrap();

    // Verify storage is not empty
    assert!(!store.list_apps().unwrap().is_empty());

    println!("✓ Existing node has data in storage and should skip bootstrap");
}

#[tokio::test]
async fn test_bootstrap_session_correlates_join_and_snapshot() {
    let nats = start_test_nats().await.unwrap();
    let nats_url = nats.url.clone();
    sleep(Duration::from_millis(100)).await;

    let bus = messaging::NatsBus::connect(&nats_url).await.unwrap();
    bus.setup_jetstream().await.unwrap();

    let join_event = Event::NodeJoined {
        node_id: "node-new".to_string(),
        bootstrap_session_id: "bootstrap-session-1".to_string(),
        bootstrap_nonce: "bootstrap-nonce-1".to_string(),
        artifact_server_url: "http://node-new.internal:8081".to_string(),
        public_key_bytes: BootstrapKeyPair::generate().public_bytes(),
        protocol_version: common::protocol::PROTOCOL_VERSION,
        binary_version: common::protocol::BINARY_VERSION.to_string(),
    };

    let snapshot = Event::StateSnapshot {
        for_node_id: "node-new".to_string(),
        bootstrap_session_id: "bootstrap-session-1".to_string(),
        bootstrap_nonce: "bootstrap-nonce-1".to_string(),
        configs: vec![],
        routes: vec![],
        encrypted_secrets: vec![],
        gateway_configs: vec![],
        api_keys: vec![],
        artifact_fetches: vec![],
        artifact_hashes: vec![],
    };

    match (join_event, snapshot) {
        (
            Event::NodeJoined {
                bootstrap_session_id: join_session,
                bootstrap_nonce: join_nonce,
                ..
            },
            Event::StateSnapshot {
                bootstrap_session_id: snapshot_session,
                bootstrap_nonce: snapshot_nonce,
                ..
            },
        ) => {
            assert_eq!(join_session, snapshot_session);
            assert_eq!(join_nonce, snapshot_nonce);
        }
        _ => unreachable!("expected bootstrap join + snapshot events"),
    }

    println!("✓ Bootstrap session correlation data verified");
}

#[tokio::test]
async fn test_secret_encryption_decryption() {
    // Generate receiver's keypair
    let receiver = BootstrapKeyPair::generate();
    let receiver_pubkey = receiver.public_bytes();

    // Encrypt secrets
    let secrets = vec![
        ("DATABASE_URL", "postgres://localhost/db"),
        ("API_KEY", "sk_test_1234567890"),
        ("SECRET_TOKEN", "my_super_secret_token"),
    ];

    let mut encrypted_secrets = Vec::new();
    for (key, value) in &secrets {
        let encrypted = secrets::encrypt_for_peer(&receiver_pubkey, value.as_bytes()).unwrap();
        assert_ne!(&encrypted[..], value.as_bytes());
        encrypted_secrets.push((key.to_string(), encrypted));
    }

    // Verify each encrypted blob is unique (different ephemeral keys)
    for i in 0..encrypted_secrets.len() {
        for j in (i + 1)..encrypted_secrets.len() {
            assert_ne!(
                &encrypted_secrets[i].1[..32],
                &encrypted_secrets[j].1[..32],
                "Ephemeral public keys should be different"
            );
        }
    }

    // Decrypt secrets
    for ((key, encrypted), (_orig_key, orig_value)) in encrypted_secrets.iter().zip(&secrets) {
        let decrypted = receiver.decrypt(encrypted).unwrap();

        let decrypted_str = String::from_utf8(decrypted).unwrap();
        assert_eq!(&decrypted_str, orig_value, "Secret mismatch for {}", key);
    }

    println!("✓ Secret encryption/decryption verified");
    println!(
        "✓ Verified {} secrets are encrypted on-wire (not plaintext)",
        secrets.len()
    );
}

#[test]
fn test_snapshot_serializes_signed_artifact_fetch_authorizations() {
    let fetch = BootstrapArtifactFetchAuthorization {
        app_id: "app1:v1".to_string(),
        sha256: "abc123".to_string(),
        artifact_url: "http://node-0.internal:9091/artifacts/abc123".to_string(),
        artifact_transfer_manifest: Some(
            ArtifactTransferAuthority::derive("node-0", &[4u8; 32])
                .issue_read_manifest_for_audience("abc123", "node-1"),
        ),
    };

    let snapshot = Event::StateSnapshot {
        for_node_id: "node-1".to_string(),
        bootstrap_session_id: "bootstrap-session-1".to_string(),
        bootstrap_nonce: "bootstrap-nonce-1".to_string(),
        configs: vec![],
        routes: vec![],
        encrypted_secrets: vec![],
        gateway_configs: vec![],
        api_keys: vec![],
        artifact_fetches: vec![fetch.clone()],
        artifact_hashes: vec![("app1:v1".to_string(), "abc123".to_string())],
    };

    let encoded = serde_json::to_string(&snapshot).unwrap();
    let decoded: Event = serde_json::from_str(&encoded).unwrap();

    match decoded {
        Event::StateSnapshot {
            artifact_fetches,
            artifact_hashes,
            ..
        } => {
            assert_eq!(artifact_fetches, vec![fetch]);
            assert_eq!(
                artifact_hashes,
                vec![("app1:v1".to_string(), "abc123".to_string())]
            );
        }
        other => panic!("unexpected event after roundtrip: {:?}", other),
    }
}

#[tokio::test]
async fn test_snapshot_event_structure() {
    let nats = start_test_nats().await.unwrap();
    let nats_url = nats.url.clone();
    sleep(Duration::from_millis(100)).await;

    let bus = messaging::NatsBus::connect(&nats_url).await.unwrap();
    bus.setup_jetstream().await.unwrap();

    // Create sample snapshot
    let mut config1 = create_test_app_config("app1:v1");
    config1
        .env_vars
        .insert("PORT".to_string(), "8080".to_string());
    config1.secret_keys.push("DB_URL".to_string());

    let config2 = create_test_app_config("app2:v1");

    let configs = vec![config1, config2];

    let routes = vec![common::types::Route {
        host: "app1.example.com".to_string(),
        app_id: AppId("app1:v1".to_string()),
        path_prefix: "/".to_string(),
        strip_prefix: false,
        created_at: 0,
        updated_at: 0,
    }];

    // Encrypt secrets for new node
    let receiver = BootstrapKeyPair::generate();
    let encrypted_secrets = vec![SecretTransportEntry {
        app_id: "app1:v1".to_string(),
        key: "DB_URL".to_string(),
        envelope: SecretTransportEnvelope::bootstrap_peer_ciphertext(
            secrets::encrypt_for_peer(&receiver.public_bytes(), b"postgres://db").unwrap(),
        ),
    }];

    let artifact_hashes = vec![
        ("app1:v1".to_string(), "abc123".to_string()),
        ("app2:v1".to_string(), "def456".to_string()),
    ];

    let snapshot = Event::StateSnapshot {
        for_node_id: "node-1".to_string(),
        bootstrap_session_id: "bootstrap-session-1".to_string(),
        bootstrap_nonce: "bootstrap-nonce-1".to_string(),
        configs: configs.clone(),
        routes: routes.clone(),
        encrypted_secrets: encrypted_secrets.clone(),
        gateway_configs: vec![],
        api_keys: vec![],
        artifact_fetches: vec![],
        artifact_hashes: artifact_hashes.clone(),
    };

    // Publish snapshot
    bus.publish(&snapshot).await.unwrap();
    sleep(Duration::from_millis(200)).await;

    // Verify structure
    if let Event::StateSnapshot {
        for_node_id,
        bootstrap_session_id,
        bootstrap_nonce,
        configs: c,
        routes: r,
        encrypted_secrets: s,
        gateway_configs,
        api_keys,
        artifact_fetches,
        artifact_hashes: h,
    } = snapshot
    {
        assert_eq!(for_node_id, "node-1");
        assert_eq!(bootstrap_session_id, "bootstrap-session-1");
        assert_eq!(bootstrap_nonce, "bootstrap-nonce-1");
        assert_eq!(c.len(), 2);
        assert_eq!(r.len(), 1);
        assert_eq!(s.len(), 1);
        assert!(gateway_configs.is_empty());
        assert!(api_keys.is_empty());
        assert!(artifact_fetches.is_empty());
        assert_eq!(h.len(), 2);

        // Verify secrets are encrypted (not plaintext)
        for entry in &s {
            match &entry.envelope.payload {
                SecretTransportPayload::BootstrapPeerCiphertextV1 { ciphertext } => {
                    assert!(ciphertext.len() > 32 + 12); // pubkey + nonce + ciphertext
                    assert_ne!(ciphertext, b"postgres://db");
                }
                other => panic!("unexpected secret payload variant in snapshot: {:?}", other),
            }
        }

        println!("✓ StateSnapshot structure verified");
        println!(
            "✓ Contains {} apps, {} routes, {} secrets, {} artifacts",
            c.len(),
            r.len(),
            s.len(),
            h.len()
        );
    }
}

#[tokio::test]
async fn test_two_node_bootstrap_simulation() {
    let nats = start_test_nats().await.unwrap();
    let nats_url = nats.url.clone();
    sleep(Duration::from_millis(100)).await;

    let bus = messaging::NatsBus::connect(&nats_url).await.unwrap();
    bus.setup_jetstream().await.unwrap();

    // ═══ NODE-0: Existing node with data ═══
    let temp_dir0 = tempfile::tempdir().unwrap();
    let db_path0 = temp_dir0.path().join("node0.db");
    let store0 = Store::open(&db_path0).unwrap();

    // Deploy an app on node-0
    let mut app_config = create_test_app_config("hello:v1");
    app_config
        .env_vars
        .insert("PORT".to_string(), "8080".to_string());
    app_config.secret_keys.push("API_KEY".to_string());
    store0.save_config(&app_config).unwrap();

    // Save artifact hash
    let wasm_bytes = b"fake wasm binary";
    let sha256 = hex::encode(Sha256::digest(wasm_bytes));
    store0.save_raw_wasm(&sha256, wasm_bytes).unwrap();
    store0.save_artifact_hash(&app_config.id, &sha256).unwrap();

    // Add a route
    let route = common::types::Route {
        host: "hello.example.com".to_string(),
        app_id: app_config.id.clone(),
        path_prefix: "/".to_string(),
        strip_prefix: false,
        created_at: 0,
        updated_at: 0,
    };
    store0.save_route(&route).unwrap();

    let gateway_config = common::types::GatewayRouteConfig {
        auth: common::types::AuthPolicy::Authenticated,
        ..Default::default()
    };
    store0
        .save_gateway_config(&app_config.id.0, &gateway_config)
        .unwrap();
    let api_keys = vec![common::types::ApiKeyRecord {
        name: "bootstrap-key".to_string(),
        key_hash: "sha256$bootstrap-key-hash".to_string(),
        scopes: vec!["/".to_string()],
    }];
    store0.save_api_keys(&app_config.id.0, &api_keys).unwrap();

    // Initialize secret provider for node-0
    let kek0 = secrets::crypto::SymmetricKey::generate();
    let secret_provider0 = secrets::LocalSecretProvider::new(store0.clone(), kek0);
    secret_provider0
        .set(&app_config.id, "API_KEY", "sk_test_node0_secret")
        .await
        .unwrap();

    println!("✓ Node-0 setup complete: 1 app, 1 route, 1 secret");

    // ═══ NODE-1: Fresh node joining ═══
    let temp_dir1 = tempfile::tempdir().unwrap();
    let db_path1 = temp_dir1.path().join("node1.db");
    let store1 = Store::open(&db_path1).unwrap();

    // Verify node-1 is fresh
    assert!(store1.list_apps().unwrap().is_empty());

    // Node-1 generates bootstrap keypair
    let keypair1 = BootstrapKeyPair::generate();
    let pubkey1 = keypair1.public_bytes();

    // Node-1 publishes NodeJoined
    let advertised_artifact_url = "http://node-1.internal:8081".to_string();
    let bootstrap_session_id = "bootstrap-session-node-1".to_string();
    let bootstrap_nonce = "bootstrap-nonce-node-1".to_string();
    let join_event = Event::NodeJoined {
        node_id: "node-1".to_string(),
        bootstrap_session_id: bootstrap_session_id.clone(),
        bootstrap_nonce: bootstrap_nonce.clone(),
        artifact_server_url: advertised_artifact_url.clone(),
        public_key_bytes: pubkey1.clone(),
        protocol_version: common::protocol::PROTOCOL_VERSION,
        binary_version: common::protocol::BINARY_VERSION.to_string(),
    };
    assert!(
        !is_loopback_url(&advertised_artifact_url),
        "two-node bootstrap simulation must use a routable artifact URL"
    );
    bus.publish(&join_event).await.unwrap();
    println!("✓ Node-1 published NodeJoined");

    sleep(Duration::from_millis(100)).await;

    // ═══ NODE-0: Respond with StateSnapshot ═══
    // The joining node correlates the first valid session/nonce-matching snapshot
    // and ignores duplicates afterwards.

    // Collect state from node-0
    let configs = vec![app_config.clone()];
    let routes = vec![route.clone()];

    // Encrypt secrets
    let secret_value = secret_provider0
        .get(&app_config.id, "API_KEY")
        .await
        .unwrap();
    let encrypted_secret = secrets::encrypt_for_peer(&pubkey1, secret_value.as_bytes()).unwrap();
    let encrypted_secrets = vec![SecretTransportEntry {
        app_id: app_config.id.0.clone(),
        key: "API_KEY".to_string(),
        envelope: SecretTransportEnvelope::bootstrap_peer_ciphertext(encrypted_secret),
    }];

    let artifact_hashes = vec![(app_config.id.0.clone(), sha256.clone())];
    let gateway_configs = vec![(app_config.id.0.clone(), gateway_config.clone())];
    let api_key_snapshot = vec![(app_config.id.0.clone(), api_keys.clone())];

    let snapshot = Event::StateSnapshot {
        for_node_id: "node-1".to_string(),
        bootstrap_session_id: bootstrap_session_id.clone(),
        bootstrap_nonce: bootstrap_nonce.clone(),
        configs,
        routes,
        encrypted_secrets: encrypted_secrets.clone(),
        gateway_configs: gateway_configs.clone(),
        api_keys: api_key_snapshot.clone(),
        artifact_fetches: vec![],
        artifact_hashes: artifact_hashes.clone(),
    };

    bus.publish(&snapshot).await.unwrap();
    println!("✓ Node-0 sent StateSnapshot to node-1");

    sleep(Duration::from_millis(100)).await;

    // ═══ NODE-1: Process StateSnapshot ═══
    if let Event::StateSnapshot {
        configs,
        routes,
        encrypted_secrets,
        gateway_configs,
        api_keys,
        artifact_fetches,
        artifact_hashes,
        ..
    } = snapshot.clone()
    {
        // Save configs
        for config in &configs {
            store1.save_config(config).unwrap();
        }

        // Save routes
        for route in &routes {
            store1.save_route(route).unwrap();
        }

        // Restore gateway policy state
        for (app_id, config) in gateway_configs {
            store1.save_gateway_config(&app_id, &config).unwrap();
        }
        for (app_id, keys) in api_keys {
            store1.save_api_keys(&app_id, &keys).unwrap();
        }

        // Decrypt and save secrets
        let kek1_bytes = *secrets::crypto::SymmetricKey::generate().as_bytes();
        let secret_provider1 = secrets::LocalSecretProvider::new(
            store1.clone(),
            secrets::crypto::SymmetricKey::from_bytes(kek1_bytes),
        );

        for SecretTransportEntry {
            app_id: app_id_str,
            key,
            envelope,
        } in encrypted_secrets
        {
            let plaintext_bytes = match envelope.payload {
                SecretTransportPayload::BootstrapPeerCiphertextV1 { ciphertext } => {
                    keypair1.decrypt(&ciphertext).unwrap()
                }
                other => panic!("unexpected bootstrap secret payload variant: {:?}", other),
            };

            let plaintext = String::from_utf8(plaintext_bytes).unwrap();
            assert_eq!(plaintext, "sk_test_node0_secret");

            let app_id = AppId(app_id_str);
            secret_provider1
                .set(&app_id, &key, &plaintext)
                .await
                .unwrap();
        }

        let verify_provider = secrets::LocalSecretProvider::new(
            store1.clone(),
            secrets::crypto::SymmetricKey::from_bytes(kek1_bytes),
        );
        let secret = verify_provider
            .get(&app_config.id, "API_KEY")
            .await
            .unwrap();
        assert_eq!(secret, "sk_test_node0_secret");

        assert!(artifact_fetches.is_empty());

        // Save artifact hashes
        for (app_id_str, hash) in artifact_hashes {
            let app_id = AppId(app_id_str);
            store1.save_artifact_hash(&app_id, &hash).unwrap();
        }

        // Save raw wasm (would normally be pushed via HTTP)
        store1.save_raw_wasm(&sha256, wasm_bytes).unwrap();

        println!("✓ Node-1 processed StateSnapshot");
    }

    // ═══ VERIFY NODE-1 STATE ═══
    // Check configs
    let apps = store1.list_apps().unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].0, "hello:v1");

    let loaded_config = store1.load_config(&app_config.id).unwrap().unwrap();
    assert_eq!(loaded_config.id.0, "hello:v1");
    assert_eq!(loaded_config.memory_limit.0, 16); // 16 pages = 1MB

    // Check routes
    let loaded_routes = store1.list_routes().unwrap();
    assert_eq!(loaded_routes.len(), 1);
    assert_eq!(loaded_routes[0].host, "hello.example.com");

    // Check gateway policy state
    let loaded_gateway = store1
        .load_gateway_config(&app_config.id.0)
        .unwrap()
        .unwrap();
    assert_eq!(loaded_gateway, gateway_config);
    let loaded_api_keys = store1.load_api_keys(&app_config.id.0).unwrap();
    assert_eq!(loaded_api_keys, api_keys);

    // Check artifact hash
    let loaded_hash = store1.get_artifact_sha256(&app_config.id).unwrap().unwrap();
    assert_eq!(loaded_hash, sha256);

    // Check raw wasm
    let loaded_wasm = store1.load_raw_wasm(&sha256).unwrap().unwrap();
    assert_eq!(loaded_wasm, wasm_bytes);

    println!("✓ Node-1 fully synchronized:");
    println!("  - {} app configs", apps.len());
    println!("  - {} routes", loaded_routes.len());
    println!("  - 1 secret (encrypted)");
    println!("  - {} artifact hashes", 1);
    println!("  - Raw wasm available for compilation");

    println!("\n🎉 Two-node bootstrap test PASSED");
}

#[tokio::test]
async fn test_on_wire_encryption_verification() {
    // This test verifies that secrets are NEVER transmitted in plaintext
    let receiver = BootstrapKeyPair::generate();
    let receiver_pubkey = receiver.public_bytes();

    let plaintext_secret = "my_super_secret_password_12345";
    let encrypted =
        secrets::encrypt_for_peer(&receiver_pubkey, plaintext_secret.as_bytes()).unwrap();

    // Verify the encrypted blob does NOT contain the plaintext anywhere
    let encrypted_str = String::from_utf8_lossy(&encrypted);
    assert!(
        !encrypted_str.contains("my_super_secret"),
        "Plaintext secret found in encrypted blob!"
    );
    assert!(
        !encrypted_str.contains("password"),
        "Plaintext secret found in encrypted blob!"
    );

    // Verify structure: [ephemeral_pubkey(32) | nonce(12) | ciphertext]
    assert!(encrypted.len() >= 32 + 12, "Encrypted blob too short");

    // Verify we can decrypt it back
    let decrypted = receiver.decrypt(&encrypted).unwrap();
    let decrypted_str = String::from_utf8(decrypted).unwrap();
    assert_eq!(decrypted_str, plaintext_secret);

    println!("✓ On-wire encryption verified:");
    println!("  - Plaintext NOT found in encrypted blob");
    println!(
        "  - Encrypted size: {} bytes (plaintext: {} bytes)",
        encrypted.len(),
        plaintext_secret.len()
    );
    println!("  - Structure: 32-byte pubkey + 12-byte nonce + ciphertext");
    println!("  - Decryption successful");
}
