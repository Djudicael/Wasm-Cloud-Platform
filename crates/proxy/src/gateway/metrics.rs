use prometheus::{IntCounter, IntGauge, Opts, Registry};

/// Prometheus metrics for the API gateway.
pub struct GatewayMetrics {
    /// Total requests that passed authentication.
    pub auth_success_total: IntCounter,

    /// Total requests that failed authentication.
    pub auth_failure_total: IntCounter,

    /// Total requests rejected by authorization (wrong roles).
    pub authz_denied_total: IntCounter,

    /// Total CORS preflight requests handled.
    pub cors_preflight_total: IntCounter,

    /// Total requests rejected by distributed rate limiter.
    pub rate_limit_denied_total: IntCounter,

    /// Total requests rejected by circuit breaker.
    pub circuit_breaker_rejected_total: IntCounter,

    /// Currently open circuits.
    pub circuits_open: IntGauge,

    /// JWKS refresh count.
    pub jwks_refresh_total: IntCounter,

    /// JWKS refresh failures.
    pub jwks_refresh_failures: IntCounter,
}

impl GatewayMetrics {
    pub fn new(registry: &Registry) -> Self {
        let auth_success_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_auth_success_total",
            "Requests that passed authentication",
        ))
        .unwrap();
        registry.register(Box::new(auth_success_total.clone())).unwrap();

        let auth_failure_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_auth_failure_total",
            "Requests that failed authentication",
        ))
        .unwrap();
        registry.register(Box::new(auth_failure_total.clone())).unwrap();

        let authz_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_authz_denied_total",
            "Requests denied by authorization (wrong roles)",
        ))
        .unwrap();
        registry.register(Box::new(authz_denied_total.clone())).unwrap();

        let cors_preflight_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_cors_preflight_total",
            "CORS preflight requests handled",
        ))
        .unwrap();
        registry.register(Box::new(cors_preflight_total.clone())).unwrap();

        let rate_limit_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_rate_limit_denied_total",
            "Requests denied by distributed rate limiter",
        ))
        .unwrap();
        registry.register(Box::new(rate_limit_denied_total.clone())).unwrap();

        let circuit_breaker_rejected_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_circuit_breaker_rejected_total",
            "Requests rejected by circuit breaker",
        ))
        .unwrap();
        registry.register(Box::new(circuit_breaker_rejected_total.clone())).unwrap();

        let circuits_open = IntGauge::with_opts(Opts::new(
            "wasm_gateway_circuits_open",
            "Currently open circuit breakers",
        ))
        .unwrap();
        registry.register(Box::new(circuits_open.clone())).unwrap();

        let jwks_refresh_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_jwks_refresh_total",
            "JWKS cache refresh attempts",
        ))
        .unwrap();
        registry.register(Box::new(jwks_refresh_total.clone())).unwrap();

        let jwks_refresh_failures = IntCounter::with_opts(Opts::new(
            "wasm_gateway_jwks_refresh_failures",
            "JWKS cache refresh failures",
        ))
        .unwrap();
        registry.register(Box::new(jwks_refresh_failures.clone())).unwrap();

        GatewayMetrics {
            auth_success_total,
            auth_failure_total,
            authz_denied_total,
            cors_preflight_total,
            rate_limit_denied_total,
            circuit_breaker_rejected_total,
            circuits_open,
            jwks_refresh_total,
            jwks_refresh_failures,
        }
    }
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        let registry = Registry::new();
        Self::new(&registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let registry = Registry::new();
        let metrics = GatewayMetrics::new(&registry);
        metrics.auth_success_total.inc();
        metrics.auth_failure_total.inc_by(2);
        metrics.authz_denied_total.inc();
        metrics.circuits_open.set(3);

        assert_eq!(metrics.auth_success_total.get(), 1);
        assert_eq!(metrics.auth_failure_total.get(), 2);
        assert_eq!(metrics.authz_denied_total.get(), 1);
        assert_eq!(metrics.circuits_open.get(), 3);
    }
}
