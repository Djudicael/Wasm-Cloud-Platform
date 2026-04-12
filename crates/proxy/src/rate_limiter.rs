// crates/proxy/src/rate_limiter.rs
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Per-app rate limit configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum sustained requests per second per node for this app.
    pub requests_per_second: u32,

    /// Burst capacity — absorbs short spikes above the sustained rate.
    /// When the bucket is full, this many extra requests can be served instantly.
    pub burst_capacity: u32,

    /// Maximum requests per second from a single IP address.
    /// Applies independently of the app-level limit.
    pub per_ip_limit: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig {
            requests_per_second: 1_000,
            burst_capacity: 50,
            per_ip_limit: 100,
        }
    }
}

/// Token bucket state for a single rate limit counter.
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate_per_second: u32, burst: u32) -> Self {
        TokenBucket {
            tokens: burst as f64,
            max_tokens: burst as f64,
            refill_rate: rate_per_second as f64,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns true if allowed.
    fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        // Refill tokens based on elapsed time
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Node-local rate limiter. No external dependencies.
pub struct RateLimiter {
    /// Per-app token buckets (app_id → bucket).
    app_buckets: RwLock<HashMap<String, TokenBucket>>,

    /// Per-IP token buckets (ip → bucket).
    /// Pruned periodically to prevent unbounded growth.
    ip_buckets: RwLock<HashMap<IpAddr, TokenBucket>>,

    /// Per-app rate limit configs.
    configs: RwLock<HashMap<String, RateLimitConfig>>,

    /// Default config for apps without explicit limits.
    default_config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(default_config: RateLimitConfig) -> Self {
        RateLimiter {
            app_buckets: RwLock::new(HashMap::new()),
            ip_buckets: RwLock::new(HashMap::new()),
            configs: RwLock::new(HashMap::new()),
            default_config,
        }
    }

    /// Set a custom rate limit for a specific app.
    pub async fn set_app_config(&self, app_id: &str, config: RateLimitConfig) {
        self.configs
            .write()
            .await
            .insert(app_id.to_string(), config);
    }

    /// Get the current config for an app (for introspection/CLI).
    pub async fn get_app_config(&self, app_id: &str) -> RateLimitConfig {
        let configs = self.configs.read().await;
        configs
            .get(app_id)
            .cloned()
            .unwrap_or(self.default_config.clone())
    }

    /// Check whether a request should be allowed.
    /// Returns Ok(()) if allowed, Err(RateLimitDenied) with reason if rejected.
    pub async fn check_request(
        &self,
        app_id: &str,
        source_ip: IpAddr,
    ) -> Result<(), RateLimitDenied> {
        let config = {
            let configs = self.configs.read().await;
            configs
                .get(app_id)
                .cloned()
                .unwrap_or(self.default_config.clone())
        };

        // 1. Check per-app limit
        {
            let mut buckets = self.app_buckets.write().await;
            let bucket = buckets.entry(app_id.to_string()).or_insert_with(|| {
                TokenBucket::new(config.requests_per_second, config.burst_capacity)
            });
            if !bucket.try_acquire() {
                return Err(RateLimitDenied::AppLimitExceeded {
                    app_id: app_id.to_string(),
                    limit: config.requests_per_second,
                });
            }
        }

        // 2. Check per-IP limit
        {
            let mut buckets = self.ip_buckets.write().await;
            let bucket = buckets
                .entry(source_ip)
                .or_insert_with(|| TokenBucket::new(config.per_ip_limit, config.per_ip_limit));
            if !bucket.try_acquire() {
                return Err(RateLimitDenied::IpLimitExceeded {
                    ip: source_ip,
                    limit: config.per_ip_limit,
                });
            }
        }

        Ok(())
    }

    /// Start a background task that prunes stale IP buckets.
    pub fn start_prune_loop(self: Arc<Self>) {
        let limiter = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                limiter
                    .prune_stale_ip_buckets(Duration::from_secs(300))
                    .await;
            }
        });
    }

    async fn prune_stale_ip_buckets(&self, max_age: Duration) {
        let now = Instant::now();
        let mut buckets = self.ip_buckets.write().await;
        let before = buckets.len();
        buckets.retain(|_, bucket| now.duration_since(bucket.last_refill) < max_age);
        let pruned = before - buckets.len();
        if pruned > 0 {
            tracing::debug!(
                pruned,
                remaining = buckets.len(),
                "pruned stale IP rate limit buckets"
            );
        }
    }
}

#[derive(Debug)]
pub enum RateLimitDenied {
    AppLimitExceeded { app_id: String, limit: u32 },
    IpLimitExceeded { ip: IpAddr, limit: u32 },
}

impl std::fmt::Display for RateLimitDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RateLimitDenied::AppLimitExceeded { app_id, limit } => {
                write!(f, "app '{}' rate limit exceeded ({}/s)", app_id, limit)
            }
            RateLimitDenied::IpLimitExceeded { ip, limit } => {
                write!(f, "IP {} rate limit exceeded ({}/s)", ip, limit)
            }
        }
    }
}

impl std::error::Error for RateLimitDenied {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn test_app_rate_limit() {
        let config = RateLimitConfig {
            requests_per_second: 10,
            burst_capacity: 5,
            per_ip_limit: 100,
        };
        let limiter = RateLimiter::new(config.clone());
        limiter.set_app_config("test-app", config).await;

        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // First 5 requests (burst) should succeed
        for _ in 0..5 {
            assert!(limiter.check_request("test-app", ip).await.is_ok());
        }

        // 6th should fail (burst exhausted, refill rate not enough)
        assert!(limiter.check_request("test-app", ip).await.is_err());
    }

    #[tokio::test]
    async fn test_per_ip_limit() {
        let config = RateLimitConfig {
            requests_per_second: 1000,
            burst_capacity: 100,
            per_ip_limit: 5,
        };
        let limiter = RateLimiter::new(config.clone());
        limiter.set_app_config("test-app", config).await;

        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));

        // First 5 should succeed (per-IP burst)
        for _ in 0..5 {
            assert!(limiter.check_request("test-app", ip).await.is_ok());
        }

        // 6th should fail due to IP limit
        let result = limiter.check_request("test-app", ip).await;
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(RateLimitDenied::IpLimitExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn test_independent_ip_tracking() {
        let config = RateLimitConfig {
            requests_per_second: 1000,
            burst_capacity: 100,
            per_ip_limit: 2,
        };
        let limiter = RateLimiter::new(config);

        let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        // Exhaust ip1
        limiter.check_request("test-app", ip1).await.unwrap();
        limiter.check_request("test-app", ip1).await.unwrap();
        assert!(limiter.check_request("test-app", ip1).await.is_err());

        // ip2 should still work
        assert!(limiter.check_request("test-app", ip2).await.is_ok());
        assert!(limiter.check_request("test-app", ip2).await.is_ok());
    }

    #[tokio::test]
    async fn test_token_refill() {
        let config = RateLimitConfig {
            requests_per_second: 10,
            burst_capacity: 1,
            per_ip_limit: 100,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // Consume the burst
        assert!(limiter.check_request("test-app", ip).await.is_ok());
        assert!(limiter.check_request("test-app", ip).await.is_err());

        // Wait for refill (10 req/s = 1 token every 100ms)
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should allow one more
        assert!(limiter.check_request("test-app", ip).await.is_ok());
    }
}
