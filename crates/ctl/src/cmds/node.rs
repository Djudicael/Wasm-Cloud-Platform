// crates/ctl/src/cmds/node.rs
use anyhow::Result;
use common::health::NodeHealthReport;
use messaging::NatsBus;

/// Check node health with detailed output.
pub async fn health(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/status", node_api);
    let resp = http.get(&url).send().await?;

    if resp.status().is_success() {
        let report: NodeHealthReport = resp.json().await?;

        println!("Node Health Report");
        println!("==================");
        println!();

        // Overall status
        let status_str = match report.status {
            common::health::NodeHealthStatus::Healthy => "HEALTHY",
            common::health::NodeHealthStatus::Degraded => "DEGRADED",
            common::health::NodeHealthStatus::Unhealthy => "UNHEALTHY",
        };
        println!("  Status:            {}", status_str);
        println!("  Node ID:           {}", report.node_id);
        println!("  Uptime:            {}s", report.uptime_secs);
        println!("  Active instances:  {}", report.active_instances);
        println!("  Deployed apps:     {}", report.deployed_apps);
        println!(
            "  Accepting traffic: {}",
            if report.accepting_requests {
                "yes"
            } else {
                "no"
            }
        );
        println!();

        // Dependencies
        println!("Dependencies");
        for dep in &report.dependencies {
            let status_icon = match dep.status {
                common::health::DependencyStatus::Healthy => "✓",
                common::health::DependencyStatus::Degraded => "⚠",
                common::health::DependencyStatus::Unhealthy => "✗",
                common::health::DependencyStatus::Unknown => "?",
            };
            let latency = dep
                .latency_ms
                .map(|ms| format!(" ({}ms)", ms))
                .unwrap_or_default();
            println!(
                "  {} {:12} {}{}",
                status_icon, dep.name, dep.message, latency
            );
        }
        println!();

        // Per-app health
        if !report.apps.is_empty() {
            println!("Applications");
            for app in &report.apps {
                let serving = if app.serving {
                    "serving"
                } else {
                    "not serving"
                };
                println!(
                    "  {:30} {}/{} instances  {}",
                    app.app_id, app.healthy_instances, app.instances, serving,
                );
            }
        }
    } else {
        println!("Node health: UNHEALTHY (status {})", resp.status());
    }

    Ok(())
}

/// Check the startup probe (for orchestrators).
pub async fn startup_probe(node_api: &str, http: &reqwest::Client) -> Result<bool> {
    let url = format!("{}/livez", node_api);
    let resp = http.get(&url).send().await?;
    Ok(resp.status().is_success())
}

/// Check the liveness probe.
pub async fn liveness_probe(node_api: &str, http: &reqwest::Client) -> Result<bool> {
    let url = format!("{}/healthz", node_api);
    let resp = http.get(&url).send().await?;
    Ok(resp.status().is_success())
}

/// Check the readiness probe.
pub async fn readiness_probe(node_api: &str, http: &reqwest::Client) -> Result<bool> {
    let url = format!("{}/readyz", node_api);
    let resp = http.get(&url).send().await?;
    Ok(resp.status().is_success())
}

pub async fn rebuild(node_api: &str, http: &reqwest::Client) -> Result<()> {
    println!("WARNING: This will force a full re-bootstrap of the node.");
    println!("All local state will be lost and rebuilt from the cluster.");
    println!();
    println!("This action will:");
    println!("  1. Gracefully drain all traffic from this node");
    println!("  2. Delete the local redb file");
    println!("  3. Restart the node, triggering a full re-bootstrap");
    println!();

    let url = format!("{}/admin/rebuild", node_api);
    let resp = http.post(&url).send().await?;

    if resp.status().is_success() {
        println!("Rebuild initiated successfully.");
    } else {
        let body = resp.text().await?;
        anyhow::bail!("Rebuild failed: {}", body);
    }

    Ok(())
}

/// Show cluster-wide health by reading NodeHealthSnapshot events from NATS.
pub async fn cluster_health(bus: &NatsBus) -> Result<()> {
    use futures::StreamExt;

    let js = async_nats::jetstream::new(bus.client().clone());

    let stream = match js.get_stream("HEALTH").await {
        Ok(s) => s,
        Err(_) => {
            println!("HEALTH stream not found — cluster may not be initialized");
            return Ok(());
        }
    };

    let consumer = match stream.get_consumer("ctl-cluster-health").await {
        Ok(c) => c,
        Err(_) => {
            let config = async_nats::jetstream::consumer::pull::Config {
                durable_name: Some("ctl-cluster-health".to_string()),
                ..Default::default()
            };
            stream.create_consumer(config).await?
        }
    };

    let mut messages = consumer.messages().await?;
    let mut node_statuses: std::collections::HashMap<String, NodeStatus> =
        std::collections::HashMap::new();

    let mut count = 0u32;
    while let Some(msg) = messages.next().await {
        if count >= 100 {
            break;
        }
        let msg = msg?;
        if let Ok(messaging::events::Event::NodeHealthSnapshot {
            node_id,
            status,
            active_instances,
            deployed_apps,
            nats_connected,
            disk_free_mb,
            memory_used_mb,
            ..
        }) = serde_json::from_slice::<messaging::events::Event>(&msg.payload)
        {
            let entry = node_statuses.entry(node_id.clone()).or_default();
            entry.status = Some(status);
            entry.active_instances = Some(active_instances);
            entry.deployed_apps = Some(deployed_apps);
            entry.nats_connected = Some(nats_connected);
            entry.disk_free_mb = Some(disk_free_mb);
            entry.memory_used_mb = Some(memory_used_mb);
        }
        let _ = msg.ack().await;
        count += 1;
    }

    println!("Cluster Health Status");
    println!("=====================");
    if node_statuses.is_empty() {
        println!("No health snapshots detected in cluster");
    } else {
        for (node_id, status) in node_statuses.iter() {
            let health = status.status.as_deref().unwrap_or("unknown");
            let instances = status
                .active_instances
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let nats = status
                .nats_connected
                .map(|b| if b { "connected" } else { "disconnected" })
                .unwrap_or("unknown");
            println!(
                "  {:20} status={:10} instances={:4} nats={}",
                node_id, health, instances, nats
            );
        }
    }

    Ok(())
}

#[derive(Default)]
struct NodeStatus {
    status: Option<String>,
    active_instances: Option<u32>,
    deployed_apps: Option<u32>,
    nats_connected: Option<bool>,
    disk_free_mb: Option<u64>,
    memory_used_mb: Option<u64>,
}
