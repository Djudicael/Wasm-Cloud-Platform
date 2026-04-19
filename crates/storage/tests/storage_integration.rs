use common::types::*;
use storage::Store;
use tempfile::NamedTempFile;

/// Helper to create a fresh store for each test.
fn make_store() -> (Store, NamedTempFile) {
    let f = NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    (s, f)
}

#[test]
fn test_schema_migration_fresh_db() {
    let (store, _f) = make_store();
    // Should complete without error and be at version 3
    // The schema version is private, but we can verify the db works
    let id = AppId::new("test", "v1");
    let config = AppConfig::default_for(id.clone());
    store.save_config(&config).unwrap();
    let loaded = store.load_config(&id).unwrap().unwrap();
    assert_eq!(loaded.id, id);
}

#[test]
fn test_artifact_store_and_load() {
    let (store, _f) = make_store();
    let id = AppId::new("test-app", "v1");
    let bytes = b"fake compiled wasm artifact bytes";

    // Store artifact
    store.store_artifact(&id, bytes).unwrap();

    // Load it back
    let loaded = store.load_artifact(&id).unwrap().unwrap();
    assert_eq!(loaded, bytes);

    // Check existence
    assert!(store.artifact_exists(&id).unwrap());

    // Delete it
    store.delete_artifact(&id).unwrap();
    assert!(!store.artifact_exists(&id).unwrap());

    // Load should return None
    assert!(store.load_artifact(&id).unwrap().is_none());
}

#[test]
fn test_config_roundtrip_with_secrets() {
    let (store, _f) = make_store();

    let mut env_vars = std::collections::HashMap::new();
    env_vars.insert("LOG_LEVEL".to_string(), "debug".to_string());
    env_vars.insert("APP_ENV".to_string(), "test".to_string());

    let config = AppConfig {
        id: AppId::new("my-app", "v2"),
        fuel_quota: FuelQuota(1_000_000),
        memory_limit: MemoryPages(512),
        max_instances: 5,
        idle_timeout_secs: 60,
        wasm_bind_port: 8080,
        env_vars,
        secret_keys: vec!["DATABASE_URL".to_string(), "API_KEY".to_string()],
        extended_limits: None,
        health_check_path: Some("/health".to_string()),
        db_max_connections: Some(10),
        rate_limit: Some(AppRateLimitConfig {
            requests_per_second: 500,
            burst_capacity: 25,
            per_ip_limit: 50,
        }),
        tenant_id: None,
        policy: None,
    };

    store.save_config(&config).unwrap();
    let loaded = store.load_config(&config.id).unwrap().unwrap();

    assert_eq!(loaded.id, config.id);
    assert_eq!(loaded.fuel_quota.0, 1_000_000);
    assert_eq!(loaded.secret_keys, vec!["DATABASE_URL", "API_KEY"]);
    assert_eq!(loaded.health_check_path, Some("/health".to_string()));
    assert_eq!(loaded.db_max_connections, Some(10));
    assert_eq!(loaded.rate_limit.as_ref().unwrap().requests_per_second, 500);
    assert_eq!(loaded.env_vars.get("LOG_LEVEL").unwrap(), "debug");
}

#[test]
fn test_route_crud() {
    let (store, _f) = make_store();

    let route = Route {
        host: "api.example.com".to_string(),
        app_id: AppId::new("my-app", "v1"),
        path_prefix: "/api".to_string(),
        strip_prefix: true,
        created_at: 1000,
        updated_at: 1000,
    };

    // Save route
    store.save_route(&route).unwrap();

    // List all routes
    let routes = store.list_routes().unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].host, "api.example.com");

    // Load specific route
    let loaded = store.load_route("api.example.com").unwrap().unwrap();
    assert_eq!(loaded.app_id, AppId::new("my-app", "v1"));
    assert_eq!(loaded.path_prefix, "/api");
    assert!(loaded.strip_prefix);

    // Delete route
    store.delete_route("api.example.com").unwrap();
    assert!(store.list_routes().unwrap().is_empty());
    assert!(store.load_route("api.example.com").unwrap().is_none());
}

#[test]
fn test_multiple_routes() {
    let (store, _f) = make_store();

    let route1 = Route {
        host: "app1.example.com".to_string(),
        app_id: AppId::new("app1", "v1"),
        path_prefix: "/".to_string(),
        strip_prefix: false,
        created_at: 1000,
        updated_at: 1000,
    };

    let route2 = Route {
        host: "app2.example.com".to_string(),
        app_id: AppId::new("app2", "v2"),
        path_prefix: "/api".to_string(),
        strip_prefix: true,
        created_at: 2000,
        updated_at: 2000,
    };

    store.save_route(&route1).unwrap();
    store.save_route(&route2).unwrap();

    let routes = store.list_routes().unwrap();
    assert_eq!(routes.len(), 2);

    // Verify both routes exist
    assert!(store.load_route("app1.example.com").unwrap().is_some());
    assert!(store.load_route("app2.example.com").unwrap().is_some());
}

#[test]
fn test_metrics_write() {
    let (store, _f) = make_store();

    let bucket = storage::metrics::MetricBucket {
        app_id: "test-app".to_string(),
        minute_ts: 1000 * 60,
        request_count: 42,
        fuel_consumed_total: 1_000_000,
        fuel_consumed_avg: 23_809,
        ram_usage_peak_bytes: 64 * 1024 * 1024,
        latency_p50_ms: 5.0,
        latency_p99_ms: 50.0,
        trap_count: 0,
    };

    // Write should succeed
    store.write_metric_bucket(&bucket).unwrap();
}

#[test]
fn test_metrics_prune_old() {
    let (store, _f) = make_store();

    // Write an old metric
    let old_bucket = storage::metrics::MetricBucket {
        app_id: "old-app".to_string(),
        minute_ts: 1000 * 60, // Very old timestamp
        request_count: 10,
        fuel_consumed_total: 100_000,
        fuel_consumed_avg: 10_000,
        ram_usage_peak_bytes: 1024 * 1024,
        latency_p50_ms: 2.0,
        latency_p99_ms: 20.0,
        trap_count: 0,
    };

    store.write_metric_bucket(&old_bucket).unwrap();

    // Prune everything older than 1 minute (should prune the old metric)
    let pruned = store.prune_old_metrics(1).unwrap();

    assert!(pruned >= 1, "should have pruned at least 1 old metric");
}

#[test]
fn test_raw_wasm_store_and_load() {
    let (store, _f) = make_store();

    let raw_wasm = b"this is raw wasm bytes before compilation";
    let sha256 = "abc123hash";

    // Store raw wasm
    store.save_raw_wasm(sha256, raw_wasm).unwrap();

    let loaded = store.load_raw_wasm(sha256).unwrap().unwrap();
    assert_eq!(loaded, raw_wasm);

    assert!(store.raw_wasm_exists(sha256).unwrap());
}

#[test]
fn test_secret_roundtrip() {
    let (store, _f) = make_store();

    let app_id = AppId::new("secure-app", "v1");
    let encrypted_blob = b"fake encrypted secrets blob";

    // Store encrypted secrets
    store.save_secrets(&app_id, encrypted_blob).unwrap();

    // Load encrypted blob
    let loaded = store.load_secrets(&app_id).unwrap().unwrap();
    assert_eq!(loaded, encrypted_blob);

    // Delete secrets
    store.delete_secrets(&app_id).unwrap();
    assert!(store.load_secrets(&app_id).unwrap().is_none());
}

#[test]
fn test_artifact_version_inventory() {
    let (store, _f) = make_store();

    // Store multiple versions
    store
        .store_artifact(&AppId::new("myapp", "v1"), b"v1 bytes")
        .unwrap();
    store
        .store_artifact(&AppId::new("myapp", "v2"), b"v2 bytes")
        .unwrap();
    store
        .store_artifact(&AppId::new("myapp", "v10"), b"v10 bytes")
        .unwrap();
    store
        .store_artifact(&AppId::new("other", "v1"), b"other v1")
        .unwrap();

    // Verify all are stored
    assert!(store.artifact_exists(&AppId::new("myapp", "v1")).unwrap());
    assert!(store.artifact_exists(&AppId::new("myapp", "v2")).unwrap());
    assert!(store.artifact_exists(&AppId::new("myapp", "v10")).unwrap());
    assert!(store.artifact_exists(&AppId::new("other", "v1")).unwrap());
}

#[test]
fn test_config_list_all() {
    let (store, _f) = make_store();

    let config1 = AppConfig::default_for(AppId::new("app1", "v1"));
    let config2 = AppConfig::default_for(AppId::new("app2", "v1"));
    let config3 = AppConfig::default_for(AppId::new("app1", "v2"));

    store.save_config(&config1).unwrap();
    store.save_config(&config2).unwrap();
    store.save_config(&config3).unwrap();

    let all = store.list_apps().unwrap();
    assert_eq!(all.len(), 3);

    // Verify we can load each one
    assert!(store.load_config(&config1.id).unwrap().is_some());
    assert!(store.load_config(&config2.id).unwrap().is_some());
    assert!(store.load_config(&config3.id).unwrap().is_some());
}

#[test]
fn test_persistence_across_reopens() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // First open: write data
    {
        let store = Store::open(&path).unwrap();
        let config = AppConfig::default_for(AppId::new("persist-test", "v1"));
        store.save_config(&config).unwrap();

        let route = Route {
            host: "persist.example.com".to_string(),
            app_id: AppId::new("persist-test", "v1"),
            path_prefix: "/".to_string(),
            strip_prefix: false,
            created_at: 1000,
            updated_at: 1000,
        };
        store.save_route(&route).unwrap();
    }

    // Second open: verify data persisted
    {
        let store = Store::open(&path).unwrap();

        let config = store
            .load_config(&AppId::new("persist-test", "v1"))
            .unwrap();
        assert!(config.is_some());

        let route = store.load_route("persist.example.com").unwrap();
        assert!(route.is_some());
    }
}
