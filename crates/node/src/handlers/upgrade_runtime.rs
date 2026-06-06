//! Rolling upgrade and graceful drain behavior.
//!
//! The event dispatcher delegates here so upgrade sequencing and shutdown logic
//! remain separate from ordinary cluster state handling.

use std::sync::Arc;

use messaging::events::Event;
use storage::Store;
use supervisor::Supervisor;
use tracing::{error, info, warn};

pub(crate) struct UpgradeContext<'a> {
    pub supervisor: &'a Arc<Supervisor>,
    pub store: &'a Store,
    pub node_id: &'a str,
    pub cluster_node_stale_after_secs: u64,
    pub upgrade_signing_public_key: Option<&'a str>,
    pub bus: &'a messaging::NatsBus,
}

pub(crate) async fn handle_node_upgrade(ctx: UpgradeContext<'_>, event: Event) {
    use crate::upgrade::{
        download_and_verify, handle_upgrade_event, verify_upgrade_signature, UpgradeAction,
    };

    let mut cluster_nodes: Vec<String> = ctx
        .store
        .list_cluster_nodes()
        .unwrap_or_default()
        .into_iter()
        .filter(|node| !node.is_stale(ctx.cluster_node_stale_after_secs))
        .map(|node| node.node_id)
        .collect();
    if !cluster_nodes.contains(&ctx.node_id.to_string()) {
        cluster_nodes.push(ctx.node_id.to_string());
    }

    if let Err(e) = verify_upgrade_signature(&event, ctx.upgrade_signing_public_key) {
        error!(error = %e, "upgrade signature verification failed");
        return;
    }

    match handle_upgrade_event(&event, ctx.node_id, &cluster_nodes) {
        Ok(UpgradeAction::NotAnUpgradeEvent) => {
            warn!("handle_node_upgrade called with non-upgrade event")
        }
        Ok(UpgradeAction::NotTargeted) => info!("upgrade not targeted at this node"),
        Ok(UpgradeAction::WaitForPredecessor { predecessor }) => {
            info!(
                predecessor,
                "waiting for predecessor node to complete upgrade"
            );
        }
        Ok(UpgradeAction::IncompatibleVersion) => {
            error!("upgrade version is incompatible with this node's protocol version");
        }
        Ok(UpgradeAction::ProceedWithUpgrade) => {
            if let Event::NodeUpgrade {
                binary_url,
                binary_sha256,
                new_protocol_version,
                new_binary_version,
                ..
            } = event
            {
                info!(
                    url = %binary_url,
                    version = %new_binary_version,
                    protocol = new_protocol_version,
                    "proceeding with upgrade"
                );

                let install_dir = std::path::PathBuf::from("/opt/wasm-cloud");
                match download_and_verify(&binary_url, &binary_sha256, &install_dir, "node").await {
                    Ok(new_binary_path) => {
                        info!(path = ?new_binary_path, "new binary downloaded, verified, and activated");
                        info!("release links updated, initiating graceful shutdown");

                        let drain_event = Event::NodeDraining {
                            node_id: ctx.node_id.to_string(),
                            drain_timeout_secs: 30,
                        };

                        if let Err(e) = ctx.bus.publish(&drain_event).await {
                            error!(error = %e, "failed to publish draining event");
                        }

                        begin_graceful_shutdown(ctx.supervisor, 30).await;

                        let complete_event = Event::NodeUpgradeComplete {
                            node_id: ctx.node_id.to_string(),
                            new_binary_version,
                            new_protocol_version,
                        };

                        if let Err(e) = ctx.bus.publish(&complete_event).await {
                            error!(error = %e, "failed to publish upgrade complete event");
                        }

                        info!("exiting for upgrade, expecting systemd restart");
                        std::process::exit(0);
                    }
                    Err(e) => error!(error = %e, "failed to download or verify new binary"),
                }
            }
        }
        Err(e) => error!(error = %e, "upgrade event handling failed"),
    }
}

pub(crate) async fn begin_graceful_shutdown(supervisor: &Arc<Supervisor>, timeout_secs: u64) {
    tracing::info!("Beginning graceful shutdown with {}s timeout", timeout_secs);
    tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)).await;
    tracing::info!("drain timeout elapsed, stopping all instances");
    supervisor
        .shutdown_all(tokio::time::Duration::from_secs(timeout_secs))
        .await;
    tracing::info!("graceful shutdown complete");
}
