// crates/ctl/src/cmds/status.rs
use anyhow::Result;
use colored::Colorize;

pub async fn run(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let health_url = format!("{}/health", node_api);

    println!("{}", "Cluster Status:".bold());
    println!();

    // Check node health
    match http.get(&health_url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                println!("  Node API:     {} {}", "✓".green(), "healthy".green());
            } else {
                println!("  Node API:     {} status {}", "✗".red(), resp.status());
            }
        }
        Err(e) => {
            println!("  Node API:     {} unreachable ({})", "✗".red(), e);
        }
    }

    // Try to fetch apps
    let apps_url = format!("{}/apps", node_api);
    if let Ok(resp) = http.get(&apps_url).send().await {
        if resp.status().is_success() {
            if let Ok(apps) = resp.json::<serde_json::Value>().await {
                if let Some(arr) = apps.as_array() {
                    println!("  Apps:         {}", arr.len().to_string().cyan());
                }
            }
        }
    }

    // Try to fetch upstreams
    let upstreams_url = format!("{}/upstreams", node_api);
    if let Ok(resp) = http.get(&upstreams_url).send().await {
        if resp.status().is_success() {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                if let Some(obj) = data.as_object() {
                    let total: usize = obj
                        .values()
                        .filter_map(|v| v.as_array())
                        .map(|arr| arr.len())
                        .sum();
                    println!("  Instances:    {}", total.to_string().cyan());
                }
            }
        }
    }

    Ok(())
}
