// crates/metrics/src/collector.rs
use super::ExecutionSample;
use std::collections::HashMap;
use std::time::Duration;
use storage::{metrics::MetricBucket, Store};
use tokio::sync::mpsc;
use tracing::error;

const CHANNEL_CAPACITY: usize = 10_000;

/// Retention period for metric buckets in minutes (7 days).
const METRICS_RETENTION_MINUTES: u64 = 60 * 24 * 7;

pub struct MetricsCollector {
    tx: mpsc::Sender<ExecutionSample>,
}

impl MetricsCollector {
    /// Create the collector and start the background aggregation task.
    pub fn start(store: Store) -> Self {
        let (tx, rx) = mpsc::channel::<ExecutionSample>(CHANNEL_CAPACITY);
        tokio::spawn(aggregation_loop(rx, store));
        MetricsCollector { tx }
    }

    /// Record an execution sample. Non-blocking (drops if channel is full).
    pub fn record(&self, sample: ExecutionSample) {
        if self.tx.try_send(sample).is_err() {
            tracing::warn!("metrics channel full, dropping sample");
        }
    }

    pub fn sender(&self) -> mpsc::Sender<ExecutionSample> {
        self.tx.clone()
    }
}

/// Background task: accumulates samples and flushes to redb once per minute.
async fn aggregation_loop(mut rx: mpsc::Receiver<ExecutionSample>, store: Store) {
    // In-memory accumulators: app_id → accumulated bucket data
    let mut buckets: HashMap<String, InProgressBucket> = HashMap::new();
    let mut flush_interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            Some(sample) = rx.recv() => {
                let minute_ts = floor_to_minute(sample.timestamp_ms);
                let bucket = buckets.entry(sample.app_id.clone())
                    .or_insert_with(|| InProgressBucket::new(&sample.app_id, minute_ts));
                bucket.add(&sample);
            }
            _ = flush_interval.tick() => {
                // Only flush buckets for completed minutes, not the current one.
                // The current minute's bucket may still be accumulating samples.
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let current_minute = floor_to_minute(now_ms);

                let all: Vec<_> = buckets.drain().collect();
                for (app_id, bucket) in all {
                    if bucket.minute_ts == current_minute {
                        // Still in progress — re-insert for next cycle
                        buckets.insert(app_id, bucket);
                    } else {
                        // Completed minute — finalize and persist
                        let mb = bucket.finalize();
                        if let Err(e) = store.write_metric_bucket(&mb) {
                            error!(error = %e, "failed to write metric bucket");
                        }
                    }
                }
                // Prune old metrics
                store.prune_old_metrics(METRICS_RETENTION_MINUTES).ok();
            }
        }
    }
}

struct InProgressBucket {
    app_id: String,
    minute_ts: u64,
    count: u64,
    fuel_sum: u64,
    ram_peak: u64,
    latency_samples: Vec<u64>,
    trap_count: u64,
}

impl InProgressBucket {
    fn new(app_id: &str, minute_ts: u64) -> Self {
        InProgressBucket {
            app_id: app_id.to_string(),
            minute_ts,
            count: 0,
            fuel_sum: 0,
            ram_peak: 0,
            latency_samples: Vec::new(),
            trap_count: 0,
        }
    }

    fn add(&mut self, s: &ExecutionSample) {
        self.count += 1;
        self.fuel_sum += s.fuel_consumed;
        self.ram_peak = self.ram_peak.max(s.ram_bytes as u64);
        self.latency_samples.push(s.wall_clock_ms);
        if s.is_trap {
            self.trap_count += 1;
        }
    }

    fn finalize(mut self) -> MetricBucket {
        self.latency_samples.sort_unstable();
        let p50 = percentile(&self.latency_samples, 50) as f64;
        let p99 = percentile(&self.latency_samples, 99) as f64;
        MetricBucket {
            app_id: self.app_id,
            minute_ts: self.minute_ts,
            request_count: self.count,
            fuel_consumed_total: self.fuel_sum,
            fuel_consumed_avg: if self.count > 0 {
                self.fuel_sum / self.count
            } else {
                0
            },
            ram_usage_peak_bytes: self.ram_peak,
            latency_p50_ms: p50,
            latency_p99_ms: p99,
            trap_count: self.trap_count,
        }
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * p / 100).min(sorted.len().saturating_sub(1));
    sorted[idx]
}

fn floor_to_minute(timestamp_ms: u64) -> u64 {
    (timestamp_ms / 1000 / 60) * 60
}
