//! Command-loop and recovery behavior for the supervisor.
//!
//! This keeps reactive operational flows separate from the core instance
//! lifecycle code in `lib.rs`.

use std::{sync::Arc, time::Duration};

use common::types::{AppId, InstanceId, InstanceState};
use tracing::{info, warn};

use crate::{Supervisor, SupervisorCommand};

pub(crate) fn start_command_loop(supervisor: Arc<Supervisor>) {
    let Ok(mut guard) = supervisor.command_rx.lock() else {
        warn!("supervisor command loop mutex poisoned - command loop not started");
        return;
    };

    let Some(mut rx) = guard.take() else {
        warn!("supervisor command loop already started - ignoring duplicate call");
        return;
    };
    drop(guard);

    tokio::spawn(async move {
        info!("supervisor command loop started");
        while let Some(cmd) = rx.recv().await {
            match cmd {
                SupervisorCommand::KillLargestInstance { reason } => {
                    supervisor.kill_largest_instance(&reason).await;
                }
                SupervisorCommand::PruneIdleInstances {
                    idle_threshold_secs,
                } => {
                    supervisor.prune_idle_instances(idle_threshold_secs).await;
                }
                SupervisorCommand::RemoveAppFromUpstream { app_id } => {
                    let pools = supervisor.pools.read().await;
                    if let Some(pool) = pools.get(&app_id.0) {
                        for addr in pool.ready_addrs() {
                            supervisor.upstream_registry.remove(&app_id, &addr).await;
                        }
                    }
                    info!(app = %app_id.0, "removed app from upstream via command");
                }
                SupervisorCommand::KillInstance {
                    app_id,
                    instance_id,
                    reason,
                } => {
                    info!(
                        app = %app_id.0,
                        instance = %instance_id.0,
                        reason,
                        "killing instance via supervisor command"
                    );
                    if let Err(e) = supervisor.kill_instance(&app_id, &instance_id).await {
                        warn!(
                            app = %app_id.0,
                            instance = %instance_id.0,
                            error = %e,
                            "failed to kill instance via command"
                        );
                    }
                }
            }
        }
        warn!("supervisor command loop exited - no more senders");
    });
}

pub(crate) async fn kill_largest_instance(supervisor: &Supervisor, reason: &str) {
    let mut largest: Option<(AppId, InstanceId, u64)> = None;

    let pools = supervisor.pools.read().await;
    for (app_id_str, pool) in pools.iter() {
        for inst in &pool.instances {
            if matches!(inst.state, InstanceState::Ready { .. }) {
                let ram = inst.billing_info.ram_bytes;
                if largest.is_none() || ram > largest.as_ref().unwrap().2 {
                    largest = Some((AppId(app_id_str.clone()), inst.id.clone(), ram));
                }
            }
        }
    }
    drop(pools);

    match largest {
        Some((app_id, instance_id, ram)) => {
            warn!(
                app = %app_id.0,
                instance = %instance_id.0,
                ram_bytes = ram,
                reason,
                "killing largest instance (memory pressure recovery)"
            );
            if let Err(e) = supervisor.kill_instance(&app_id, &instance_id).await {
                warn!(error = %e, "failed to kill largest instance");
            }
        }
        None => {
            info!(reason, "no instances to kill for memory pressure recovery");
        }
    }
}

pub(crate) async fn prune_idle_instances(supervisor: &Supervisor, idle_threshold_secs: u64) {
    let mut total_pruned = 0usize;

    let app_ids = {
        let pools = supervisor.pools.read().await;
        pools.keys().cloned().collect::<Vec<_>>()
    };

    for app_id_str in app_ids {
        let app_id = AppId(app_id_str.clone());
        let idle_ids = {
            let pools = supervisor.pools.read().await;
            pools
                .get(&app_id_str)
                .map(|p| p.idle_instance_ids(idle_threshold_secs))
                .unwrap_or_default()
        };

        for instance_id in idle_ids {
            info!(
                app = %app_id.0,
                instance = %instance_id.0,
                idle_threshold_secs,
                "pruning idle instance (FD pressure recovery)"
            );
            if let Err(e) = supervisor.kill_instance(&app_id, &instance_id).await {
                warn!(
                    app = %app_id.0,
                    instance = %instance_id.0,
                    error = %e,
                    "failed to prune idle instance"
                );
            } else {
                total_pruned += 1;
            }
        }
    }

    if total_pruned > 0 {
        info!(total_pruned, idle_threshold_secs, "pruned idle instances");
    } else {
        info!(idle_threshold_secs, "no idle instances to prune");
    }
}

pub(crate) async fn shutdown_all(supervisor: &Supervisor, timeout: Duration) {
    tracing::info!("shutting down all instances");

    let app_ids = supervisor.list_app_ids().await;
    for app_id in app_ids {
        if let Err(e) = supervisor.drain_app(&app_id, timeout).await {
            tracing::warn!(app = %app_id.0, error = %e, "drain failed");
        }

        let instance_ids: Vec<InstanceId> = {
            let pools = supervisor.pools.read().await;
            pools
                .get(&app_id.0)
                .map(|p| p.instances.iter().map(|i| i.id.clone()).collect())
                .unwrap_or_default()
        };

        for instance_id in instance_ids {
            if let Err(e) = supervisor
                .kill_instance_gracefully(&app_id, &instance_id, timeout / 3, timeout * 2 / 3)
                .await
            {
                tracing::warn!(
                    app = %app_id.0,
                    instance = %instance_id.0,
                    error = %e,
                    "graceful shutdown failed"
                );
            }
        }
    }

    tracing::info!("all instances shutdown complete");
}
