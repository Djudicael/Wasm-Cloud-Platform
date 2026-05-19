use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Distributed rate limiter using NATS KV for cross-node coordination.
pub struct DistributedRateLimiter {
    /// Local token bucket for the hot path.
    local: Arc<tokio::sync::Mutex<LocalBucket>>,

    /// NATS KV store for cluster-wide counter sync.
    kv: Arc<RwLock<Option<async_nats::jetstream::kv::Store>>>,

    /// This node's ID (used as the KV key suffix).
    node_id: String,

    /// The app this limiter is for.
    app_id: String,

    /// Configuration.
    config: DistributedRateLimitConfig,
}

#[derive(Debug, Clone)]
pub struct DistributedRateLimitConfig {
    /// Global requests per second across all nodes.
    pub global_rps: u32,

    /// Burst capacity per node.
    pub per_node_burst: u32,

    /// How often to sync with NATS KV (milliseconds).
    pub sync_interval_ms: u64,

    /// NATS KV bucket name.
    pub kv_bucket: String,
}

struct LocalBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
    consumed_since_sync: u64,
}

impl DistributedRateLimiter {
    pub fn new(app_id: String, node_id: String, config: DistributedRateLimitConfig) -> Self {
        let initial_refill_rate = config.global_rps as f64 / 3.0;
        DistributedRateLimiter {
            local: Arc::new(tokio::sync::Mutex::new(LocalBucket {
                tokens: config.per_node_burst as f64,
                max_tokens: config.per_node_burst as f64,
                refill_rate: initial_refill_rate,
                last_refill: Instant::now(),
                consumed_since_sync: 0,
            })),
            kv: Arc::new(RwLock::new(None)),
            node_id,
            app_id,
            config,
        }
    }

    pub async fn set_kv_store(&self, store: async_nats::jetstream::kv::Store) {
        *self.kv.write().await = Some(store);
    }

    pub async fn check_request(&self) -> bool {
        let mut bucket = self.local.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.last_refill = now;

        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_rate).min(bucket.max_tokens);

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            bucket.consumed_since_sync += 1;
            true
        } else {
            false
        }
    }

    pub fn start_sync_loop(self: Arc<Self>) {
        let limiter = self.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(limiter.config.sync_interval_ms));
            loop {
                interval.tick().await;
                if let Err(e) = limiter.sync_with_cluster().await {
                    tracing::warn!(
                        app = %limiter.app_id,
                        error = %e,
                        "distributed rate limit sync failed"
                    );
                }
            }
        });
    }

    async fn sync_with_cluster(&self) -> Result<(), String> {
        let kv = self.kv.read().await;
        let kv = kv.as_ref().ok_or("NATS KV not initialized")?;

        let key = format!("ratelimit:{}:{}", self.app_id, self.node_id);
        let bucket = self.local.lock().await;
        let consumed = bucket.consumed_since_sync;
        drop(bucket);

        let payload = serde_json::to_string(&RateLimitEntry {
            node_id: self.node_id.clone(),
            consumed,
            timestamp: chrono::Utc::now().timestamp_millis(),
        })
        .map_err(|e| format!("serialize: {e}"))?;

        kv.put(key, payload.into())
            .await
            .map_err(|e| format!("kv put: {e}"))?;

        let prefix = format!("ratelimit:{}:", self.app_id);
        let mut keys = Vec::new();
        let mut key_stream = kv.keys().await.map_err(|e| format!("kv keys: {e}"))?;
        while let Some(key_result) = key_stream.next().await {
            match key_result {
                Ok(key) => {
                    if key.starts_with(&prefix) {
                        keys.push(key);
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "failed to get key from stream");
                }
            }
        }

        let mut total_consumed: u64 = 0;
        let mut node_count: u64 = 0;
        let now = chrono::Utc::now().timestamp_millis();
        let stale_threshold = (self.config.sync_interval_ms as i64) * 5;

        for key in keys {
            match kv.get(&key).await {
                Ok(Some(entry)) => {
                    if let Ok(entry) = serde_json::from_slice::<RateLimitEntry>(&entry) {
                        if now - entry.timestamp < stale_threshold {
                            total_consumed += entry.consumed;
                            node_count += 1;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, key = %key, "failed to read KV entry");
                }
            }
        }

        node_count = node_count.max(1);
        let fair_share_rps = self.config.global_rps as f64 / node_count as f64;

        let mut bucket = self.local.lock().await;
        bucket.refill_rate = fair_share_rps;
        bucket.consumed_since_sync = 0;

        tracing::debug!(
            app = %self.app_id,
            nodes = node_count,
            total_consumed,
            fair_share_rps,
            "distributed rate limit sync"
        );

        Ok(())
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RateLimitEntry {
    pub node_id: String,
    pub consumed: u64,
    pub timestamp: i64,
}

impl RateLimitEntry {
    pub fn new(node_id: String, consumed: u64, timestamp: i64) -> Self {
        RateLimitEntry {
            node_id,
            consumed,
            timestamp,
        }
    }
}
