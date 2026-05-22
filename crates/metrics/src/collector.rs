use super::ExecutionSample;
use std::collections::HashMap;
use std::time::Duration;
use storage::{metrics::MetricBucket, Store};
use tokio::sync::mpsc;
use tracing::error;

const CHANNEL_CAPACITY: usize = 10_000;

/// Retention period for metric buckets in minutes (7 days).
const METRICS_RETENTION_MINUTES: u64 = 60 * 24 * 7;
type BucketKey = (String, u64);

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
    // In-memory accumulators: (app_id, minute_ts) -> accumulated bucket data
    let mut buckets: HashMap<BucketKey, InProgressBucket> = HashMap::new();
    let mut flush_interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            Some(sample) = rx.recv() => {
                record_sample(&mut buckets, sample);
            }
            _ = flush_interval.tick() => {
                // Only flush buckets for completed minutes, not the current one.
                // The current minute's bucket may still be accumulating samples.
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let current_minute = floor_to_minute(now_ms);

                for bucket in take_completed_buckets(&mut buckets, current_minute) {
                    let mb = bucket.finalize();
                    if let Err(e) = store.write_metric_bucket(&mb) {
                        error!(error = %e, "failed to write metric bucket");
                    }
                }
                store.prune_old_metrics(METRICS_RETENTION_MINUTES).ok();
            }
        }
    }
}

fn record_sample(buckets: &mut HashMap<BucketKey, InProgressBucket>, sample: ExecutionSample) {
    let minute_ts = floor_to_minute(sample.timestamp_ms);
    let bucket = buckets
        .entry((sample.app_id.clone(), minute_ts))
        .or_insert_with(|| InProgressBucket::new(&sample.app_id, minute_ts));
    bucket.add(&sample);
}

fn take_completed_buckets(
    buckets: &mut HashMap<BucketKey, InProgressBucket>,
    current_minute: u64,
) -> Vec<InProgressBucket> {
    let all: Vec<_> = buckets.drain().collect();
    let mut completed = Vec::new();

    for (key, bucket) in all {
        if bucket.minute_ts < current_minute {
            completed.push(bucket);
        } else {
            buckets.insert(key, bucket);
        }
    }

    completed
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

#[cfg(test)]
mod tests {
    use super::{floor_to_minute, record_sample, take_completed_buckets, InProgressBucket};
    use crate::ExecutionSample;
    use std::collections::HashMap;

    fn sample(app_id: &str, timestamp_ms: u64, wall_clock_ms: u64) -> ExecutionSample {
        ExecutionSample {
            app_id: app_id.to_string(),
            instance_id: "inst-1".to_string(),
            timestamp_ms,
            fuel_consumed: 100,
            fuel_limit: 1000,
            ram_bytes: 2048,
            wall_clock_ms,
            status_code: 200,
            is_trap: false,
            trap_reason: None,
            trace_id: None,
        }
    }

    #[test]
    fn test_record_sample_keys_buckets_by_app_and_minute() {
        let mut buckets = HashMap::new();
        let just_before = 12 * 60 * 60 * 1000 + 59_900;
        let just_after = 12 * 60 * 60 * 1000 + 60_050;

        record_sample(&mut buckets, sample("api:v1", just_before, 10));
        record_sample(&mut buckets, sample("api:v1", just_after, 20));

        assert_eq!(buckets.len(), 2);
        assert!(buckets.contains_key(&("api:v1".to_string(), floor_to_minute(just_before))));
        assert!(buckets.contains_key(&("api:v1".to_string(), floor_to_minute(just_after))));
    }

    #[test]
    fn test_take_completed_buckets_flushes_only_older_minutes() {
        let mut buckets = HashMap::new();
        let previous_minute = 12 * 60 * 60;
        let current_minute = previous_minute + 60;

        buckets.insert(
            ("api:v1".to_string(), previous_minute),
            InProgressBucket::new("api:v1", previous_minute),
        );
        buckets.insert(
            ("api:v1".to_string(), current_minute),
            InProgressBucket::new("api:v1", current_minute),
        );

        let completed = take_completed_buckets(&mut buckets, current_minute);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].minute_ts, previous_minute);
        assert_eq!(buckets.len(), 1);
        assert!(buckets.contains_key(&("api:v1".to_string(), current_minute)));
    }

    #[test]
    fn test_minute_boundary_samples_finalize_into_distinct_buckets() {
        let mut buckets = HashMap::new();
        let just_before = 12 * 60 * 60 * 1000 + 59_900;
        let just_after = 12 * 60 * 60 * 1000 + 60_050;

        record_sample(&mut buckets, sample("api:v1", just_before, 10));
        record_sample(&mut buckets, sample("api:v1", just_after, 200));

        let completed = take_completed_buckets(&mut buckets, floor_to_minute(just_after));
        assert_eq!(completed.len(), 1);
        let finalized = completed.into_iter().next().unwrap().finalize();
        assert_eq!(finalized.minute_ts, floor_to_minute(just_before));
        assert_eq!(finalized.request_count, 1);
        assert_eq!(finalized.latency_p50_ms, 10.0);
        assert_eq!(finalized.latency_p99_ms, 10.0);

        let in_progress = buckets
            .get(&("api:v1".to_string(), floor_to_minute(just_after)))
            .unwrap();
        assert_eq!(in_progress.count, 1);
        assert_eq!(in_progress.latency_samples, vec![200]);
    }
}
