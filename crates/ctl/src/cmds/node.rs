use anyhow::Result;
use messaging::NatsBus;

pub async fn health(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/health", node_api);
    let resp = http.get(&url).send().await?;

    if resp.status().is_success() {
        println!("Node health: OK");
    } else {
        println!("Node health: UNHEALTHY (status {})", resp.status());
    }

    Ok(())
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

pub async fn cluster_health(bus: &NatsBus) -> Result<()> {
    use futures::StreamExt;

    let js = async_nats::jetstream::new(bus.client().clone());

    let stream = match js.get_stream("DEPLOY").await {
        Ok(s) => s,
        Err(_) => {
            println!("DEPLOY stream not found — cluster may not be initialized");
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
        if let Ok(event) = serde_json::from_slice::<messaging::events::Event>(&msg.payload) {
            match event {
                messaging::events::Event::NodeJoined { node_id, .. } => {
                    node_statuses.entry(node_id.clone()).or_default().last_seen =
                        Some("connected".to_string());
                }
                messaging::events::Event::NodeLoad { node_id, .. } => {
                    node_statuses.entry(node_id.clone()).or_default().last_seen =
                        Some("connected".to_string());
                }
                _ => {}
            }
        }
        let _ = msg.ack().await;
        count += 1;
    }

    println!("Cluster Health Status");
    println!("=====================");
    if node_statuses.is_empty() {
        println!("No nodes detected in cluster");
    } else {
        for (node_id, status) in node_statuses.iter() {
            let nats_status = status.last_seen.as_deref().unwrap_or("unknown");
            println!("  {}: NATS={}", node_id, nats_status);
        }
    }

    Ok(())
}

#[derive(Default)]
struct NodeStatus {
    last_seen: Option<String>,
}
