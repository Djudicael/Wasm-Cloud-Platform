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
                return Err(anyhow::anyhow!(
                    "Node API returned status {}",
                    resp.status()
                ));
            }
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to connect to node API: {}", e));
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
                return Err(anyhow::anyhow!(
                    "Node API returned status {}",
                    resp.status()
                ));
            }
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to connect to node API: {}", e));
        }
    }
    Ok(())
}
