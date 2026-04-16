// crates/ctl/src/cmds/list.rs
use anyhow::Result;
use colored::Colorize;

pub async fn run(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/apps", node_api);
    match http.get(&url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                let apps: serde_json::Value = resp.json().await?;
                println!("{}", "Deployed applications:".bold());
                println!("{}", serde_json::to_string_pretty(&apps)?);
            } else {
                println!("{} Node API returned status {}", "✗".red(), resp.status());
            }
        }
        Err(e) => {
            println!("{} Failed to connect to node API: {}", "✗".red(), e);
        }
    }
    Ok(())
}

pub async fn instances(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/upstreams", node_api);
    match http.get(&url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await?;
                println!("{}", "Running instances:".bold());
                println!("{}", serde_json::to_string_pretty(&data)?);
            } else {
                println!("{} Node API returned status {}", "✗".red(), resp.status());
            }
        }
        Err(e) => {
            println!("{} Failed to connect to node API: {}", "✗".red(), e);
        }
    }
    Ok(())
}
