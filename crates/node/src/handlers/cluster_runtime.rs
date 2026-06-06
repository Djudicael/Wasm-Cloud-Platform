//! Cluster runtime state mutations.
//!
//! These helpers keep routing, gateway state, and node health/load updates out
//! of the top-level event dispatcher so the event match stays readable.

use std::sync::Arc;

use common::{error::PlatformError, types::AppId};
use proxy::{
    dns_webhook::DnsWebhookManager,
    node_table::{NodeEntry, NodeLoadTable},
    router::HostRouter,
};
use storage::Store;
use tracing::info;

use super::{extract_proxy_host, merge_cluster_node_record, now_unix_secs};

pub(crate) struct ClusterRuntimeContext<'a> {
    pub host_router: &'a Arc<HostRouter>,
    pub store: &'a Store,
    pub dns_webhook: Option<&'a DnsWebhookManager>,
    pub node_table: &'a Arc<NodeLoadTable>,
    pub gateway: Option<&'a Arc<proxy::gateway::Gateway>>,
}

pub(crate) async fn handle_route_add(
    ctx: ClusterRuntimeContext<'_>,
    route: common::types::Route,
) -> Result<(), PlatformError> {
    ctx.store.save_route(&route)?;
    ctx.host_router
        .add_route(
            route.host.clone(),
            route.path_prefix.clone(),
            route.app_id.clone(),
            route.strip_prefix,
        )
        .await;
    info!(host = %route.host, app = %route.app_id.0, "route added");
    if let Some(webhook) = ctx.dns_webhook {
        webhook
            .notify_route_change("add", &route.host, &route.app_id.0)
            .await;
    }
    Ok(())
}

pub(crate) async fn handle_route_remove(
    ctx: ClusterRuntimeContext<'_>,
    host: String,
) -> Result<(), PlatformError> {
    let existing = ctx.store.load_route(&host).ok().flatten();
    let app_id = existing.as_ref().map(|r| r.app_id.clone());
    let path_prefix = existing
        .as_ref()
        .map(|r| r.path_prefix.clone())
        .unwrap_or_default();
    ctx.store.delete_route(&host)?;
    ctx.host_router.remove_route(&host, &path_prefix).await;
    info!(host, "route removed");
    if let Some(webhook) = ctx.dns_webhook {
        if let Some(app_id) = app_id {
            webhook
                .notify_route_change("remove", &host, &app_id.0)
                .await;
        }
    }
    Ok(())
}

pub(crate) async fn handle_node_load(
    ctx: ClusterRuntimeContext<'_>,
    node_id: String,
    fuel_budget_used_percent: f32,
    active_instances: u32,
    proxy_address: String,
) -> Result<(), PlatformError> {
    let mut cluster_node =
        merge_cluster_node_record(ctx.store.load_cluster_node(&node_id)?, &node_id);
    cluster_node.last_seen_unix_secs = now_unix_secs();
    cluster_node.proxy_address = Some(proxy_address.clone());
    cluster_node.active_instances = Some(active_instances);
    ctx.store.save_cluster_node(&cluster_node)?;

    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let entry = NodeEntry {
        node_id: node_id.clone(),
        proxy_address,
        fuel_used_percent: fuel_budget_used_percent,
        active_instances,
        last_seen: now,
        health_status: common::health::NodeHealthStatus::Healthy,
    };
    ctx.node_table.update(entry).await;

    if let Some(webhook) = ctx.dns_webhook {
        let nodes = ctx.node_table.nodes.read().await;
        let ips: Vec<String> = nodes
            .values()
            .filter_map(|n| extract_proxy_host(&n.proxy_address))
            .collect();
        drop(nodes);
        webhook.set_node_ips(ips).await;
    }
    Ok(())
}

fn parse_health_status(status: &str) -> common::health::NodeHealthStatus {
    match status {
        "healthy" => common::health::NodeHealthStatus::Healthy,
        "degraded" => common::health::NodeHealthStatus::Degraded,
        "unhealthy" => common::health::NodeHealthStatus::Unhealthy,
        _ => common::health::NodeHealthStatus::Degraded,
    }
}

pub(crate) async fn handle_node_health_changed(
    ctx: ClusterRuntimeContext<'_>,
    node_id: String,
    status: String,
    active_instances: u32,
    accepting_requests: bool,
) -> Result<(), PlatformError> {
    let health_status = parse_health_status(&status);
    ctx.node_table.update_health(&node_id, health_status).await;
    let mut cluster_node =
        merge_cluster_node_record(ctx.store.load_cluster_node(&node_id)?, &node_id);
    cluster_node.last_seen_unix_secs = now_unix_secs();
    cluster_node.health_status = health_status;
    cluster_node.active_instances = Some(active_instances);
    cluster_node.accepting_requests = Some(accepting_requests);
    ctx.store.save_cluster_node(&cluster_node)?;
    Ok(())
}

pub(crate) async fn handle_node_health_snapshot(
    ctx: ClusterRuntimeContext<'_>,
    node_id: String,
    status: String,
    active_instances: u32,
    deployed_apps: u32,
) -> Result<(), PlatformError> {
    let health_status = parse_health_status(&status);
    ctx.node_table.update_health(&node_id, health_status).await;
    let mut cluster_node =
        merge_cluster_node_record(ctx.store.load_cluster_node(&node_id)?, &node_id);
    cluster_node.last_seen_unix_secs = now_unix_secs();
    cluster_node.health_status = health_status;
    cluster_node.active_instances = Some(active_instances);
    cluster_node.deployed_apps = Some(deployed_apps);
    ctx.store.save_cluster_node(&cluster_node)?;
    Ok(())
}

pub(crate) async fn handle_gateway_config_update(
    ctx: ClusterRuntimeContext<'_>,
    app_id: AppId,
    config: common::types::GatewayRouteConfig,
) -> Result<(), PlatformError> {
    info!(app = %app_id.0, "received gateway config update");
    ctx.store.save_gateway_config(&app_id.0, &config)?;
    if let Some(gw) = ctx.gateway {
        gw.set_route_config(&app_id.0, config).await;
    }
    Ok(())
}

pub(crate) async fn handle_gateway_config_remove(
    ctx: ClusterRuntimeContext<'_>,
    app_id: AppId,
) -> Result<(), PlatformError> {
    info!(app = %app_id.0, "received gateway config remove");
    ctx.store.delete_gateway_config(&app_id.0)?;
    if let Some(gw) = ctx.gateway {
        gw.remove_route_config(&app_id.0).await;
    }
    Ok(())
}
