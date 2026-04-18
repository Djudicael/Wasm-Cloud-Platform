// crates/proxy/src/metrics.rs
use prometheus::{IntCounterVec, Opts, Registry};

pub struct RateLimitMetrics {
    /// Counter of rejected requests, labeled by app and reason.
    pub rejected_total: IntCounterVec,
}

impl RateLimitMetrics {
    pub fn new(registry: &Registry) -> Self {
        let rejected_total = IntCounterVec::new(
            Opts::new(
                "proxy_rate_limit_rejected_total",
                "Total requests rejected by rate limiting",
            ),
            &["app", "reason"], // reason: "app_limit" | "ip_limit" | "backpressure"
        )
        .expect("Failed to create rejected_total metric");

        registry
            .register(Box::new(rejected_total.clone()))
            .expect("Failed to register rejected_total metric");

        RateLimitMetrics { rejected_total }
    }

    pub fn record_rejection(&self, app_id: &str, reason: &str) {
        self.rejected_total
            .with_label_values(&[app_id, reason])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_metrics() {
        let registry = Registry::new();
        let metrics = RateLimitMetrics::new(&registry);

        // Record some rejections
        metrics.record_rejection("test-app", "app_limit");
        metrics.record_rejection("test-app", "ip_limit");
        metrics.record_rejection("other-app", "backpressure");

        // Verify metrics were recorded
        let metric_families = registry.gather();
        let rate_limit_family = metric_families
            .iter()
            .find(|mf| mf.name() == "proxy_rate_limit_rejected_total")
            .expect("Metric not found");

        assert_eq!(rate_limit_family.get_metric().len(), 3);
    }
}
