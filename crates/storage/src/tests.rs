use crate::Store;
use common::types::{AppConfig, AppId, ClusterNodeRecord, FuelQuota, MemoryPages};
use redb::ReadableDatabase;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

fn make_store() -> (Store, NamedTempFile) {
    let f = NamedTempFile::new().unwrap();
    let store = Store::open(f.path()).unwrap();
    (store, f)
}

#[test]
fn test_store_open_fresh_and_idempotent() {
    let f = NamedTempFile::new().unwrap();
    let store = Store::open(f.path()).expect("First open creates tables");
    drop(store);

    // Second open is idempotent and shouldn't panic
    let _store2 = Store::open(f.path()).expect("Second open should succeed");
}

#[test]
fn test_artifact_roundtrip() {
    let (store, _f) = make_store();
    let id = AppId::new("test-app", "v1");
    let bytes = b"fake wasm artifact bytes";

    assert!(!store.artifact_exists(&id).unwrap());
    store.store_artifact(&id, bytes).unwrap();
    assert!(store.artifact_exists(&id).unwrap());

    let loaded = store.load_artifact(&id).unwrap().unwrap();
    assert_eq!(loaded, bytes);

    store.delete_artifact(&id).unwrap();
    assert!(!store.artifact_exists(&id).unwrap());
    assert!(store.load_artifact(&id).unwrap().is_none());
}

#[test]
fn test_artifact_survives_drop() {
    let f = NamedTempFile::new().unwrap();
    let store1 = Store::open(f.path()).unwrap();
    let id = AppId::new("persist", "v1");
    store1.store_artifact(&id, b"persist-data").unwrap();
    drop(store1);

    let store2 = Store::open(f.path()).unwrap();
    let loaded = store2.load_artifact(&id).unwrap().unwrap();
    assert_eq!(loaded, b"persist-data");
}

#[test]
fn test_config_roundtrip() {
    let (store, _f) = make_store();
    let mut config = AppConfig::default_for(AppId::new("config-app", "v1"));
    config.fuel_quota = FuelQuota(100);
    config.memory_limit = MemoryPages(10);
    config.env_vars = std::collections::HashMap::from([("KEY".to_string(), "VAL".to_string())]);
    config.wasm_bind_port = 8080;

    store.save_config(&config).unwrap();
    let loaded = store.load_config(&config.id).unwrap().unwrap();

    assert_eq!(config.id, loaded.id);
    assert_eq!(config.fuel_quota.0, loaded.fuel_quota.0);
    assert_eq!(config.memory_limit.0, loaded.memory_limit.0);
    assert_eq!(config.env_vars, loaded.env_vars);
    assert_eq!(config.wasm_bind_port, loaded.wasm_bind_port);

    // List apps
    let apps = store.list_apps().unwrap();
    assert!(apps.contains(&config.id));

    // Upsert
    let mut config2 = config.clone();
    config2.wasm_bind_port = 9090;
    store.save_config(&config2).unwrap();
    let loaded2 = store.load_config(&config.id).unwrap().unwrap();
    assert_eq!(loaded2.wasm_bind_port, 9090);
}

#[test]
fn test_metrics_store() {
    let (store, _f) = make_store();
    use crate::metrics::MetricBucket;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let min_ts = ((now - 120) / 60) * 60;

    let bucket = MetricBucket {
        app_id: "met-app".to_string(),
        minute_ts: min_ts,
        request_count: 5,
        fuel_consumed_total: 100,
        fuel_consumed_avg: 20,
        ram_usage_peak_bytes: 1024,
        latency_p50_ms: 1.0,
        latency_p99_ms: 5.0,
        trap_count: 0,
    };

    store.write_metric_bucket(&bucket).unwrap();

    let recent = store.load_recent_metrics("met-app", 5).unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].request_count, 5);

    let removed = store.prune_old_metrics(0).unwrap();
    assert_eq!(removed, 1);

    let recent2 = store.load_recent_metrics("met-app", 5).unwrap();
    assert!(recent2.is_empty());
}

#[test]
fn test_prune_old_metrics_performance() {
    let (store, _f) = make_store();
    use crate::metrics::MetricBucket;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Insert 1000 old buckets
    let start = std::time::Instant::now();
    for i in 0..1000 {
        let bucket = MetricBucket {
            app_id: format!("perf-app-{i}"),
            minute_ts: now - (86400 * 2) - i, // Old data
            request_count: 1,
            fuel_consumed_total: 10,
            fuel_consumed_avg: 10,
            ram_usage_peak_bytes: 100,
            latency_p50_ms: 1.0,
            latency_p99_ms: 1.0,
            trap_count: 0,
        };
        store.write_metric_bucket(&bucket).unwrap();
    }

    let removed = store.prune_old_metrics(1440).unwrap();
    assert_eq!(removed, 1000);

    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 5000, "Should be reasonably fast");
}

#[test]
fn test_concurrency() {
    use std::thread;
    let (store, _f) = make_store();
    let id = AppId::new("concurrent", "v1");
    store.store_artifact(&id, b"init").unwrap();

    let s1 = store.clone();
    let s2 = store.clone();
    let id1 = id.clone();
    let id2 = id.clone();

    let t1 = thread::spawn(move || s1.load_artifact(&id1).unwrap());

    let t2 = thread::spawn(move || s2.load_artifact(&id2).unwrap());

    assert!(t1.join().unwrap().is_some());
    assert!(t2.join().unwrap().is_some());

    // Read and Write concurrency (MVCC)
    let s3 = store.clone();
    let s4 = store.clone();
    let tx_read = s3.db.begin_read().unwrap();

    let t3 = thread::spawn(move || {
        s4.store_artifact(&AppId::new("new", "v1"), b"data")
            .unwrap();
    });
    t3.join().unwrap();

    // Read tx started before write should still be valid (MVCC)
    let table = tx_read.open_table(crate::tables::ARTIFACTS).unwrap();
    let existing = table.get("concurrent:v1").unwrap().unwrap();
    assert_eq!(existing.value(), b"init");
}

#[test]
fn test_prune_old_versions() {
    let (store, _f) = make_store();

    store
        .store_artifact(&AppId::new("app-prune", "v1"), b"v1")
        .unwrap();
    store
        .store_artifact(&AppId::new("app-prune", "v2"), b"v2")
        .unwrap();
    store
        .store_artifact(&AppId::new("app-prune", "v3"), b"v3")
        .unwrap();
    store
        .store_artifact(&AppId::new("app-prune", "v4"), b"v4")
        .unwrap();
    store
        .store_artifact(&AppId::new("app-prune", "v5"), b"v5")
        .unwrap();

    // Keep the latest 3, but mark v1 as active so it won't be deleted
    store
        .prune_old_versions("app-prune", 3, &["app-prune:v1"])
        .unwrap();

    assert!(store
        .artifact_exists(&AppId::new("app-prune", "v1"))
        .unwrap());
    assert!(!store
        .artifact_exists(&AppId::new("app-prune", "v2"))
        .unwrap());
    assert!(store
        .artifact_exists(&AppId::new("app-prune", "v3"))
        .unwrap());
    assert!(store
        .artifact_exists(&AppId::new("app-prune", "v4"))
        .unwrap());
    assert!(store
        .artifact_exists(&AppId::new("app-prune", "v5"))
        .unwrap());
}

#[test]
fn test_cluster_node_registry_roundtrip() {
    let (store, _f) = make_store();
    let mut node = ClusterNodeRecord::new("node-1", 1_700_000_000);
    node.joined_at_unix_secs = Some(1_700_000_000);
    node.proxy_address = Some("node-1.internal:8080".to_string());
    node.artifact_server_url = Some("http://node-1.internal:9091".to_string());
    node.protocol_version = Some(7);
    node.binary_version = Some("0.5.0".to_string());
    node.active_instances = Some(3);
    node.deployed_apps = Some(2);

    store.save_cluster_node(&node).unwrap();

    let loaded = store.load_cluster_node("node-1").unwrap().unwrap();
    assert_eq!(loaded, node);

    let listed = store.list_cluster_nodes().unwrap();
    assert_eq!(listed, vec![node]);
}

// ── SCHEMA MIGRATION TESTS ────────────────────────────────────────────────────

#[test]
fn test_fresh_database_has_current_schema_version() {
    let f = NamedTempFile::new().unwrap();
    let store = Store::open(f.path()).unwrap();

    // Fresh database should be migrated to current version
    let version = store.read_schema_version().unwrap();
    assert_eq!(version, crate::CURRENT_SCHEMA_VERSION);
}

#[test]
fn test_schema_version_persistence() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    {
        let store = Store::open(&path).unwrap();
        let version = store.read_schema_version().unwrap();
        assert_eq!(version, crate::CURRENT_SCHEMA_VERSION);
    }

    // Reopen and verify version persisted
    let store2 = Store::open(&path).unwrap();
    let version2 = store2.read_schema_version().unwrap();
    assert_eq!(version2, crate::CURRENT_SCHEMA_VERSION);
}

#[test]
fn test_migration_v1_to_v2_adds_db_max_connections() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Create a v1 database manually
    {
        let store = Store::open(&path).unwrap();

        // Manually set version to 1
        store.write_schema_version(1).unwrap();

        // Create an old-format config without db_max_connections
        let id = AppId::new("test-app", "v1");
        let old_config_json = serde_json::json!({
            "id": "test-app:v1",
            "fuel_quota": 500_000_000,
            "memory_limit": 2048,
            "max_instances": 10,
            "idle_timeout_secs": 300,
            "wasm_bind_port": 8080,
            "env_vars": {},
            "secret_keys": [],
            "extended_limits": null,
            "health_check_path": null
            // NOTE: db_max_connections is intentionally missing
        });

        let tx = store.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::tables::CONFIGS).unwrap();
            table
                .insert(
                    id.0.as_str(),
                    serde_json::to_string(&old_config_json).unwrap().as_str(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
    }

    // Reopen, which should trigger migration (v1 -> v2 -> v3)
    let store = Store::open(&path).unwrap();

    // Verify version upgraded to current (v3)
    let version = store.read_schema_version().unwrap();
    assert_eq!(version, crate::CURRENT_SCHEMA_VERSION);

    // Verify the config now has db_max_connections (from v2 migration)
    let id = AppId::new("test-app", "v1");
    let config = store.load_config(&id).unwrap().unwrap();
    assert_eq!(config.db_max_connections, Some(10));

    // Verify the config also has rate_limit (from v3 migration)
    assert!(config.rate_limit.is_some());
}

#[test]
fn test_migration_is_idempotent() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Create a v1 database with a config that already has db_max_connections
    {
        let store = Store::open(&path).unwrap();
        store.write_schema_version(1).unwrap();

        let id = AppId::new("idempotent-app", "v1");
        let config_json = serde_json::json!({
            "id": "idempotent-app:v1",
            "fuel_quota": 500_000_000,
            "memory_limit": 2048,
            "max_instances": 10,
            "idle_timeout_secs": 300,
            "wasm_bind_port": 8080,
            "env_vars": {},
            "secret_keys": [],
            "extended_limits": null,
            "health_check_path": null,
            "db_max_connections": 25  // Already set to custom value
        });

        let tx = store.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::tables::CONFIGS).unwrap();
            table
                .insert(
                    id.0.as_str(),
                    serde_json::to_string(&config_json).unwrap().as_str(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
    }

    // Reopen and run migration
    let store = Store::open(&path).unwrap();

    // Verify the custom value is preserved (not overwritten)
    let id = AppId::new("idempotent-app", "v1");
    let config = store.load_config(&id).unwrap().unwrap();
    assert_eq!(config.db_max_connections, Some(25));
}

#[test]
fn test_backup_created_before_migration() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Create a v1 database
    {
        let store = Store::open(&path).unwrap();
        store.write_schema_version(1).unwrap();
    }

    // Verify no backup exists yet
    let backup_path = path.with_extension("redb.v1.bak");
    assert!(!backup_path.exists());

    // Reopen (triggers migration and backup)
    let _store = Store::open(&path).unwrap();

    // Verify backup was created
    assert!(backup_path.exists());

    // Cleanup
    std::fs::remove_file(backup_path).ok();
}

#[test]
fn test_backup_not_overwritten() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Create a v1 database
    {
        let store = Store::open(&path).unwrap();
        store.write_schema_version(1).unwrap();
    }

    // Manually create a backup file
    let backup_path = path.with_extension("redb.v1.bak");
    std::fs::write(&backup_path, b"original backup").unwrap();

    // Reopen (should NOT overwrite existing backup)
    let _store = Store::open(&path).unwrap();

    // Verify backup was not overwritten
    let backup_content = std::fs::read(&backup_path).unwrap();
    assert_eq!(backup_content, b"original backup");

    // Cleanup
    std::fs::remove_file(backup_path).ok();
}

// ── HOT CONFIG PERSISTENCE TESTS ──────────────────────────────────────────────

#[test]
fn test_hot_config_save_load_roundtrip() {
    let (store, _f) = make_store();

    // Save a hot config override
    let hot_json = serde_json::json!({
        "rate_limit": {
            "default_requests_per_second": 5000,
            "default_burst_capacity": 1000,
            "default_per_ip_limit": 500
        },
        "ebpf": {
            "enabled": true,
            "fd_soft_limit": 4096,
            "fd_hard_limit": 8192,
            "mem_low_threshold_pages": 65536,
            "mem_critical_threshold_pages": 16384,
            "disk_slow_threshold_ns": 50000000,
            "tcp_conn_limit_per_pid": 10000,
            "syscall_rate_limit": 100000,
            "sampling_period_secs": 10
        },
        "gc": {
            "artifact_keep_versions": 5,
            "metrics_retain_days": 14,
            "undeploy_grace_secs": 7200,
            "gc_interval_secs": 300,
            "disk_warning_threshold": 0.85
        },
        "health": {
            "check_interval_secs": 10,
            "default_idle_timeout_secs": 600,
            "default_max_instances": 20,
            "default_fuel_quota": 1000000000,
            "default_memory_pages": 4096
        },
        "logging": {
            "level": "debug",
            "otlp_endpoint": null
        }
    });
    let hot_str = serde_json::to_string(&hot_json).unwrap();

    store.save_meta("hot_config_overrides", &hot_str).unwrap();

    // Load it back
    let loaded = store.load_meta("hot_config_overrides").unwrap();
    assert!(loaded.is_some());
    let loaded_json: serde_json::Value = serde_json::from_str(&loaded.unwrap()).unwrap();

    // Verify key fields survived the round-trip
    assert_eq!(
        loaded_json["rate_limit"]["default_requests_per_second"],
        5000
    );
    assert_eq!(loaded_json["ebpf"]["fd_soft_limit"], 4096);
    assert_eq!(loaded_json["gc"]["gc_interval_secs"], 300);
    assert_eq!(loaded_json["health"]["check_interval_secs"], 10);
    assert_eq!(loaded_json["logging"]["level"], "debug");
}

#[test]
fn test_hot_config_clear() {
    let (store, _f) = make_store();

    // Save a hot config override
    store
        .save_meta(
            "hot_config_overrides",
            "{\"logging\":{\"level\":\"trace\"}}",
        )
        .unwrap();

    // Verify it exists
    let loaded = store.load_meta("hot_config_overrides").unwrap();
    assert!(loaded.is_some());

    // Clear it
    store.delete_meta("hot_config_overrides").unwrap();

    // Verify it's gone
    let loaded2 = store.load_meta("hot_config_overrides").unwrap();
    assert!(loaded2.is_none());
}

#[test]
fn test_hot_config_survives_restart() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Write hot config in first session
    {
        let store = Store::open(&path).unwrap();
        store
            .save_meta(
                "hot_config_overrides",
                "{\"logging\":{\"level\":\"warn\"},\"gc\":{\"gc_interval_secs\":120}}",
            )
            .unwrap();
    }

    // Reopen (simulates restart) and verify persistence
    {
        let store2 = Store::open(&path).unwrap();
        let loaded = store2.load_meta("hot_config_overrides").unwrap();
        assert!(loaded.is_some());
        let json: serde_json::Value = serde_json::from_str(&loaded.unwrap()).unwrap();
        assert_eq!(json["logging"]["level"], "warn");
        assert_eq!(json["gc"]["gc_interval_secs"], 120);
    }
}

#[test]
fn test_hot_config_corrupted_falls_back() {
    let (store, _f) = make_store();

    // Write invalid JSON
    store
        .save_meta("hot_config_overrides", "not valid json{{{")
        .unwrap();

    // Loading should return the raw string — the caller (HotConfigHandle)
    // is responsible for handling deserialization errors gracefully.
    let loaded = store.load_meta("hot_config_overrides").unwrap();
    assert!(loaded.is_some());
    // The value is the raw corrupted string; deserialization will fail
    assert!(serde_json::from_str::<serde_json::Value>(&loaded.unwrap()).is_err());
}

#[test]
#[should_panic(expected = "Database schema version 99 is NEWER than the binary supports")]
fn test_downgrade_not_supported() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Create a database with a future version
    {
        let store = Store::open(&path).unwrap();
        store.write_schema_version(99).unwrap();
    }

    // Reopen should panic
    let _store = Store::open(&path).unwrap();
}

#[test]
fn test_no_migration_when_already_current() {
    let f = NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Create a database at current version
    {
        let _store = Store::open(&path).unwrap();
        // Already at current version
    }

    // Verify no backup is created on second open
    let backup_path = path.with_extension(format!("redb.v{}.bak", crate::CURRENT_SCHEMA_VERSION));

    let _store2 = Store::open(&path).unwrap();

    // No backup should be created since no migration was needed
    assert!(!backup_path.exists());
}

// ── Auth Config Persistence Tests ─────────────────────────────────────────────

#[test]
fn test_auth_config_save_load_roundtrip() {
    let (store, _f) = make_store();

    // No auth config saved yet
    assert!(store.load_auth_config().unwrap().is_none());

    // Save an auth config
    let config = common::auth::AuthConfig {
        enabled: true,
        read_token: Some("read_token_abcdef1234567890".to_string()),
        write_token: Some("write_token_fedcba0987654321".to_string()),
        require_tls: true,
        rate_limit_per_second: 10,
        rate_limit_burst: 20,
        trusted_proxies: Vec::new(),
    };
    store.save_auth_config(&config).unwrap();

    // Load it back
    let loaded = store.load_auth_config().unwrap().unwrap();
    assert!(loaded.enabled);
    assert_eq!(
        loaded.read_token,
        Some("read_token_abcdef1234567890".to_string())
    );
    assert_eq!(
        loaded.write_token,
        Some("write_token_fedcba0987654321".to_string())
    );
    assert!(loaded.require_tls);
    assert_eq!(loaded.rate_limit_per_second, 10);
    assert_eq!(loaded.rate_limit_burst, 20);
}

#[test]
fn test_auth_config_survives_restart() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let path = f.path().to_path_buf();

    // Save auth config in first session
    {
        let store = Store::open(&path).unwrap();
        let config = common::auth::AuthConfig {
            enabled: true,
            write_token: Some("my_secret_write_token_1234".to_string()),
            read_token: None,
            require_tls: false,
            rate_limit_per_second: 5,
            rate_limit_burst: 10,
            trusted_proxies: Vec::new(),
        };
        store.save_auth_config(&config).unwrap();
    }

    // Reopen and verify the config persisted
    {
        let store = Store::open(&path).unwrap();
        let loaded = store.load_auth_config().unwrap().unwrap();
        assert!(loaded.enabled);
        assert_eq!(
            loaded.write_token,
            Some("my_secret_write_token_1234".to_string())
        );
        assert!(loaded.read_token.is_none());
        assert!(!loaded.require_tls);
        assert_eq!(loaded.rate_limit_per_second, 5);
        assert_eq!(loaded.rate_limit_burst, 10);
    }
}

#[test]
fn test_auth_config_delete() {
    let (store, _f) = make_store();

    // Save an auth config
    let config = common::auth::AuthConfig {
        enabled: true,
        write_token: Some("token_to_be_deleted_1234".to_string()),
        ..Default::default()
    };
    store.save_auth_config(&config).unwrap();
    assert!(store.load_auth_config().unwrap().is_some());

    // Delete it
    store.delete_auth_config().unwrap();
    assert!(store.load_auth_config().unwrap().is_none());
}

#[test]
fn test_auth_config_overwrite() {
    let (store, _f) = make_store();

    // Save initial config
    let config1 = common::auth::AuthConfig {
        enabled: true,
        write_token: Some("first_write_token_123456".to_string()),
        read_token: Some("first_read_token_1234567".to_string()),
        ..Default::default()
    };
    store.save_auth_config(&config1).unwrap();

    // Overwrite with new config (simulates token rotation)
    let config2 = common::auth::AuthConfig {
        enabled: true,
        write_token: Some("rotated_write_token_abcdef".to_string()),
        read_token: Some("first_read_token_1234567".to_string()), // unchanged
        ..Default::default()
    };
    store.save_auth_config(&config2).unwrap();

    // Load and verify the new config
    let loaded = store.load_auth_config().unwrap().unwrap();
    assert_eq!(
        loaded.write_token,
        Some("rotated_write_token_abcdef".to_string())
    );
    assert_eq!(
        loaded.read_token,
        Some("first_read_token_1234567".to_string())
    );
}

#[test]
fn test_auth_config_disabled_default() {
    let (store, _f) = make_store();

    // Save a disabled (default) auth config
    let config = common::auth::AuthConfig::default();
    assert!(!config.enabled);
    store.save_auth_config(&config).unwrap();

    let loaded = store.load_auth_config().unwrap().unwrap();
    assert!(!loaded.enabled);
    assert!(loaded.read_token.is_none());
    assert!(loaded.write_token.is_none());
}
