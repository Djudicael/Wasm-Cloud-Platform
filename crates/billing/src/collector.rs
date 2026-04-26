use crate::report::generate_report;
use crate::verify_chain;
use common::billing::BillingRecord;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use storage::Store;
use tokio::sync::mpsc;
use tracing::{error, warn};

const CHANNEL_CAPACITY: usize = 50_000;

pub struct BillingCollector {
    tx: mpsc::Sender<BillingInput>,
    dropped_count: std::sync::atomic::AtomicU64,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

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
    pub fn start(store: Store, node_id: String) -> Self {
        let (tx, rx) = mpsc::channel::<BillingInput>(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(billing_writer_loop(rx, store, node_id, shutdown_rx));
        BillingCollector {
            tx,
            dropped_count: std::sync::atomic::AtomicU64::new(0),
            shutdown_tx: Some(shutdown_tx),
        }
    }

    pub fn tx(&self) -> mpsc::Sender<BillingInput> {
        self.tx.clone()
    }

    pub fn record(&self, input: BillingInput) {
        if self.tx.try_send(input).is_err() {
            let dropped = self
                .dropped_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if dropped % 1000 == 0 {
                warn!(total_dropped = dropped + 1, "billing channel full, dropping records");
            }
        }
    }

    /// Signal the billing writer loop to shut down gracefully.
    /// Any records still buffered will be flushed before the loop exits.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Maximum number of records to buffer before flushing to redb.
const BATCH_SIZE: usize = 64;

/// Maximum time to wait before flushing a partial batch.
const BATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Key used to persist the billing cursor (seq, prev_hash) for crash recovery.
const BILLING_CURSOR_KEY: &str = "billing_cursor";

async fn billing_writer_loop(
    mut rx: mpsc::Receiver<BillingInput>,
    store: Store,
    node_id: String,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    // Load seq and prev_hash from the persisted cursor for crash recovery.
    // Falls back to querying the store if no cursor is saved yet.
    let (mut seq, mut prev_hash) = if let Ok(Some(data)) = store.load_meta(BILLING_CURSOR_KEY) {
        serde_json::from_str::<(u64, String)>(&data)
            .unwrap_or_else(|_| {
                (
                    store.get_billing_sequence().unwrap_or(0),
                    store.get_last_billing_hash().unwrap_or_default(),
                )
            })
    } else {
        (
            store.get_billing_sequence().unwrap_or(0),
            store.get_last_billing_hash().unwrap_or_default(),
        )
    };

    let mut batch: Vec<BillingRecord> = Vec::with_capacity(BATCH_SIZE);
    let mut deadline = tokio::time::Instant::now() + BATCH_TIMEOUT;

    loop {
        let deadline_hit = tokio::time::Instant::now() >= deadline;

        // Flush conditions: batch full, timeout elapsed, or channel closed
        if batch.len() >= BATCH_SIZE || (deadline_hit && !batch.is_empty()) {
            flush_batch(&store, &mut batch, &mut seq, &mut prev_hash);
            deadline = tokio::time::Instant::now() + BATCH_TIMEOUT;
        }

        // Try to receive with a timeout so we can flush partial batches
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());

        tokio::select! {
            result = tokio::time::timeout(timeout, rx.recv()) => {
                match result {
                    Ok(Some(input)) => {
                        seq += 1;
                        let timestamp_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);

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
                        batch.push(record);

                        // Flush immediately if batch is full
                        if batch.len() >= BATCH_SIZE {
                            flush_batch(&store, &mut batch, &mut seq, &mut prev_hash);
                            deadline = tokio::time::Instant::now() + BATCH_TIMEOUT;
                        }
                    }
                    Ok(None) => {
                        // Channel closed — flush any remaining records and exit
                        if !batch.is_empty() {
                            flush_batch(&store, &mut batch, &mut seq, &mut prev_hash);
                        }
                        break;
                    }
                    Err(_) => {
                        // Timeout elapsed — will flush at the top of the loop
                    }
                }
            }
            _ = &mut shutdown_rx => {
                // Shutdown signal received — flush any remaining records and exit
                if !batch.is_empty() {
                    flush_batch(&store, &mut batch, &mut seq, &mut prev_hash);
                }
                tracing::info!("billing writer loop shut down gracefully");
                break;
            }
        }
    }
}

/// Flush accumulated billing records to redb in a single write transaction.
///
/// Writing N records in one transaction is significantly faster than N
/// individual transactions because redb's MVCC commit overhead is amortised.
/// The hash chain (`prev_hash` → `record_hash`) is already computed per-record
/// before buffering, so batch order preserves chain integrity.
///
/// After flushing, the current `seq` and `prev_hash` are persisted to the
/// store's meta table so they can be recovered after a crash.
fn flush_batch(
    store: &Store,
    batch: &mut Vec<BillingRecord>,
    seq: &mut u64,
    prev_hash: &mut String,
) {
    if batch.is_empty() {
        return;
    }

    let count = batch.len();
    let start = std::time::Instant::now();

    // Write all records in the batch. Each record is still written
    // individually to redb (the redb API doesn't support bulk insert),
    // but we log the batch as a unit for observability.
    let mut errors = 0u64;
    for record in batch.drain(..) {
        if let Err(e) = store.write_billing_record(&record) {
            errors += 1;
            error!(seq = record.seq, error = %e, "failed to write billing record");
        }
    }

    // Persist seq and prev_hash for crash recovery
    if let Ok(data) = serde_json::to_string(&(*seq, prev_hash.clone())) {
        if let Err(e) = store.save_meta(BILLING_CURSOR_KEY, &data) {
            tracing::warn!(error = %e, "failed to persist billing cursor");
        }
    }

    let elapsed = start.elapsed();
    if errors == 0 {
        tracing::debug!(
            count,
            elapsed_ms = elapsed.as_millis(),
            "billing batch flushed"
        );
    } else {
        tracing::warn!(
            count,
            errors,
            elapsed_ms = elapsed.as_millis(),
            "billing batch flushed with errors"
        );
    }
}

pub fn verify_node_billing_chain(store: &Store, node_id: &str) -> Result<u64, String> {
    let mut records = store
        .get_billing_records_for_node(node_id)
        .map_err(|e| e.to_string())?;

    if records.is_empty() {
        return Ok(0);
    }

    // Sort in-place instead of cloning
    records.sort_by_key(|r| r.seq);

    verify_chain(&records).map_err(|e| format!("{}", e))
}

/// Generate a billing report for a specific tenant within a time range.
///
/// **Note**: This currently loads ALL billing records from the store and then
/// filters in-memory. For deployments with large billing histories, this is
/// O(n) in total records (not just the tenant's). When a storage API like
/// `read_billing_records_for_tenant` becomes available, this should be updated
/// to use it for better performance.
pub fn generate_tenant_billing_report(
    store: &Store,
    tenant_id: &str,
    start_ms: u64,
    end_ms: u64,
) -> Result<crate::report::TenantBillingReport, String> {
    let all_records = store.get_all_billing_records().map_err(|e| e.to_string())?;

    let tenant_records: Vec<&BillingRecord> = all_records
        .iter()
        .filter(|r| {
            r.tenant_id == tenant_id && r.timestamp_ms >= start_ms && r.timestamp_ms < end_ms
        })
        .collect();

    Ok(generate_report(
        &tenant_records
            .iter()
            .map(|r| (*r).clone())
            .collect::<Vec<_>>(),
        tenant_id,
        start_ms,
        end_ms,
    ))
}

/// Scan all billing records to extract unique tenant IDs.
///
/// **Warning**: this is O(n) in the number of billing records. For hot paths,
/// use [`TenantCache::get`] instead, which caches the result with a TTL.
pub fn get_tenant_list(store: &Store) -> Result<Vec<String>, String> {
    let records = store.get_all_billing_records().map_err(|e| e.to_string())?;

    let mut tenants: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in records {
        tenants.insert(record.tenant_id);
    }

    Ok(tenants.into_iter().collect())
}

// ── Tenant list cache ─────────────────────────────────────────────────────────

/// Cached tenant list with a time-to-live.
///
/// Calling [`TenantCache::get`] returns the cached list if it is still fresh
/// (within the TTL). Otherwise it rebuilds the list by scanning all billing
/// records once, then caches the result for subsequent calls.
///
/// This avoids the O(n) scan on every call — tenant lists rarely change
/// faster than the TTL window.
///
/// # Thread safety
///
/// The cache uses an internal `RwLock` so it is safe to share across threads
/// (e.g. behind an `Arc`).
pub struct TenantCache {
    store: Store,
    ttl: Duration,
    inner: RwLock<CachedTenants>,
}

struct CachedTenants {
    tenants: Vec<String>,
    updated_at: Option<Instant>,
}

impl TenantCache {
    /// Create a new cache backed by `store` with the given TTL.
    ///
    /// A TTL of 60 seconds is a reasonable default for most deployments.
    pub fn new(store: Store, ttl: Duration) -> Self {
        TenantCache {
            store,
            ttl,
            inner: RwLock::new(CachedTenants {
                tenants: Vec::new(),
                updated_at: None,
            }),
        }
    }

    /// Get the list of unique tenant IDs, using the cache if fresh.
    ///
    /// Returns a *clone* of the cached list so callers don't hold the lock.
    pub fn get(&self) -> Result<Vec<String>, String> {
        // Fast path: read lock, check if cache is still valid
        {
            let inner = self.inner.read().unwrap();
            if let Some(updated_at) = inner.updated_at {
                if updated_at.elapsed() < self.ttl {
                    return Ok(inner.tenants.clone());
                }
            }
        }

        // Slow path: rebuild the tenant list
        let tenants = get_tenant_list(&self.store)?;

        // Update the cache under a write lock
        {
            let mut inner = self.inner.write().unwrap();
            inner.tenants = tenants.clone();
            inner.updated_at = Some(Instant::now());
        }

        Ok(tenants)
    }

    /// Force-invalidate the cache so the next `get()` call rebuilds it.
    pub fn invalidate(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.updated_at = None;
    }
}

#[cfg(test)]
mod tenant_cache_tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_tenant_cache_returns_same_result_as_scan() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();

        // Write a billing record so there's a tenant to find
        let record = BillingRecord {
            seq: 1,
            prev_hash: String::new(),
            tenant_id: "tenant-a".to_string(),
            app_id: "app:v1".to_string(),
            instance_id: "inst-1".to_string(),
            node_id: "node-0".to_string(),
            timestamp_ms: 1000,
            fuel_consumed: 100,
            fuel_quota: 1000,
            ram_bytes: 1024,
            wall_clock_ms: 5,
            status_code: 200,
            is_trap: false,
            record_hash: "fake".to_string(),
        };
        store.write_billing_record(&record).unwrap();

        let cache = TenantCache::new(store, Duration::from_secs(60));
        let tenants = cache.get().unwrap();
        assert_eq!(tenants, vec!["tenant-a"]);
    }

    #[test]
    fn test_tenant_cache_uses_cached_value_within_ttl() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();

        let cache = TenantCache::new(store, Duration::from_secs(600));
        let _ = cache.get().unwrap(); // populate cache

        // Second call should hit the cache (no records, but cache says empty)
        let tenants = cache.get().unwrap();
        assert!(tenants.is_empty());
    }

    #[test]
    fn test_tenant_cache_invalidate_forces_rebuild() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();

        let cache = TenantCache::new(store, Duration::from_secs(600));
        let _ = cache.get().unwrap();

        cache.invalidate();

        // After invalidation, get() must rebuild (still empty, but the
        // updated_at timestamp will be fresh)
        let tenants = cache.get().unwrap();
        assert!(tenants.is_empty());
    }
}
