// crates/storage/src/metrics.rs
use crate::{tables::METRICS, Store};
use common::error::PlatformError;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricBucket {
    pub app_id: String,
    pub minute_ts: u64, // unix timestamp floored to minute
    pub request_count: u64,
    pub fuel_consumed_total: u64,
    pub fuel_consumed_avg: u64,
    pub ram_usage_peak_bytes: u64,
    pub latency_p50_ms: f64,
    pub latency_p99_ms: f64,
    pub trap_count: u64, // Out-of-Fuel or OOM events
}

impl Store {
    pub fn write_metric_bucket(&self, bucket: &MetricBucket) -> Result<(), PlatformError> {
        let key = format!("{}:{}", bucket.app_id, bucket.minute_ts);
        let json =
            serde_json::to_string(bucket).map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(METRICS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(key.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
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

        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(METRICS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut buckets = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
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

        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut removed = 0u64;
        {
            let mut table = tx
                .open_table(METRICS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            let stale_keys: Vec<String> = table
                .iter()
                .map_err(|e| PlatformError::Storage(e.to_string()))?
                .filter_map(|e| e.ok())
                .filter(|(k, _)| {
                    k.value()
                        .split(':')
                        .last()
                        .and_then(|ts| ts.parse::<u64>().ok())
                        .map(|ts| ts < cutoff)
                        .unwrap_or(false)
                })
                .map(|(k, _)| k.value().to_string())
                .collect();

            for key in stale_keys {
                table
                    .remove(key.as_str())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                removed += 1;
            }
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(removed)
    }
}
