use prometheus::{IntCounter, Opts, Registry};

/// Prometheus metrics for admin API authentication events.
pub struct AuthMetrics {
    /// Total successful authentications.
    pub auth_successes_total: IntCounter,

    /// Total failed authentications (bad token, missing token).
    pub auth_failures_total: IntCounter,

    /// Total requests rate-limited on the admin API.
    pub rate_limited_total: IntCounter,
}

impl AuthMetrics {
    /// Create and register auth metrics with the given Prometheus registry.
    pub fn new(registry: &Registry) -> Self {
        let auth_successes_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_successes_total",
            "Successful admin API authentications",
        ))
        .expect("failed to create auth_successes_total counter");
        registry
            .register(Box::new(auth_successes_total.clone()))
            .expect("failed to register auth_successes_total counter");

        let auth_failures_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_failures_total",
            "Failed admin API authentications",
        ))
        .expect("failed to create auth_failures_total counter");
        registry
            .register(Box::new(auth_failures_total.clone()))
            .expect("failed to register auth_failures_total counter");

        let rate_limited_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_rate_limited_total",
            "Admin API requests rejected by rate limiter",
        ))
        .expect("failed to create rate_limited_total counter");
        registry
            .register(Box::new(rate_limited_total.clone()))
            .expect("failed to register rate_limited_total counter");

        AuthMetrics {
            auth_successes_total,
            auth_failures_total,
            rate_limited_total,
        }
    }

    /// Create auth metrics without registering with a registry (for testing).
    pub fn new_unregistered() -> Self {
        let auth_successes_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_successes_total",
            "Successful admin API authentications",
        ))
        .unwrap();

        let auth_failures_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_failures_total",
            "Failed admin API authentications",
        ))
        .unwrap();

        let rate_limited_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_rate_limited_total",
            "Admin API requests rejected by rate limiter",
        ))
        .unwrap();

        AuthMetrics {
            auth_successes_total,
            auth_failures_total,
            rate_limited_total,
        }
    }
}
