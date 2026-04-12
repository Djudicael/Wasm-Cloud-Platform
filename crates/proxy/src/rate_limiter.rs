use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

struct Bucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl Bucket {
    fn consume(&mut self, tokens: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= tokens {
            self.tokens -= tokens;
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    requests_per_second: f64,
    burst_size: f64,
}

impl RateLimiter {
    pub fn new(rps: f64, burst: f64) -> Self {
        RateLimiter {
            buckets: Default::default(),
            requests_per_second: rps,
            burst_size: burst,
        }
    }

    /// Returns true if the request should be allowed through.
    pub async fn allow(&self, app_id: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry(app_id.to_string()).or_insert_with(|| Bucket {
            tokens: self.burst_size,
            max_tokens: self.burst_size,
            refill_rate: self.requests_per_second,
            last_refill: Instant::now(),
        });
        bucket.consume(1.0)
    }
}
