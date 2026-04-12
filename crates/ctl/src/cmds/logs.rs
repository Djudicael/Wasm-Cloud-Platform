// crates/ctl/src/cmds/logs.rs
use anyhow::Result;
use colored::Colorize;

pub async fn run(app_id: &str, node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/logs/{}", node_api, app_id);
    println!(
        "{} Streaming logs for {} (Ctrl-C to stop)...",
        "→".cyan(),
        app_id.yellow()
    );

    match http
        .get(&url)
        .header("accept", "text/event-stream")
        .send()
        .await
    {
        Ok(mut resp) => {
            while let Some(chunk) = resp.chunk().await? {
                let text = String::from_utf8_lossy(&chunk);
                for line in text.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        // Try to pretty-print JSON, else print raw
                        match serde_json::from_str::<serde_json::Value>(data) {
                            Ok(json) => {
                                if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
                                    let level = json
                                        .get("level")
                                        .and_then(|l| l.as_str())
                                        .unwrap_or("INFO");
                                    let colored_level = match level {
                                        "ERROR" => level.red(),
                                        "WARN" => level.yellow(),
                                        "INFO" => level.cyan(),
                                        "DEBUG" => level.bright_black(),
                                        _ => level.normal(),
                                    };
                                    println!("[{}] {}", colored_level, msg);
                                } else {
                                    println!("{}", serde_json::to_string_pretty(&json)?);
                                }
                            }
                            Err(_) => println!("{}", data),
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!(
                "{} SSE endpoint not available (not implemented yet): {}",
                "⚠".yellow(),
                e
            );
        }
    }
    Ok(())
}
