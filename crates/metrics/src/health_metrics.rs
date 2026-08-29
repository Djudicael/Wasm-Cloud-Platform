// crates/metrics/src/health_metrics.rs
use prometheus::{IntGauge, IntGaugeVec, Opts, Registry};
use std::{collections::HashSet, sync::Mutex};

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

    /// Available filesystem inodes.
    pub disk_free_inodes: IntGauge,

    /// Process memory usage in MB.
    pub memory_used_mb: IntGauge,

    /// Effective memory limit in MB.
    pub memory_limit_mb: IntGauge,

    /// Process memory use as an integer percentage of the effective limit.
    pub memory_usage_percent: IntGauge,

    /// Per-app healthy instance count (labeled by app_id).
    pub app_healthy_instances: IntGaugeVec,

    /// Per-app total instance count (labeled by app_id).
    pub app_total_instances: IntGaugeVec,

    /// App labels exported by the previous report, used to delete stale series.
    previous_app_ids: Mutex<HashSet<String>>,
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

        let disk_free_inodes =
            IntGauge::new("wasm_node_disk_free_inodes", "Available filesystem inodes").unwrap();
        registry
            .register(Box::new(disk_free_inodes.clone()))
            .unwrap_or(());

        let memory_used_mb =
            IntGauge::new("wasm_node_memory_used_mb", "Process memory usage in MB").unwrap();
        registry
            .register(Box::new(memory_used_mb.clone()))
            .unwrap_or(());

        let memory_limit_mb = IntGauge::new(
            "wasm_node_memory_limit_mb",
            "Effective process memory limit in MB",
        )
        .unwrap();
        registry
            .register(Box::new(memory_limit_mb.clone()))
            .unwrap_or(());

        let memory_usage_percent = IntGauge::new(
            "wasm_node_memory_usage_percent",
            "Process memory use as a percentage of the effective limit",
        )
        .unwrap();
        registry
            .register(Box::new(memory_usage_percent.clone()))
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
            disk_free_inodes,
            memory_used_mb,
            memory_limit_mb,
            memory_usage_percent,
            app_healthy_instances,
            app_total_instances,
            previous_app_ids: Mutex::new(HashSet::new()),
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
                    let words: Vec<_> = dep.message.split_whitespace().collect();
                    if let Some(inodes) = words.windows(2).find_map(|window| {
                        (window[1] == "inodes")
                            .then(|| window[0].parse::<i64>().ok())
                            .flatten()
                    }) {
                        self.disk_free_inodes.set(inodes);
                    }
                }
                "memory" => {
                    // Parse "XXX MB / YYY MB" from the message
                    let mut parts = dep.message.split('/');
                    if let Some(part) = parts.next() {
                        if let Some(used) = part
                            .trim()
                            .strip_suffix(" MB")
                            .and_then(|s| s.parse::<i64>().ok())
                        {
                            self.memory_used_mb.set(used);
                            if let Some(limit) = parts
                                .next()
                                .and_then(|value| value.split_whitespace().next())
                                .and_then(|value| value.parse::<i64>().ok())
                            {
                                self.memory_limit_mb.set(limit);
                                if limit > 0 {
                                    self.memory_usage_percent.set(used * 100 / limit);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Remove labels for apps absent from the current report. Leaving a zero
        // gauge behind would make an undeployed application alert forever.
        let current_app_ids: HashSet<_> =
            report.apps.iter().map(|app| app.app_id.clone()).collect();
        let mut previous_app_ids = self
            .previous_app_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for removed in previous_app_ids.difference(&current_app_ids) {
            let _ = self.app_healthy_instances.remove_label_values(&[removed]);
            let _ = self.app_total_instances.remove_label_values(&[removed]);
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
        *previous_app_ids = current_app_ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::health::{AppHealthSummary, NodeHealthReport, NodeHealthStatus};
    use prometheus::core::Collector;

    fn report(apps: Vec<AppHealthSummary>) -> NodeHealthReport {
        NodeHealthReport {
            status: NodeHealthStatus::Healthy,
            node_id: "node-0".to_string(),
            timestamp: "2026-08-27T00:00:00Z".to_string(),
            uptime_secs: 1,
            startup_complete: true,
            accepting_requests: true,
            active_instances: apps.iter().map(|app| app.instances).sum(),
            deployed_apps: apps.len() as u32,
            dependencies: Vec::new(),
            apps,
        }
    }

    #[test]
    fn removed_apps_delete_their_prometheus_series() {
        let registry = Registry::new();
        let metrics = HealthMetrics::new(&registry);
        metrics.update_from_report(&report(vec![AppHealthSummary {
            app_id: "default/lifecycle:v1".to_string(),
            instances: 1,
            healthy_instances: 1,
            serving: true,
        }]));
        assert_eq!(
            metrics.app_healthy_instances.collect()[0]
                .get_metric()
                .len(),
            1
        );

        metrics.update_from_report(&report(Vec::new()));
        assert!(metrics
            .app_healthy_instances
            .collect()
            .iter()
            .all(|family| family.get_metric().is_empty()));
        assert!(metrics
            .app_total_instances
            .collect()
            .iter()
            .all(|family| family.get_metric().is_empty()));
    }
}
