# Step 02 — Local Persistent Storage with redb

## Goal
Implement the local storage layer using `redb`, an embedded key-value database written in Rust.
This layer is the "memory" of each node — it holds compiled Wasm artifacts, app configs,
encrypted secrets, and aggregated metrics. There is no central database; each node is sovereign.

---

## Context & Rationale

### The Problem This Solves

The platform is **shared-nothing**: no central database that all nodes read from. This is a
deliberate choice — a central DB is a single point of failure, a network hop on every cold
start, and a scaling bottleneck.

But shared-nothing creates its own problem: **what happens to state when the node restarts?**
Without persistence, every node restart is a clean slate — all deployed apps are forgotten,
all secrets lost, all routes gone. Operators would need to manually redeploy everything.

`redb` solves this: it is an embedded database that persists to a single file on disk. On
restart, the Supervisor reads all apps, configs, and secrets back from disk and resumes exactly
where it left off. No central coordinator needed.

### Why Not Use a Central Database?

A central Postgres or etcd instance sounds appealing because it would give a single
authoritative view of cluster state. The problems:

1. **Network dependency**: Every cold start (< 10ms target) would require a DB query. Over a LAN
   this adds 1–2ms. Fine for containers. Unacceptable when the total budget is 10ms.
2. **Single point of failure**: If the DB is unreachable, no node can spawn instances. The whole
   cluster goes down.
3. **Complexity**: Running a highly-available database cluster requires its own ops burden
   (replication, failover, backups). This platform is designed to have minimal operational
   surface area.

With redb, each node is **completely autonomous**. The disk holds everything needed to serve
traffic. NATS is the control plane for receiving new deployments, but it is not needed to serve
existing traffic.

### Why redb and Not SQLite or RocksDB?

The specific requirements are:
1. **Concurrent reads while writing metrics**: Metrics are written every second; artifacts are
   read on every cold start. These must not block each other.
2. **Binary blob support**: Compiled Wasm artifacts are 2–15 MB blobs. SQLite stores them in
   `BLOB` columns with poor random-access performance. redb stores them as raw byte slices
   with O(1) lookup by key.
3. **Typed tables**: The code uses `TableDefinition<&str, &[u8]>` — type safety at compile time.
   SQLite uses string queries; a typo is a runtime error, not a compile error.
4. **Pure Rust**: No C FFI means no linking issues when cross-compiling, no `libsqlite3`
   required on the target system, and no CVEs from C code.

### How This Layer Fits in the System

```
                 ┌─────────────┐
  Node startup   │             │  redb file opened
  ─────────────► │    STORE    │◄──────────────────── /var/lib/wasm-node/state.redb
                 │             │
                 └──────┬──────┘
                        │
          ┌─────────────┼──────────────┬──────────────────┐
          │             │              │                  │
          ▼             ▼              ▼                  ▼
    [artifacts]    [configs]      [secrets]          [metrics]
    (compiled      (AppConfig     (encrypted         (1-min
    Wasm blobs)    per app)       bundles)           buckets)
          │             │              │
          ▼             ▼              ▼
      runtime       supervisor     secrets crate
      (loads        (reads on      (decrypts at
      artifact      cold start)    spawn time)
      bytes)
```

The Store is created once in `main.rs` and passed as a shared `Arc` to every component
that needs persistence.

---

---

## 1. Why redb?

| Property | redb | SQLite | RocksDB |
|----------|------|--------|---------|
| Written in | Pure Rust | C | C++ |
| ACID transactions | Yes | Yes | No (LSM) |
| Typed keys/values | Yes (via generics) | No | No |
| Concurrent reads | Yes (MVCC) | Limited | Yes |
| Binary blob support | Excellent | Mediocre | Good |
| Zero-config | Yes | Yes | No |

`redb` uses MVCC (Multi-Version Concurrency Control): reads never block writes.
This is critical because the Supervisor reads artifacts frequently while metrics writes happen concurrently.

---

## 2. Table Definitions

All tables live in `crates/storage/src/tables.rs`.

```rust
use redb::{TableDefinition, MultimapTableDefinition};

// ── ARTIFACT STORE ────────────────────────────────────────────────────────────
// Key   : app_id as &str  (e.g. "api-users:v1")
// Value : raw bytes of the AOT-compiled Wasmer artifact (can be several MB)
pub const ARTIFACTS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("artifacts");

// ── APP CONFIG ────────────────────────────────────────────────────────────────
// Key   : app_id
// Value : JSON-serialized AppConfig struct
pub const CONFIGS: TableDefinition<&str, &str> =
    TableDefinition::new("configs");

// ── ENCRYPTED SECRETS ─────────────────────────────────────────────────────────
// Key   : app_id
// Value : EncryptedBlob struct serialized with bincode
pub const SECRETS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("secrets");

// ── TELEMETRY (aggregated, 1-minute buckets) ──────────────────────────────────
// Key   : composite key "<app_id>:<timestamp_minute>" (e.g. "api-users:1735000000")
// Value : JSON-serialized MetricBucket
pub const METRICS: TableDefinition<&str, &str> =
    TableDefinition::new("metrics");
```

---

## 3. Database Handle

```rust
// crates/storage/src/lib.rs
use redb::Database;
use std::path::Path;
use std::sync::Arc;

pub mod artifact;
pub mod config;
pub mod secrets;
pub mod metrics;
pub mod tables;

#[derive(Clone)]
pub struct Store {
    pub db: Arc<Database>,
}

impl Store {
    /// Open (or create) the database at the given path.
    /// Creates all table definitions on first run.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;

        // Ensure tables exist (idempotent)
        let tx = db.begin_write()?;
        {
            tx.open_table(tables::ARTIFACTS)?;
            tx.open_table(tables::CONFIGS)?;
            tx.open_table(tables::SECRETS)?;
            tx.open_table(tables::METRICS)?;
        }
        tx.commit()?;

        Ok(Store { db: Arc::new(db) })
    }
}
```

---

## 4. Artifact Repository

Stores and retrieves AOT-compiled Wasm binaries.

```rust
// crates/storage/src/artifact.rs
use crate::{Store, tables::ARTIFACTS};
use common::{error::PlatformError, types::AppId};

impl Store {
    /// Persist a compiled Wasm artifact.
    /// `bytes` is the serialized Wasmer Artifact (output of Module::serialize()).
    pub fn store_artifact(&self, id: &AppId, bytes: &[u8]) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(ARTIFACTS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(id.0.as_str(), bytes)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))?;
        tracing::info!(app = %id.0, bytes = bytes.len(), "artifact stored");
        Ok(())
    }

    /// Load a compiled artifact. Returns None if not yet compiled.
    pub fn load_artifact(&self, id: &AppId) -> Result<Option<Vec<u8>>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(ARTIFACTS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let result = table.get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .map(|v| v.value().to_vec());
        Ok(result)
    }

    /// Check if an artifact exists without loading the bytes.
    pub fn artifact_exists(&self, id: &AppId) -> Result<bool, PlatformError> {
        Ok(self.load_artifact(id)?.is_some())
    }

    /// Delete an artifact (e.g. when an app is undeployed).
    pub fn delete_artifact(&self, id: &AppId) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(ARTIFACTS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(id.0.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }
}
```

---

## 5. Config Repository

Stores per-app configuration: env vars, fuel quota, memory limits, port.

```rust
// crates/storage/src/config.rs
use crate::{Store, tables::CONFIGS};
use common::{error::PlatformError, types::{AppConfig, AppId}};

impl Store {
    pub fn save_config(&self, config: &AppConfig) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(CONFIGS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(config.id.0.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn load_config(&self, id: &AppId) -> Result<Option<AppConfig>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(CONFIGS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table.get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            Some(v) => {
                let config = serde_json::from_str(v.value())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// List all deployed app IDs.
    pub fn list_apps(&self) -> Result<Vec<AppId>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(CONFIGS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut ids = Vec::new();
        for entry in table.iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            let (k, _) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            ids.push(AppId(k.value().to_string()));
        }
        Ok(ids)
    }
}
```

---

## 6. Metrics Repository

Aggregated metrics are buffered in RAM then written in 1-minute batches.

```rust
// crates/storage/src/metrics.rs
use crate::{Store, tables::METRICS};
use common::error::PlatformError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricBucket {
    pub app_id: String,
    pub minute_ts: u64,         // unix timestamp floored to minute
    pub request_count: u64,
    pub fuel_consumed_total: u64,
    pub fuel_consumed_avg: u64,
    pub ram_usage_peak_bytes: u64,
    pub latency_p50_ms: f64,
    pub latency_p99_ms: f64,
    pub trap_count: u64,        // Out-of-Fuel or OOM events
}

impl Store {
    pub fn write_metric_bucket(&self, bucket: &MetricBucket) -> Result<(), PlatformError> {
        let key = format!("{}:{}", bucket.app_id, bucket.minute_ts);
        let json = serde_json::to_string(bucket)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(METRICS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(key.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    /// Load last N minutes of metrics for an app.
    pub fn load_recent_metrics(
        &self,
        app_id: &str,
        last_n_minutes: u64,
    ) -> Result<Vec<MetricBucket>, PlatformError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cutoff = (now / 60 - last_n_minutes) * 60;
        let prefix = format!("{app_id}:");

        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(METRICS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut buckets = Vec::new();
        for entry in table.iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            let (k, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            if k.value().starts_with(&prefix) {
                let ts: u64 = k.value().split(':').last().unwrap().parse().unwrap_or(0);
                if ts >= cutoff {
                    let bucket: MetricBucket = serde_json::from_str(v.value())
                        .map_err(|e| PlatformError::Storage(e.to_string()))?;
                    buckets.push(bucket);
                }
            }
        }
        Ok(buckets)
    }

    /// Prune metrics older than `retention_minutes`.
    pub fn prune_old_metrics(&self, retention_minutes: u64) -> Result<u64, PlatformError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cutoff = (now / 60).saturating_sub(retention_minutes) * 60;

        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut removed = 0u64;
        {
            let mut table = tx.open_table(METRICS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            let stale_keys: Vec<String> = table.iter()
                .map_err(|e| PlatformError::Storage(e.to_string()))?
                .filter_map(|e| e.ok())
                .filter(|(k, _)| {
                    k.value().split(':').last()
                        .and_then(|ts| ts.parse::<u64>().ok())
                        .map(|ts| ts < cutoff)
                        .unwrap_or(false)
                })
                .map(|(k, _)| k.value().to_string())
                .collect();

            for key in stale_keys {
                table.remove(key.as_str())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                removed += 1;
            }
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(removed)
    }
}
```

---

## 7. Testing the Storage Layer

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn make_store() -> Store {
        let f = NamedTempFile::new().unwrap();
        Store::open(f.path()).unwrap()
    }

    #[test]
    fn test_artifact_roundtrip() {
        let store = make_store();
        let id = AppId::new("test-app", "v1");
        let bytes = b"fake wasm artifact bytes";
        store.store_artifact(&id, bytes).unwrap();
        let loaded = store.load_artifact(&id).unwrap().unwrap();
        assert_eq!(loaded, bytes);
    }

    #[test]
    fn test_config_roundtrip() {
        use common::types::{FuelQuota, MemoryPages};
        let store = make_store();
        let config = AppConfig {
            id: AppId::new("test-app", "v1"),
            fuel_quota: FuelQuota(100_000_000),
            memory_limit: MemoryPages(2048),
            env_vars: vec![("PORT".into(), "8080".into())],
            port: 8080,
        };
        store.save_config(&config).unwrap();
        let loaded = store.load_config(&config.id).unwrap().unwrap();
        assert_eq!(loaded.id, config.id);
    }
}

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Tables
- [ ] `ARTIFACTS`, `CONFIGS`, `SECRETS`, `METRICS`, `ROUTES`, `RAW_WASM`, `SCHEMA_META` table definitions compile
- [ ] `Store::open()` creates all tables on a fresh database without error
- [ ] `Store::open()` is idempotent — calling it twice on the same path does not panic or corrupt data

### Artifact Store
- [ ] `store_artifact()` writes bytes, `load_artifact()` reads the same bytes back
- [ ] `artifact_exists()` returns `false` before insert and `true` after
- [ ] `delete_artifact()` removes the entry; subsequent `load_artifact()` returns `None`
- [ ] Artifacts survive a `Store` drop and re-open (data is truly persisted)

### Config Store
- [ ] `save_config()` serializes `AppConfig` to JSON without loss of fields
- [ ] `load_config()` deserializes back to an equal `AppConfig`
- [ ] `list_apps()` returns all inserted `AppId` values
- [ ] Overwriting a config with `save_config()` replaces the old value (upsert)

### Metrics Store
- [ ] `write_metric_bucket()` stores a `MetricBucket`
- [ ] `load_recent_metrics()` returns only buckets within the requested time window
- [ ] `prune_old_metrics()` deletes old entries and returns the correct count
- [ ] Writing 1000 buckets and pruning runs in < 100ms

### Concurrency
- [ ] Two threads can call `load_artifact()` simultaneously without deadlock
- [ ] A read and a write transaction can run concurrently (MVCC — read is never blocked)

### Tests
- [ ] `test_artifact_roundtrip` passes
- [ ] `test_config_roundtrip` passes
- [ ] A test for `prune_old_metrics` passes
- [ ] All tests use `tempfile::NamedTempFile` (no shared global state between tests)
```
