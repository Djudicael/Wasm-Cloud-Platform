//! Upstream selection and node steering decisions.
//!
//! This keeps the request-facing service entrypoint focused on the Pingora
//! hooks while the node-local versus remote selection policy lives here.

use common::health::NodeHealthStatus;
use common::types::AppId;

use super::{WasmProxy, REMOTE_STEER_FUEL_THRESHOLD_PERCENT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionError {
    AdmissionFenced,
    NoUpstream,
}

pub(super) async fn select_upstream(
    proxy: &WasmProxy,
    app_id: &AppId,
) -> Result<crate::upstream::UpstreamEndpoint, SelectionError> {
    if let Some(endpoint) = proxy.upstream.next(app_id).await {
        return Ok(endpoint);
    }

    if node_is_overloaded(proxy).await {
        if let Some(node) = proxy
            .node_table
            .least_loaded_other_node(&proxy.local_node_id)
            .await
        {
            match tokio::net::lookup_host(&node.proxy_address).await {
                Ok(mut addrs) => {
                    if let Some(addr) = addrs.next() {
                        return Ok(crate::upstream::UpstreamEndpoint { addr, h2c: false });
                    }
                    tracing::warn!(
                        node = %node.node_id,
                        proxy_address = %node.proxy_address,
                        "remote node advertised no resolvable proxy address"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        node = %node.node_id,
                        proxy_address = %node.proxy_address,
                        error = %error,
                        "failed to resolve remote node proxy address"
                    );
                }
            }
        }
    }

    if !proxy.backpressure.can_cold_start() {
        return Err(SelectionError::AdmissionFenced);
    }
    tracing::info!(app_id = %app_id.0, "cold start on local node");
    let started = (proxy.cold_start)(app_id.clone()).await;
    if started.is_none() {
        return Err(if proxy.backpressure.can_cold_start() {
            SelectionError::NoUpstream
        } else {
            SelectionError::AdmissionFenced
        });
    }
    proxy.upstream.next(app_id).await.ok_or_else(|| {
        tracing::warn!(app_id = %app_id.0, "cold start returned no registered upstream");
        SelectionError::NoUpstream
    })
}

pub(super) async fn node_is_overloaded(proxy: &WasmProxy) -> bool {
    let unhealthy = proxy.node_table.is_unhealthy(&proxy.local_node_id).await;
    if unhealthy {
        return true;
    }

    let nodes = proxy.node_table.nodes.read().await;
    let Some(local) = nodes.get(&proxy.local_node_id) else {
        return false;
    };
    local.health_status == NodeHealthStatus::Unhealthy
        || local.fuel_used_percent >= REMOTE_STEER_FUEL_THRESHOLD_PERCENT
}
