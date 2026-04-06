# Step 28 — Billing & Fuel Accounting

## Goal
Implement a tamper-evident, per-tenant fuel accounting system. The system must:
- Record every Wasm execution's fuel consumption with tenant attribution
- Produce billing-grade reports (auditable, immutable once written)
- Support per-app and per-tenant aggregation
- Separate billing data from operational metrics (different retention, different access)
- Enable usage-based pricing (pay per fuel unit consumed)

---

## Context & Rationale

### The Problem This Solves

Step 11 (Metrics) tracks fuel consumption per app for operational visibility — dashboards,
alerting, capacity planning. But operational metrics are:

- **Aggregated**: p50/p99 latency, average fuel per request — not individual request records
- **Ephemeral**: pruned after 7 days (step 26)
- **Mutable**: metrics can be re-aggregated, overwritten, or lost during GC

Billing requires a different contract:
- **Per-request granularity**: each execution must be recorded individually
- **Immutable**: once written, a billing record cannot be modified or deleted by the platform
- **Long retention**: billing records must survive for months (contractual, legal, tax reasons)
- **Attributable**: each record links to a tenant, app, version, and time window

Without a dedicated billing system, the only option is to reconstruct usage from Prometheus
metrics — which are aggregated, sampled, and lack per-request precision. This is unacceptable
for invoicing.

### Why Fuel Is the Ideal Billing Unit

Traditional cloud platforms bill by:
- **Time**: "your container ran for 3,600 seconds" — but was it doing work or sleeping?
- **vCPU-seconds**: "you used 0.5 vCPU for 3,600s" — but vCPU is a statistical average, not
  actual computation
- **Request count**: "you made 1M requests" — but a request that does 1ms of work costs the
  same as one that does 10s of work

Fuel is **deterministic computation**:
- 1 fuel unit = 1 Wasm instruction executed
- A request that parses a small JSON body consumes 50,000 fuel
- A request that resizes an image consumes 50,000,000 fuel
- The image resize costs 1,000× more — and the billing reflects that exactly

This alignment between cost and billing eliminates the incentive mismatches that plague
time-based billing (where idle containers are as expensive as active ones).

### Why Separate from Operational Metrics

```
                    Operational Metrics         Billing Records
                    ──────────────────          ──────────────
Granularity:        1-minute aggregates         Per-request
Retention:          7 days                      12+ months
Mutability:         Overwritten by GC           Append-only, immutable
Purpose:            Dashboards, alerts          Invoices, audits
Storage:            redb [metrics] table        redb [billing] table + periodic export
Access:             Any operator                Billing system only (read-only API)
```

Mixing billing data with operational metrics would either:
- Force 12-month retention on all metrics (wasting disk), or
- Force 7-day retention on billing (losing revenue data)

### Tamper Evidence: Why Chained Hashes

Each billing record includes a hash of the previous record (`prev_hash`). This creates
a hash chain similar to a blockchain — modifying any record breaks the chain. An auditor
can verify the entire billing history by recomputing the chain and checking for gaps.

This is not cryptographic proof against a sophisticated attacker with root access to the
node (they could recompute the chain after modification). But it protects against:
- Accidental data corruption
- Software bugs that modify existing records
- Unauthorized deletions

For stronger guarantees, billing records can be periodically exported to an external
append-only store (S3 with object lock, or a ledger database).

---

---

## 1. Billing Record Structure

```rust
// crates/billing/src/lib.rs
pub mod collector;
pub mod export;
pub mod report;

use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};

/// A single billing record, written after each Wasm execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingRecord {
    /// Monotonically increasing sequence number within this node.
    pub seq: u64,

    /// SHA-256 of the previous record (hex string). Empty for the first record.
    pub prev_hash: String,

    /// Tenant identifier (maps to the customer being billed).
    pub tenant_id: String,

    /// App identifier (e.g. "api-users:v2").
    pub app_id: String,

    /// Instance that handled the request.
    pub instance_id: String,

    /// Node that processed the request.
    pub node_id: String,

    /// Request timestamp (milliseconds since UNIX epoch).
    pub timestamp_ms: u64,

    /// Fuel units consumed by this execution.
    pub fuel_consumed: u64,

    /// Fuel quota that was allocated for this execution.
    pub fuel_quota: u64,

    /// Peak linear memory usage in bytes.
    pub ram_bytes: u64,

    /// Wall-clock execution time in milliseconds.
    pub wall_clock_ms: u64,

    /// HTTP status code returned (200, 500, etc.).
    pub status_code: u16,

    /// Whether the execution ended in a trap (OOM, out of fuel).
    pub is_trap: bool,

    /// SHA-256 of this record (computed over all fields above).
    pub record_hash: String,
}

impl BillingRecord {
    /// Compute the hash of this record (excluding the record_hash field itself).
    pub fn compute_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.seq.to_le_bytes());
        hasher.update(self.prev_hash.as_bytes());
        hasher.update(self.tenant_id.as_bytes());
        hasher.update(self.app_id.as_bytes());
        hasher.update(self.instance_id.as_bytes());
        hasher.update(self.node_id.as_bytes());
        hasher.update(self.timestamp_ms.to_le_bytes());
        hasher.update(self.fuel_consumed.to_le_bytes());
        hasher.update(self.fuel_quota.to_le_bytes());
        hasher.update(self.ram_bytes.to_le_bytes());
        hasher.update(self.wall_clock_ms.to_le_bytes());
        hasher.update(self.status_code.to_le_bytes());
        hasher.update(&[self.is_trap as u8]);
        format!("{:x}", hasher.finalize())
    }
}
```

---

## 2. Tenant Mapping

Each app is owned by a tenant. The mapping is stored in `AppConfig` and resolved
at billing time.

```rust
// Extension to crates/common/src/types.rs (AppConfig)
//
// AppConfig gains:
//   pub tenant_id: String,
//
// Set during deploy:
//   wasm-ctl deploy --app api-users --tenant acme-corp ...
//
// Default: if no tenant is specified, the app_name is used as the tenant_id.
// This handles single-tenant deployments without requiring explicit tenant setup.
```

---

## 3. Billing Collector

Like the metrics collector (step 11), the billing collector uses a non-blocking mpsc
channel. But unlike metrics, billing records are written **individually** (not aggregated)
because each record is a billable event.

```rust
// crates/billing/src/collector.rs
use super::BillingRecord;
use storage::Store;
use tokio::sync::mpsc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, error};

const CHANNEL_CAPACITY: usize = 50_000;

pub struct BillingCollector {
    tx: mpsc::Sender<BillingInput>,
}

/// Input from the Supervisor after each Wasm execution.
pub struct BillingInput {
    pub tenant_id: String,
    pub app_id: String,
    pub instance_id: String,
    pub node_id: String,
    pub fuel_consumed: u64,
    pub fuel_quota: u64,
    pub ram_bytes: u64,
    pub wall_clock_ms: u64,
    pub status_code: u16,
    pub is_trap: bool,
}

impl BillingCollector {
    /// Create the collector and start the background writer task.
    pub fn start(store: Store, node_id: String) -> Self {
        let (tx, rx) = mpsc::channel::<BillingInput>(CHANNEL_CAPACITY);
        tokio::spawn(billing_writer_loop(rx, store, node_id));
        BillingCollector { tx }
    }

    /// Record a billing event. Non-blocking (drops if channel is full).
    pub fn record(&self, input: BillingInput) {
        if let Err(_) = self.tx.try_send(input) {
            tracing::warn!("billing channel full, dropping record — this should be investigated");
        }
    }
}

async fn billing_writer_loop(
    mut rx: mpsc::Receiver<BillingInput>,
    store: Store,
    node_id: String,
) {
    let mut seq = store.get_billing_sequence().unwrap_or(0);
    let mut prev_hash = store.get_last_billing_hash().unwrap_or_default();

    while let Some(input) = rx.recv().await {
        seq += 1;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut record = BillingRecord {
            seq,
            prev_hash: prev_hash.clone(),
            tenant_id: input.tenant_id,
            app_id: input.app_id,
            instance_id: input.instance_id,
            node_id: node_id.clone(),
            timestamp_ms,
            fuel_consumed: input.fuel_consumed,
            fuel_quota: input.fuel_quota,
            ram_bytes: input.ram_bytes,
            wall_clock_ms: input.wall_clock_ms,
            status_code: input.status_code,
            is_trap: input.is_trap,
            record_hash: String::new(),
        };

        record.record_hash = record.compute_hash();
        prev_hash = record.record_hash.clone();

        if let Err(e) = store.write_billing_record(&record) {
            error!(seq, error = %e, "failed to write billing record");
        }
    }
}
```

---

## 4. Billing Storage

Billing records are stored in a dedicated redb table with a different retention policy
than operational metrics.

```rust
// crates/storage/src/billing.rs
use crate::{Store, tables::BILLING};
use common::error::PlatformError;
use billing::BillingRecord;

impl Store {
    /// Write a single billing record. Append-only — never overwrites.
    pub fn write_billing_record(&self, record: &BillingRecord) -> Result<(), PlatformError> {
        let key = format!("{}:{}", record.node_id, record.seq);
        let json = serde_json::to_string(record)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(BILLING)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(key.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    /// Get the last billing sequence number for this node.
    pub fn get_billing_sequence(&self) -> Result<u64, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(BILLING)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut max_seq = 0u64;
        for entry in table.iter().map_err(|e| PlatformError::Storage(e.to_string()))? {
            let (_, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let record: BillingRecord = serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            max_seq = max_seq.max(record.seq);
        }
        Ok(max_seq)
    }

    /// Get the hash of the last billing record (for chaining).
    pub fn get_last_billing_hash(&self) -> Result<String, PlatformError> {
        let seq = self.get_billing_sequence()?;
        if seq == 0 {
            return Ok(String::new());
        }
        // Find the record with the highest sequence number
        // (implementation: iterate and find max, or use a metadata key)
        Ok(String::new()) // Placeholder — real implementation uses metadata key
    }
}
```

---

## 5. Billing Reports

Aggregated reports for invoicing, generated from the raw billing records.

```rust
// crates/billing/src/report.rs
use super::BillingRecord;
use serde::{Deserialize, Serialize};

/// A billing report for a single tenant over a time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantBillingReport {
    pub tenant_id: String,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub total_requests: u64,
    pub total_fuel_consumed: u64,
    pub total_wall_clock_ms: u64,
    pub peak_ram_bytes: u64,
    pub trap_count: u64,
    pub per_app: Vec<AppUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUsage {
    pub app_id: String,
    pub request_count: u64,
    pub fuel_consumed: u64,
    pub avg_fuel_per_request: u64,
    pub wall_clock_ms: u64,
    pub trap_count: u64,
}

/// Generate a billing report for a tenant within a time window.
pub fn generate_report(
    records: &[BillingRecord],
    tenant_id: &str,
    start_ms: u64,
    end_ms: u64,
) -> TenantBillingReport {
    let tenant_records: Vec<&BillingRecord> = records.iter()
        .filter(|r| r.tenant_id == tenant_id
            && r.timestamp_ms >= start_ms
            && r.timestamp_ms < end_ms)
        .collect();

    let mut per_app: std::collections::HashMap<String, AppUsage> =
        std::collections::HashMap::new();

    let mut total_fuel = 0u64;
    let mut total_wall = 0u64;
    let mut peak_ram = 0u64;
    let mut trap_count = 0u64;

    for r in &tenant_records {
        total_fuel += r.fuel_consumed;
        total_wall += r.wall_clock_ms;
        peak_ram = peak_ram.max(r.ram_bytes);
        if r.is_trap { trap_count += 1; }

        let app = per_app.entry(r.app_id.clone()).or_insert(AppUsage {
            app_id: r.app_id.clone(),
            request_count: 0,
            fuel_consumed: 0,
            avg_fuel_per_request: 0,
            wall_clock_ms: 0,
            trap_count: 0,
        });
        app.request_count += 1;
        app.fuel_consumed += r.fuel_consumed;
        app.wall_clock_ms += r.wall_clock_ms;
        if r.is_trap { app.trap_count += 1; }
    }

    // Compute averages
    let mut apps: Vec<AppUsage> = per_app.into_values().collect();
    for app in &mut apps {
        app.avg_fuel_per_request = if app.request_count > 0 {
            app.fuel_consumed / app.request_count
        } else { 0 };
    }
    apps.sort_by(|a, b| b.fuel_consumed.cmp(&a.fuel_consumed));

    TenantBillingReport {
        tenant_id: tenant_id.to_string(),
        period_start_ms: start_ms,
        period_end_ms: end_ms,
        total_requests: tenant_records.len() as u64,
        total_fuel_consumed: total_fuel,
        total_wall_clock_ms: total_wall,
        peak_ram_bytes: peak_ram,
        trap_count,
        per_app: apps,
    }
}
```

---

## 6. Periodic Export

Billing records are periodically exported to an external store for long-term retention.
This keeps the local redb billing table bounded while ensuring records survive node loss.

```rust
// crates/billing/src/export.rs
use super::BillingRecord;
use common::error::PlatformError;

/// Export strategy — pluggable backend for billing record archival.
#[async_trait::async_trait]
pub trait BillingExporter: Send + Sync {
    /// Export a batch of billing records to the external store.
    async fn export_batch(&self, records: &[BillingRecord]) -> Result<(), PlatformError>;
}

/// S3-compatible exporter (Minio, AWS S3, R2).
/// Writes billing records as NDJSON files, one per export batch.
pub struct S3Exporter {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: String,
}

#[async_trait::async_trait]
impl BillingExporter for S3Exporter {
    async fn export_batch(&self, records: &[BillingRecord]) -> Result<(), PlatformError> {
        // 1. Serialize records to NDJSON (one JSON object per line)
        let mut body = String::new();
        for record in records {
            let line = serde_json::to_string(record)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            body.push_str(&line);
            body.push('\n');
        }

        // 2. Generate object key: prefix/node-0/2026/04/06/1712400000.ndjson
        let first_ts = records.first().map(|r| r.timestamp_ms).unwrap_or(0);
        let key = format!(
            "{}/{}/{}.ndjson",
            self.prefix, records[0].node_id, first_ts
        );

        // 3. PUT to S3 (using reqwest or aws-sdk-s3)
        // Implementation depends on chosen S3 client library.
        tracing::info!(key = %key, records = records.len(), "billing batch exported to S3");
        Ok(())
    }
}

/// Local file exporter (for development or single-node deployments).
pub struct FileExporter {
    pub dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl BillingExporter for FileExporter {
    async fn export_batch(&self, records: &[BillingRecord]) -> Result<(), PlatformError> {
        let first_ts = records.first().map(|r| r.timestamp_ms).unwrap_or(0);
        let path = self.dir.join(format!("billing_{}.ndjson", first_ts));

        let mut body = String::new();
        for record in records {
            let line = serde_json::to_string(record)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            body.push_str(&line);
            body.push('\n');
        }

        tokio::fs::write(&path, body.as_bytes()).await
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        tracing::info!(path = %path.display(), records = records.len(), "billing batch exported");
        Ok(())
    }
}
```

---

## 7. Billing Export Loop

Runs hourly, exports accumulated records, then marks them as exported.

```rust
// crates/billing/src/export.rs (continued)
use storage::Store;
use std::sync::Arc;
use std::time::Duration;

pub fn start_export_loop(
    store: Store,
    exporter: Arc<dyn BillingExporter>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;

            // Read unexported records in batches of 10,000
            match store.read_unexported_billing_records(10_000) {
                Ok(records) if records.is_empty() => {
                    // Nothing to export
                }
                Ok(records) => {
                    let count = records.len();
                    let last_seq = records.last().map(|r| r.seq).unwrap_or(0);

                    match exporter.export_batch(&records).await {
                        Ok(()) => {
                            // Mark records as exported (update watermark)
                            store.set_billing_export_watermark(last_seq).ok();
                            tracing::info!(count, last_seq, "billing export complete");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "billing export failed — will retry next tick");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to read billing records for export");
                }
            }
        }
    });
}
```

---

## 8. Hash Chain Verification

An operator or auditor can verify the integrity of the billing chain at any time.

```rust
// crates/billing/src/lib.rs (continued)
use sha2::{Sha256, Digest};

/// Verify the hash chain of billing records.
/// Returns Ok(count) if all records are consistent, Err on first inconsistency.
pub fn verify_chain(records: &[BillingRecord]) -> Result<u64, ChainError> {
    let mut expected_prev = String::new();

    for (i, record) in records.iter().enumerate() {
        // Check prev_hash links to the previous record
        if record.prev_hash != expected_prev {
            return Err(ChainError::BrokenLink {
                seq: record.seq,
                expected: expected_prev,
                actual: record.prev_hash.clone(),
            });
        }

        // Verify the record's own hash
        let computed = record.compute_hash();
        if computed != record.record_hash {
            return Err(ChainError::TamperedRecord {
                seq: record.seq,
                expected: computed,
                actual: record.record_hash.clone(),
            });
        }

        expected_prev = record.record_hash.clone();
    }

    Ok(records.len() as u64)
}

#[derive(Debug)]
pub enum ChainError {
    BrokenLink { seq: u64, expected: String, actual: String },
    TamperedRecord { seq: u64, expected: String, actual: String },
}
```

---

## 9. CLI Commands

```
# View billing summary for a tenant
wasm-ctl billing report --tenant acme-corp --period 2026-04
# Output:
# Tenant: acme-corp
# Period: 2026-04-01 to 2026-04-30
# Total requests: 2,456,789
# Total fuel consumed: 1,234,567,890,000
# Per-app breakdown:
#   api-users:v3   1,200,000 req   800B fuel   avg 667K fuel/req
#   payments:v2      456,789 req   300B fuel   avg 656K fuel/req

# Export billing records (force immediate export)
wasm-ctl billing export --node node-0

# Verify billing chain integrity
wasm-ctl billing verify --node node-0
# Output:
# Verified 1,234,567 records — chain is consistent ✓

# View raw billing records (for debugging)
wasm-ctl billing records --app api-users --last 10
```

---

## 10. New redb Table

The billing system requires a new table in the redb schema:

```rust
// crates/storage/src/tables.rs (extended)
use redb::TableDefinition;

/// Billing records — append-only, per-request fuel accounting.
/// Key: "node_id:sequence_number" (e.g. "node-0:12345")
/// Value: JSON-serialized BillingRecord
pub const BILLING: TableDefinition<&str, &str> = TableDefinition::new("billing");

/// Billing export watermark — tracks the last exported sequence number.
/// Key: "export_watermark"
/// Value: sequence number as string
// Stored in SCHEMA_META table.
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Recording
- [ ] Every Wasm execution produces a `BillingRecord` with correct fuel, RAM, and timing data
- [ ] Records are written to the `billing` redb table via non-blocking mpsc channel
- [ ] The billing channel dropping a record emits a warning metric (investigate if it happens)

### Hash Chain
- [ ] Each record's `prev_hash` correctly references the previous record's `record_hash`
- [ ] `verify_chain()` returns `Ok` for a valid chain of 100,000+ records
- [ ] Modifying any record's fuel value causes `verify_chain()` to return `TamperedRecord`
- [ ] Deleting a record from the middle causes `verify_chain()` to return `BrokenLink`

### Reporting
- [ ] `wasm-ctl billing report --tenant X --period Y` produces a correct per-app breakdown
- [ ] Fuel totals in the report match the sum of individual records (cross-check)

### Export
- [ ] The export loop runs every hour and exports unexported records
- [ ] After export, the watermark advances and the same records are not exported again
- [ ] Export failure does not lose data — records remain in redb and are retried next tick
- [ ] S3 exporter writes valid NDJSON files to the configured bucket

### Tenant Mapping
- [ ] Apps without an explicit `tenant_id` use the app name as the tenant
- [ ] Multi-tenant deployments correctly attribute fuel to the right tenant

### Isolation
- [ ] Billing writes do not block the request path (non-blocking channel)
- [ ] Billing records are not deleted by the artifact GC (step 26) — separate retention
- [ ] Billing export does not interfere with operational metrics
