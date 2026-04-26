// crates/metrics/src/health_metrics.rs
use prometheus::{IntGauge, IntGaugeVec, Opts, Registry};

/// Prometheus metrics for the health check subsystem.
pub struct HealthMetrics {
    /// Current node health status: 0=unhealthy, 1=degraded, 2=healthy.
    pub node_health_status: IntGauge,

    /// Number of active Wasm instances.
    pub active_instances: IntGauge,

    /// Number of deployed apps.
    pub deployed_apps: IntGauge,

    /// Whether NATS is connected: 0=disconnected, 1=connected.
    pub nats_connected: IntGauge,

    /// Whether the node is accepting requests: 0=rejecting, 1=accepting.
    pub accepting_requests: IntGauge,

    /// Available disk space in MB.
    pub disk_free_mb: IntGauge,

    /// Process memory usage in MB.
    pub memory_used_mb: IntGauge,

    /// Per-app healthy instance count (labeled by app_id).
    pub app_healthy_instances: IntGaugeVec,

    /// Per-app total instance count (labeled by app_id).
    pub app_total_instances: IntGaugeVec,
}

impl HealthMetrics {
    pub fn new(registry: &Registry) -> Self {
        let node_health_status = IntGauge::new(
            "wasm_node_health_status",
            "Node health status: 0=unhealthy, 1=degraded, 2=healthy",
        )
        .unwrap();
        registry
            .register(Box::new(node_health_status.clone()))
            .unwrap_or(());

        let active_instances = IntGauge::new(
            "wasm_node_active_instances",
            "Number of active Wasm instances",
        )
        .unwrap();
        registry
            .register(Box::new(active_instances.clone()))
            .unwrap_or(());

        let deployed_apps =
            IntGauge::new("wasm_node_deployed_apps", "Number of deployed applications").unwrap();
        registry
            .register(Box::new(deployed_apps.clone()))
            .unwrap_or(());

        let nats_connected = IntGauge::new(
            "wasm_node_nats_connected",
            "NATS connection status: 0=disconnected, 1=connected",
        )
        .unwrap();
        registry
            .register(Box::new(nats_connected.clone()))
            .unwrap_or(());

        let accepting_requests = IntGauge::new(
            "wasm_node_accepting_requests",
            "Whether the node is accepting requests: 0=rejecting, 1=accepting",
        )
        .unwrap();
        registry
            .register(Box::new(accepting_requests.clone()))
            .unwrap_or(());

        let disk_free_mb =
            IntGauge::new("wasm_node_disk_free_mb", "Available disk space in MB").unwrap();
        registry
            .register(Box::new(disk_free_mb.clone()))
            .unwrap_or(());

        let memory_used_mb =
            IntGauge::new("wasm_node_memory_used_mb", "Process memory usage in MB").unwrap();
        registry
            .register(Box::new(memory_used_mb.clone()))
            .unwrap_or(());

        let app_healthy_instances = IntGaugeVec::new(
            Opts::new(
                "wasm_node_app_healthy_instances",
                "Number of healthy instances per app",
            ),
            &["app"],
        )
        .unwrap();
        registry
            .register(Box::new(app_healthy_instances.clone()))
            .unwrap_or(());

        let app_total_instances = IntGaugeVec::new(
            Opts::new(
                "wasm_node_app_total_instances",
                "Total number of instances per app",
            ),
            &["app"],
        )
        .unwrap();
        registry
            .register(Box::new(app_total_instances.clone()))
            .unwrap_or(());

        HealthMetrics {
            node_health_status,
            active_instances,
            deployed_apps,
            nats_connected,
            accepting_requests,
            disk_free_mb,
            memory_used_mb,
            app_healthy_instances,
            app_total_instances,
        }
    }

    /// Update all metrics from a health report.
    pub fn update_from_report(&self, report: &common::health::NodeHealthReport) {
        self.node_health_status.set(match report.status {
            common::health::NodeHealthStatus::Healthy => 2,
            common::health::NodeHealthStatus::Degraded => 1,
            common::health::NodeHealthStatus::Unhealthy => 0,
        });
        self.active_instances.set(report.active_instances as i64);
        self.deployed_apps.set(report.deployed_apps as i64);
        self.accepting_requests
            .set(if report.accepting_requests { 1 } else { 0 });

        for dep in &report.dependencies {
            match dep.name.as_str() {
                "nats" => self.nats_connected.set(
                    if dep.status == common::health::DependencyStatus::Healthy {
                        1
                    } else {
                        0
                    },
                ),
                "disk" => {
                    // Extract the first number from the disk message.
                    // Messages vary: "51234 MB free", "low disk space: 800 MB free", "only 12 MB free"
                    if let Some(mb) = dep
                        .message
                        .split_whitespace()
                        .find_map(|word| word.parse::<i64>().ok())
                    {
                        self.disk_free_mb.set(mb);
                    }
                }
                "memory" => {
                    // Parse "XXX MB / YYY MB" from the message
                    if let Some(part) = dep.message.split('/').next() {
                        if let Some(val) = part
                            .trim()
                            .strip_suffix(" MB")
                            .and_then(|s| s.parse::<i64>().ok())
                        {
                            self.memory_used_mb.set(val);
                        }
                    }
                }
                _ => {}
            }
        }

        // Per-app metrics: iterate all apps and set gauge values with the app label.
        for app in &report.apps {
            self.app_healthy_instances
                .with_label_values(&[&app.app_id])
                .set(app.healthy_instances as i64);
            self.app_total_instances
                .with_label_values(&[&app.app_id])
                .set(app.instances as i64);
        }
    }
}
