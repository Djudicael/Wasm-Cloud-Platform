//! CLI commands for WASI policy enforcement.
//!
//! Provides subcommands to view effective policies, check violation counts,
//! and list available policy profiles.

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;

use common::policy::{FilesystemPolicyConfig, NetworkPolicyConfig, PolicyProfile};

/// Policy-related commands.
#[derive(Subcommand)]
pub enum PolicyCommand {
    /// View the effective policy for a deployed application
    Policy {
        /// Application ID (e.g. "api-users:v2")
        app_id: String,
    },

    /// View policy violation counts for a deployed application
    PolicyViolations {
        /// Application ID (e.g. "api-users:v2")
        app_id: String,
    },

    /// List available policy profiles
    PolicyProfiles,
}

pub async fn run(cmd: PolicyCommand, node_api: &str, http: &reqwest::Client) -> Result<()> {
    match cmd {
        PolicyCommand::Policy { app_id } => show_policy(&app_id, node_api, http).await,
        PolicyCommand::PolicyViolations { app_id } => {
            show_violations(&app_id, node_api, http).await
        }
        PolicyCommand::PolicyProfiles => list_profiles(),
    }
}

/// Fetch an app's config from the node API and display its resolved policy.
async fn show_policy(app_id: &str, node_api: &str, http: &reqwest::Client) -> Result<()> {
    // Try to fetch the app config from the node's admin API
    let url = format!("{}/apps/{}", node_api, app_id);
    let resp = http.get(&url).send().await;

    match resp {
        Ok(resp) if resp.status().is_success() => {
            let config: serde_json::Value = resp.json().await?;
            display_policy_from_config(&config);
        }
        Ok(resp) => {
            let status = resp.status();
            // If the API isn't available, show a helpful message
            println!(
                "{}",
                format!("Could not fetch policy for {}: HTTP {}", app_id, status).yellow()
            );
            println!(
                "{}",
                "The node API may not support the /apps endpoint yet.".dimmed()
            );
        }
        Err(e) => {
            println!(
                "{}",
                format!("Could not reach node API at {}: {}", node_api, e).red()
            );
            println!(
                "{}",
                "Ensure the node is running and --node-api is set correctly.".dimmed()
            );
        }
    }

    Ok(())
}

/// Display a policy from a JSON config value.
fn display_policy_from_config(config: &serde_json::Value) {
    let policy_json = config.get("policy");

    println!("{}", "Network Policy:".bold());
    if let Some(policy) = policy_json.and_then(|p| p.get("network")) {
        display_network_policy(policy);
    } else {
        println!(
            "  {}",
            "(using defaults — no explicit network policy)".dimmed()
        );
        display_network_policy_defaults();
    }

    println!();
    println!("{}", "Filesystem Policy:".bold());
    if let Some(policy) = policy_json.and_then(|p| p.get("filesystem")) {
        display_filesystem_policy(policy);
    } else {
        println!(
            "  {}",
            "(using defaults — no explicit filesystem policy)".dimmed()
        );
        display_filesystem_policy_defaults();
    }
}

fn display_network_policy(policy: &serde_json::Value) {
    let tcp = policy
        .get("allow_outbound_tcp")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "allowed".green() } else { "denied".red() })
        .unwrap_or_else(|| "allowed (default)".green());
    println!("  outbound_tcp: {}", tcp);

    let udp = policy
        .get("allow_outbound_udp")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "allowed".green() } else { "denied".red() })
        .unwrap_or_else(|| "denied (default)".red());
    println!("  outbound_udp: {}", udp);

    let dns = policy
        .get("allow_dns")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "allowed".green() } else { "denied".red() })
        .unwrap_or_else(|| "allowed (default)".green());
    println!("  dns: {}", dns);

    let allowed_cidrs = policy
        .get("allowed_cidrs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            if arr.is_empty() {
                "(all)".to_string()
            } else {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|| "(all)".to_string());
    println!("  allowed_cidrs: {}", allowed_cidrs.cyan());

    let denied_cidrs = policy
        .get("denied_cidrs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            if arr.is_empty() {
                "(none)".to_string()
            } else {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|| "(none)".to_string());
    println!("  denied_cidrs: {}", denied_cidrs.yellow());

    let max_conn = policy
        .get("max_outbound_connections")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "100 (default)".to_string());
    println!("  max_outbound_connections: {}", max_conn);

    let max_egress = policy
        .get("max_egress_bytes")
        .and_then(|v| v.as_u64())
        .map(|n| {
            if n == 0 {
                "unlimited".to_string()
            } else {
                format_bytes(n)
            }
        })
        .unwrap_or_else(|| "unlimited (default)".to_string());
    println!("  max_egress_bytes: {}", max_egress);

    let inbound = policy
        .get("allow_inbound")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "allowed".green() } else { "denied".red() })
        .unwrap_or_else(|| "allowed (default)".green());
    println!("  inbound_tcp_bind: {}", inbound);

    let bind_ports = policy
        .get("allowed_bind_ports")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64())
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "(assigned port)".to_string());
    println!("  allowed_bind_ports: [{}]", bind_ports.cyan());
}

fn display_network_policy_defaults() {
    let defaults = NetworkPolicyConfig {
        allow_outbound_tcp: None,
        allow_outbound_udp: None,
        allow_dns: None,
        allowed_cidrs: None,
        denied_cidrs: None,
        max_outbound_connections: None,
        max_egress_bytes: None,
        allow_inbound: None,
    };
    println!("  outbound_tcp: {}", "allowed".green());
    println!("  outbound_udp: {}", "denied".red());
    println!("  dns: {}", "allowed".green());
    println!("  allowed_cidrs: {}", "(all)".cyan());
    println!("  denied_cidrs: {}", "(none)".yellow());
    println!("  max_outbound_connections: 100");
    println!("  max_egress_bytes: unlimited");
    println!("  inbound_tcp_bind: {}", "allowed".green());
    let _ = defaults; // suppress unused warning
}

fn display_filesystem_policy(policy: &serde_json::Value) {
    let max_fds = policy
        .get("max_open_fds")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "64 (default)".to_string());
    println!("  max_open_fds: {}", max_fds);

    let max_write = policy
        .get("max_fs_write_bytes")
        .and_then(|v| v.as_u64())
        .map(|n| {
            if n == 0 {
                "unlimited".to_string()
            } else {
                format_bytes(n)
            }
        })
        .unwrap_or_else(|| "50 MB (default)".to_string());
    println!("  max_fs_write_bytes: {}", max_write);

    let create = policy
        .get("allow_file_create")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "allowed".green() } else { "denied".red() })
        .unwrap_or_else(|| "denied (default)".red());
    println!("  allow_file_create: {}", create);

    let delete = policy
        .get("allow_file_delete")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "allowed".green() } else { "denied".red() })
        .unwrap_or_else(|| "denied (default)".red());
    println!("  allow_file_delete: {}", delete);

    let paths = policy
        .get("allowed_paths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            if arr.is_empty() {
                "(none)".to_string()
            } else {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|| "(none)".to_string());
    println!("  allowed_paths: {}", paths.cyan());
}

fn display_filesystem_policy_defaults() {
    let _defaults = FilesystemPolicyConfig {
        max_open_fds: None,
        max_fs_write_bytes: None,
        max_fs_read_bytes: None,
        allow_file_create: None,
        allow_file_delete: None,
        allowed_paths: None,
    };
    println!("  max_open_fds: 64");
    println!("  max_fs_write_bytes: 50 MB");
    println!("  allow_file_create: {}", "denied".red());
    println!("  allow_file_delete: {}", "denied".red());
    println!("  allowed_paths: {}", "(none)".cyan());
}

/// Fetch and display policy violation counts for an app.
async fn show_violations(app_id: &str, node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/apps/{}/policy-violations", node_api, app_id);
    let resp = http.get(&url).send().await;

    match resp {
        Ok(resp) if resp.status().is_success() => {
            let violations: serde_json::Value = resp.json().await?;
            println!("{}", format!("Policy violations for {}:", app_id).bold());
            println!();
            println!(
                "  connection_denied: {}",
                fmt_violation_count(&violations, "connection_denied_total")
            );
            println!(
                "  egress_denied:     {}",
                fmt_violation_count(&violations, "egress_denied_total")
            );
            println!(
                "  fd_denied:         {}",
                fmt_violation_count(&violations, "fd_denied_total")
            );
            println!(
                "  fs_write_denied:   {}",
                fmt_violation_count(&violations, "fs_write_denied_total")
            );
            println!(
                "  bind_denied:       {}",
                fmt_violation_count(&violations, "bind_denied_total")
            );
            println!(
                "  dns_denied:        {}",
                fmt_violation_count(&violations, "dns_denied_total")
            );
        }
        Ok(resp) => {
            let status = resp.status();
            println!(
                "{}",
                format!("Could not fetch violations for {}: HTTP {}", app_id, status).yellow()
            );
        }
        Err(e) => {
            println!(
                "{}",
                format!("Could not reach node API at {}: {}", node_api, e).red()
            );
        }
    }

    Ok(())
}

fn fmt_violation_count(violations: &serde_json::Value, key: &str) -> colored::ColoredString {
    let count = violations.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    if count == 0 {
        count.to_string().green()
    } else {
        count.to_string().red()
    }
}

/// List all available policy profiles with descriptions.
fn list_profiles() -> Result<()> {
    println!("{}", "Available Policy Profiles:".bold());
    println!();

    let profiles: Vec<(PolicyProfile, &str, &str)> = vec![
        (
            PolicyProfile::HttpApi,
            "http_api",
            "HTTP API server (inbound + outbound TCP, DNS, no filesystem writes)",
        ),
        (
            PolicyProfile::BackgroundWorker,
            "background_worker",
            "Background worker (outbound TCP, DNS, no inbound, limited fs writes)",
        ),
        (
            PolicyProfile::StaticSite,
            "static_site",
            "Static site (inbound only, no outbound, read-only filesystem)",
        ),
        (
            PolicyProfile::DatabaseProxy,
            "database_proxy",
            "Database proxy (high connection limit, restricted to private CIDRs)",
        ),
        (
            PolicyProfile::Unrestricted,
            "unrestricted",
            "No limits (trusted internal tools only)",
        ),
    ];

    for (_profile, name, description) in &profiles {
        println!("  {} - {}", name.cyan().bold(), description);
    }

    println!();
    println!(
        "{}",
        "Apply a profile with: wasm-ctl deploy <app> --policy-profile <name>".dimmed()
    );
    println!(
        "{}",
        "Override specific settings with: --policy-network-max-outbound-connections, etc.".dimmed()
    );

    // Also show the resolved config for each profile in verbose mode
    println!();
    println!("{}", "Profile Details:".bold());
    for (profile, name, _description) in &profiles {
        println!();
        println!("  {}:", name.cyan());
        let config = profile.to_config();
        if let Some(ref net) = config.network {
            if let Some(tcp) = net.allow_outbound_tcp {
                println!(
                    "    outbound_tcp: {}",
                    if tcp { "allowed" } else { "denied" }
                );
            }
            if let Some(udp) = net.allow_outbound_udp {
                println!(
                    "    outbound_udp: {}",
                    if udp { "allowed" } else { "denied" }
                );
            }
            if let Some(dns) = net.allow_dns {
                println!("    dns: {}", if dns { "allowed" } else { "denied" });
            }
            if let Some(limit) = net.max_outbound_connections {
                println!("    max_outbound_connections: {}", limit);
            }
            if let Some(ref cidrs) = net.allowed_cidrs {
                if !cidrs.is_empty() {
                    println!("    allowed_cidrs: {}", cidrs.join(", "));
                }
            }
            if let Some(ref cidrs) = net.denied_cidrs {
                if !cidrs.is_empty() {
                    println!("    denied_cidrs: {}", cidrs.join(", "));
                }
            }
        }
        if let Some(ref fs) = config.filesystem {
            if let Some(fds) = fs.max_open_fds {
                println!("    max_open_fds: {}", fds);
            }
            if let Some(bytes) = fs.max_fs_write_bytes {
                println!(
                    "    max_fs_write_bytes: {}",
                    if bytes == 0 {
                        "unlimited".to_string()
                    } else {
                        format_bytes(bytes)
                    }
                );
            }
            if let Some(create) = fs.allow_file_create {
                println!(
                    "    allow_file_create: {}",
                    if create { "allowed" } else { "denied" }
                );
            }
            if let Some(delete) = fs.allow_file_delete {
                println!(
                    "    allow_file_delete: {}",
                    if delete { "allowed" } else { "denied" }
                );
            }
        }
    }

    Ok(())
}

/// Format a byte count as a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}
