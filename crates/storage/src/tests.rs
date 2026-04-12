use crate::Store;
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
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
