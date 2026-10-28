//! Policy counter export and gauge refresh helpers for supervisor-managed
//! instances.

use crate::{instance::ManagedInstance, pool::InstancePool, Supervisor};
use std::collections::HashMap;

impl Supervisor {
    fn update_instance_network_metrics(
        metrics: &metrics::exporter::PolicyMetrics,
        instance: &ManagedInstance,
        snapshot: &crate::instance::PolicyCounterSnapshot,
    ) {
        let id = instance.id.0.to_string();
        let app = instance.app_id.0.as_str();
        metrics
            .instance_egress_bytes
            .with_label_values(&[app, &id])
            .set(snapshot.egress_bytes.min(i64::MAX as u64) as i64);
        metrics
            .instance_outbound_connections
            .with_label_values(&[app, &id])
            .set(i64::from(snapshot.active_outbound_connections));
        for (resource, value) in [
            ("egress", snapshot.egress_denied_total),
            ("connections", snapshot.connection_denied_total),
        ] {
            metrics
                .instance_network_denials
                .with_label_values(&[app, &id, resource])
                .set(value.min(i64::MAX as u64) as i64);
        }
        for (operation, value) in ["connect", "read", "write", "http"]
            .into_iter()
            .zip(snapshot.io_timeouts)
        {
            metrics
                .instance_io_timeouts
                .with_label_values(&[app, &id, operation])
                .set(value.min(i64::MAX as u64) as i64);
        }
    }
    pub(crate) fn export_policy_metrics(
        policy_metrics: &metrics::exporter::PolicyMetrics,
        instances: &mut [ManagedInstance],
    ) -> (i64, i64, i64, i64) {
        let mut active_outbound_connections = 0_i64;
        let mut open_fds = 0_i64;
        let mut current_memory_bytes = 0_i64;
        let mut current_table_elements = 0_i64;

        for instance in instances {
            let Some(counters) = instance.policy_counters.as_ref() else {
                continue;
            };

            let snapshot = crate::instance::PolicyCounterSnapshot::from_counters(counters);
            let last = instance.last_policy_export;
            Self::update_instance_network_metrics(policy_metrics, instance, &snapshot);
            for (index, operation) in ["connect", "read", "write", "http"].into_iter().enumerate() {
                policy_metrics
                    .io_timeouts_total
                    .with_label_values(&[operation])
                    .inc_by(snapshot.io_timeouts[index].saturating_sub(last.io_timeouts[index]));
            }

            policy_metrics.connection_denied_total.inc_by(
                snapshot
                    .connection_denied_total
                    .saturating_sub(last.connection_denied_total),
            );
            policy_metrics.egress_denied_total.inc_by(
                snapshot
                    .egress_denied_total
                    .saturating_sub(last.egress_denied_total),
            );
            policy_metrics.fd_denied_total.inc_by(
                snapshot
                    .fd_denied_total
                    .saturating_sub(last.fd_denied_total),
            );
            policy_metrics.fs_write_denied_total.inc_by(
                snapshot
                    .fs_write_denied_total
                    .saturating_sub(last.fs_write_denied_total),
            );
            policy_metrics.bind_denied_total.inc_by(
                snapshot
                    .bind_denied_total
                    .saturating_sub(last.bind_denied_total),
            );
            policy_metrics.dns_denied_total.inc_by(
                snapshot
                    .dns_denied_total
                    .saturating_sub(last.dns_denied_total),
            );
            policy_metrics.memory_growth_denied_total.inc_by(
                snapshot
                    .memory_growth_denied_total
                    .saturating_sub(last.memory_growth_denied_total),
            );
            policy_metrics.table_growth_denied_total.inc_by(
                snapshot
                    .table_growth_denied_total
                    .saturating_sub(last.table_growth_denied_total),
            );

            active_outbound_connections += i64::from(snapshot.active_outbound_connections);
            open_fds += i64::from(snapshot.open_fds);
            current_memory_bytes += snapshot.current_memory_bytes as i64;
            current_table_elements += i64::from(snapshot.current_table_elements);
            instance.last_policy_export = snapshot;
        }

        policy_metrics
            .active_outbound_connections
            .set(active_outbound_connections);
        policy_metrics.open_fds.set(open_fds);
        policy_metrics
            .current_memory_bytes
            .set(current_memory_bytes);
        policy_metrics
            .current_table_elements
            .set(current_table_elements);

        (
            active_outbound_connections,
            open_fds,
            current_memory_bytes,
            current_table_elements,
        )
    }

    pub(crate) fn flush_policy_metrics_for_instance(&self, instance: &mut ManagedInstance) {
        let policy_metrics = self
            .policy_metrics
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().cloned());
        if let Some(policy_metrics) = policy_metrics {
            Self::export_policy_metrics(&policy_metrics, std::slice::from_mut(instance));
            policy_metrics.remove_instance_network(&instance.app_id.0, &instance.id.0.to_string());
        }
    }

    pub(crate) fn export_policy_metrics_from_pools(
        policy_metrics: &metrics::exporter::PolicyMetrics,
        pools: &mut HashMap<String, InstancePool>,
    ) {
        let mut active_outbound_connections = 0_i64;
        let mut open_fds = 0_i64;
        let mut current_memory_bytes = 0_i64;
        let mut current_table_elements = 0_i64;

        for pool in pools.values_mut() {
            let (
                pool_active_outbound,
                pool_open_fds,
                pool_current_memory_bytes,
                pool_current_table_elements,
            ) = Self::export_policy_metrics(policy_metrics, &mut pool.instances);
            active_outbound_connections += pool_active_outbound;
            open_fds += pool_open_fds;
            current_memory_bytes += pool_current_memory_bytes;
            current_table_elements += pool_current_table_elements;
        }

        policy_metrics
            .active_outbound_connections
            .set(active_outbound_connections);
        policy_metrics.open_fds.set(open_fds);
        policy_metrics
            .current_memory_bytes
            .set(current_memory_bytes);
        policy_metrics
            .current_table_elements
            .set(current_table_elements);
    }

    pub(crate) async fn refresh_policy_metric_gauges(&self) {
        let policy_metrics = self
            .policy_metrics
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().cloned());
        let Some(policy_metrics) = policy_metrics else {
            return;
        };

        let pools = self.pools.read().await;
        let mut active_outbound_connections = 0_i64;
        let mut open_fds = 0_i64;
        let mut current_memory_bytes = 0_i64;
        let mut current_table_elements = 0_i64;
        for pool in pools.values() {
            for instance in &pool.instances {
                let Some(counters) = instance.policy_counters.as_ref() else {
                    continue;
                };
                let snapshot = crate::instance::PolicyCounterSnapshot::from_counters(counters);
                Self::update_instance_network_metrics(&policy_metrics, instance, &snapshot);
                active_outbound_connections += i64::from(snapshot.active_outbound_connections);
                open_fds += i64::from(snapshot.open_fds);
                current_memory_bytes += snapshot.current_memory_bytes as i64;
                current_table_elements += i64::from(snapshot.current_table_elements);
            }
        }

        policy_metrics
            .active_outbound_connections
            .set(active_outbound_connections);
        policy_metrics.open_fds.set(open_fds);
        policy_metrics
            .current_memory_bytes
            .set(current_memory_bytes);
        policy_metrics
            .current_table_elements
            .set(current_table_elements);
    }
}
