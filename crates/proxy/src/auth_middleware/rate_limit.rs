use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-IP rate limiter for the admin API.
///
/// Uses a simple token bucket algorithm. Each IP gets its own bucket
/// with a configurable refill rate and burst capacity.
///
/// Default: 10 requests/second per IP, burst of 20.
/// This makes brute-force token guessing infeasible (16^64 possible values
/// at 10 guesses/second ~= 10^57 years).
pub struct AdminRateLimiter {
    /// Per-IP token buckets.
    buckets: Mutex<HashMap<IpAddr, TokenBucket>>,

    /// Tokens added per second.
    refill_rate: f64,

    /// Maximum tokens (burst capacity).
    max_tokens: f64,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl AdminRateLimiter {
    /// Create a new rate limiter with the given rate and burst.
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        AdminRateLimiter {
            buckets: Mutex::new(HashMap::new()),
            refill_rate: rate_per_second as f64,
            max_tokens: burst as f64,
        }
    }

    /// Create a rate limiter that always allows requests (disabled).
    pub fn disabled() -> Self {
        AdminRateLimiter {
            buckets: Mutex::new(HashMap::new()),
            refill_rate: 1_000_000.0,
            max_tokens: 1_000_000.0,
        }
    }

    /// Check if a request from the given IP is allowed.
    ///
    /// Returns `true` if allowed, `false` if rate-limited.
    /// Requests with no IP information are allowed (conservative: don't block
    /// unknown sources), but a debug log is emitted.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        let ip = match ip {
            Some(ip) => ip,
            None => {
                tracing::debug!("admin API request with no client IP - skipping rate limit");
                return true;
            }
        };

        if self.max_tokens >= 1_000_000.0 {
            return true;
        }

        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.entry(ip).or_insert_with(|| TokenBucket {
            tokens: self.max_tokens,
            last_refill: Instant::now(),
        });

        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.last_refill = now;
        bucket.tokens = (bucket.tokens + elapsed * self.refill_rate).min(self.max_tokens);

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Prune stale IP buckets that haven't been used recently.
    ///
    /// Call this periodically (e.g., every 60 seconds) to prevent
    /// memory leaks from long-running nodes with many unique IPs.
    pub fn prune_stale(&self, max_age: Duration) {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        let before = buckets.len();
        buckets.retain(|_, bucket| now.duration_since(bucket.last_refill) < max_age);
        let after = buckets.len();
        if before != after {
            tracing::debug!(
                pruned = before - after,
                remaining = after,
                "pruned stale rate limit buckets"
            );
        }
    }
}
