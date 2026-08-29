// crates/proxy/src/rate_limiter.rs
use dashmap::DashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-app success tracking for adaptive rate limiting and metrics.
struct SuccessCounter {
    total: AtomicU64,
    last_minute: AtomicU64,
    last_reset: std::sync::Mutex<Instant>,
}

impl SuccessCounter {
    fn new() -> Self {
        SuccessCounter {
            total: AtomicU64::new(0),
            last_minute: AtomicU64::new(0),
            last_reset: std::sync::Mutex::new(Instant::now()),
        }
    }

    fn record(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);

        // Reset per-minute counter if a minute has passed
        let mut last_reset = self.last_reset.lock().unwrap();
        if last_reset.elapsed() >= Duration::from_secs(60) {
            self.last_minute.store(0, Ordering::Relaxed);
            *last_reset = Instant::now();
        }
        self.last_minute.fetch_add(1, Ordering::Relaxed);
    }

    fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    fn last_minute(&self) -> u64 {
        self.last_minute.load(Ordering::Relaxed)
    }
}

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
    app_buckets: DashMap<String, TokenBucket>,

    /// Per-application, per-IP token buckets ((app_id, ip) → bucket).
    /// Pruned periodically to prevent unbounded growth.
    ip_buckets: DashMap<(String, IpAddr), TokenBucket>,

    /// Per-app rate limit configs.
    configs: DashMap<String, RateLimitConfig>,

    /// Default config for apps without explicit limits.
    /// Behind an `RwLock` so it can be updated at runtime via hot-reload
    /// without disrupting ongoing requests.
    default_config: std::sync::RwLock<RateLimitConfig>,
    /// Per-app success counters for metrics and adaptive limiting.
    success_counters: DashMap<String, SuccessCounter>,
}

impl RateLimiter {
    pub fn new(default_config: RateLimitConfig) -> Self {
        RateLimiter {
            app_buckets: DashMap::new(),
            ip_buckets: DashMap::new(),
            configs: DashMap::new(),
            default_config: std::sync::RwLock::new(default_config),
            success_counters: DashMap::new(),
        }
    }

    /// Update the default rate-limit configuration at runtime (hot-reload).
    ///
    /// This is called by the config sync loop when the operator changes
    /// rate-limit parameters via the admin API. The update takes effect
    /// immediately for subsequent requests; existing token buckets keep
    /// their current state.
    pub fn update_default_config(&self, new_config: RateLimitConfig) {
        let mut guard = self.default_config.write().unwrap();
        tracing::info!(
            old_rps = guard.requests_per_second,
            new_rps = new_config.requests_per_second,
            new_burst = new_config.burst_capacity,
            new_per_ip = new_config.per_ip_limit,
            "rate limiter default config updated via hot-reload"
        );
        *guard = new_config;
    }

    /// Read the current default config (used by sync loops and introspection).
    pub fn read_default_config(&self) -> RateLimitConfig {
        self.default_config.read().unwrap().clone()
    }

    /// Set a custom rate limit for a specific app.
    pub fn set_app_config(&self, app_id: &str, config: RateLimitConfig) {
        self.configs.insert(app_id.to_string(), config);
        // An existing bucket was sized from the previous configuration.
        self.app_buckets.remove(app_id);
        self.ip_buckets
            .retain(|(bucket_app, _), _| bucket_app != app_id);
    }

    /// Remove an application override and its bucket so future requests use
    /// the current defaults with a freshly sized bucket.
    pub fn remove_app_config(&self, app_id: &str) {
        self.configs.remove(app_id);
        self.app_buckets.remove(app_id);
        self.ip_buckets
            .retain(|(bucket_app, _), _| bucket_app != app_id);
    }

    /// Get the current config for an app (for introspection/CLI).
    pub fn get_app_config(&self, app_id: &str) -> RateLimitConfig {
        self.configs
            .get(app_id)
            .map(|c| c.clone())
            .unwrap_or_else(|| self.default_config.read().unwrap().clone())
    }

    /// Check whether a request should be allowed.
    /// Returns Ok(()) if allowed, Err(RateLimitDenied) with reason if rejected.
    pub fn check_request(&self, app_id: &str, source_ip: IpAddr) -> Result<(), RateLimitDenied> {
        let config = self
            .configs
            .get(app_id)
            .map(|c| c.clone())
            .unwrap_or_else(|| self.default_config.read().unwrap().clone());

        // 1. Check per-app limit
        {
            let mut bucket = self
                .app_buckets
                .entry(app_id.to_string())
                .or_insert_with(|| {
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
            let mut bucket = self
                .ip_buckets
                .entry((app_id.to_string(), source_ip))
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

    /// Record a successful request for metrics and adaptive rate limiting.
    pub fn record_success(&self, app_id: &str, _source_ip: IpAddr) {
        let counter = self
            .success_counters
            .entry(app_id.to_string())
            .or_insert_with(SuccessCounter::new);
        counter.record();

        tracing::debug!(
            app_id,
            total = counter.total(),
            "Rate limiter recorded success"
        );
    }

    /// Get success statistics for an app.
    pub fn get_success_stats(&self, app_id: &str) -> Option<(u64, u64)> {
        self.success_counters
            .get(app_id)
            .map(|c| (c.total(), c.last_minute()))
    }

    /// Start a background task that prunes stale IP buckets.
    pub fn start_prune_loop(self: Arc<Self>) {
        let limiter = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                limiter.prune_stale_ip_buckets(Duration::from_secs(300));
            }
        });
    }

    fn prune_stale_ip_buckets(&self, max_age: Duration) {
        let now = Instant::now();
        let before = self.ip_buckets.len();
        self.ip_buckets
            .retain(|_, bucket| now.duration_since(bucket.last_refill) < max_age);
        let pruned = before - self.ip_buckets.len();
        if pruned > 0 {
            tracing::debug!(
                pruned,
                remaining = self.ip_buckets.len(),
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
    async fn test_record_success_tracks_counts() {
        let limiter = RateLimiter::new(RateLimitConfig::default());
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        limiter.record_success("test-app", ip);
        limiter.record_success("test-app", ip);
        limiter.record_success("test-app", ip);

        let (total, _last_minute) = limiter.get_success_stats("test-app").unwrap();
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn test_app_rate_limit() {
        let config = RateLimitConfig {
            requests_per_second: 10,
            burst_capacity: 5,
            per_ip_limit: 100,
        };
        let limiter = RateLimiter::new(config.clone());
        limiter.set_app_config("test-app", config);

        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // First 5 requests (burst) should succeed
        for _ in 0..5 {
            assert!(limiter.check_request("test-app", ip).is_ok());
        }

        // 6th should fail (burst exhausted, refill rate not enough)
        assert!(limiter.check_request("test-app", ip).is_err());
    }

    #[tokio::test]
    async fn test_per_ip_limit() {
        let config = RateLimitConfig {
            requests_per_second: 1000,
            burst_capacity: 100,
            per_ip_limit: 5,
        };
        let limiter = RateLimiter::new(config.clone());
        limiter.set_app_config("test-app", config);

        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));

        // First 5 should succeed (per-IP burst)
        for _ in 0..5 {
            assert!(limiter.check_request("test-app", ip).is_ok());
        }

        // 6th should fail due to IP limit
        let result = limiter.check_request("test-app", ip);
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
        limiter.check_request("test-app", ip1).unwrap();
        limiter.check_request("test-app", ip1).unwrap();
        assert!(limiter.check_request("test-app", ip1).is_err());

        // ip2 should still work
        assert!(limiter.check_request("test-app", ip2).is_ok());
        assert!(limiter.check_request("test-app", ip2).is_ok());
    }

    #[tokio::test]
    async fn test_same_ip_is_isolated_between_applications() {
        let config = RateLimitConfig {
            requests_per_second: 1_000,
            burst_capacity: 100,
            per_ip_limit: 2,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

        for _ in 0..2 {
            limiter.check_request("app-a", ip).unwrap();
        }
        assert!(limiter.check_request("app-a", ip).is_err());
        for _ in 0..2 {
            limiter.check_request("app-b", ip).unwrap();
        }
    }

    #[tokio::test]
    async fn test_config_update_replaces_existing_ip_bucket() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_second: 100,
            burst_capacity: 100,
            per_ip_limit: 1,
        });
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        limiter.check_request("test-app", ip).unwrap();
        assert!(limiter.check_request("test-app", ip).is_err());

        limiter.set_app_config(
            "test-app",
            RateLimitConfig {
                requests_per_second: 1_000,
                burst_capacity: 1_000,
                per_ip_limit: 100,
            },
        );
        for _ in 0..100 {
            limiter.check_request("test-app", ip).unwrap();
        }
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
        assert!(limiter.check_request("test-app", ip).is_ok());
        assert!(limiter.check_request("test-app", ip).is_err());

        // Wait for refill (10 req/s = 1 token every 100ms)
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should allow one more
        assert!(limiter.check_request("test-app", ip).is_ok());
    }
}
