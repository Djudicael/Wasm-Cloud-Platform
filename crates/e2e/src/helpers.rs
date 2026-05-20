//! Helper functions for chaos testing.
//!
//! Provides HTTP utilities, wait-for functions, deployment helpers, and
//! artifact management used by the fixture, injector, verifier, and
//! chaos test scenarios.
//!
//! ## WSL / Linux Requirement
//!
//! Some helpers (e.g., `wait_for_tcp`) use Unix-specific APIs. Run inside
//! WSL or on a native Linux host for full compatibility.

use messaging::NatsBus;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

// ── TCP / HTTP Wait Helpers ──────────────────────────────────────────

/// Wait for a TCP port to be accepting connections.
///
/// Polls the port every 100ms until a connection succeeds or the timeout
/// is exceeded. Used to verify that NATS, the admin API, or the proxy
/// are ready before proceeding with the test.
pub async fn wait_for_tcp(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        if check_tcp(host, port) {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "TCP {host}:{port} not reachable within {:?}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Non-async check if a TCP port is accepting connections.
///
/// Returns `true` if a connection to `host:port` succeeds immediately.
/// This is a synchronous, non-blocking check suitable for use in
/// `spawn_blocking` contexts or when you don't want to await.
pub fn check_tcp(host: &str, port: u16) -> bool {
    TcpStream::connect(format!("{host}:{port}")).is_ok()
}

/// Wait for a node's health endpoint to return HTTP 200.
///
/// Polls `GET http://{admin_addr}/health` until it returns a success status
/// or the timeout is exceeded. This is the primary readiness check for
/// wasm-node processes.
pub async fn wait_for_health(admin_addr: &str, timeout: Duration) -> Result<(), String> {
    let start = std::time::Instant::now();
    let url = format!("http://{admin_addr}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                return Ok(());
            }
            Ok(resp) => {
                // Server responded but not 200 — may still be starting
                if start.elapsed() > timeout {
                    return Err(format!(
                        "health endpoint at {admin_addr} returned {} (not healthy) within {:?}",
                        resp.status(),
                        timeout
                    ));
                }
            }
            Err(_) => {
                if start.elapsed() > timeout {
                    return Err(format!(
                        "health endpoint at {admin_addr} not ready within {:?}",
                        timeout
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait for an app to be ready by sending HTTP requests until one succeeds.
///
/// Sends `GET /` with the given `Host` header to the proxy port. Retries
/// up to `max_attempts` times with 500ms delay between attempts.
pub async fn wait_for_app_ready(
    proxy_port: u16,
    host: &str,
    max_attempts: u32,
) -> Result<(), String> {
    for i in 0..max_attempts {
        match send_request(proxy_port, host, "/").await {
            Ok(resp) if resp.status().is_success() => {
                info!(host, attempts = i + 1, "app is ready");
                return Ok(());
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(format!(
        "app on host '{host}' did not become ready after {max_attempts} attempts"
    ))
}

// ── HTTP Request Helpers ─────────────────────────────────────────────

/// Send an HTTP GET request to the proxy with a `Host` header.
///
/// Returns the full `reqwest::Response` so the caller can inspect status,
/// headers, and body.
pub async fn send_request(
    proxy_port: u16,
    host: &str,
    path: &str,
) -> Result<reqwest::Response, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let url = format!("http://127.0.0.1:{proxy_port}{path}");

    client
        .get(&url)
        .header("Host", host)
        .send()
        .await
        .map_err(|e| format!("request to {url} (Host: {host}) failed: {e}"))
}

/// Send an HTTP GET request and return the response body as text.
pub async fn send_request_text(
    proxy_port: u16,
    host: &str,
    path: &str,
) -> Result<(u16, String), String> {
    let resp = send_request(proxy_port, host, path).await?;
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;
    Ok((status, body))
}

// ── Artifact Management ──────────────────────────────────────────────

/// Compute SHA-256 hex digest of a file.
///
/// Reads the entire file into memory and computes the hash. Suitable for
/// small files (Wasm artifacts are typically < 50 MB).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes =
        std::fs::read(path).map_err(|e| format!("failed to read file {}: {e}", path.display()))?;
    let hash = Sha256::digest(&bytes);
    Ok(hex::encode(hash))
}

/// Compute SHA-256 hex digest of raw bytes.
pub fn sha256_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hex::encode(hash)
}

/// Upload a WASM artifact to a node's artifact server.
///
/// Sends a `PUT` request to `http://127.0.0.1:{artifact_port}/artifacts/{sha256}`
/// with the WASM bytes as the body. The artifact server stores the file
/// so it can be fetched by other nodes during deployment.
pub async fn upload_artifact(
    artifact_port: u16,
    wasm_path: &Path,
    sha256: &str,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{artifact_port}/artifacts/{sha256}");

    let wasm_bytes = std::fs::read(wasm_path)
        .map_err(|e| format!("failed to read WASM file {}: {e}", wasm_path.display()))?;

    let response = client
        .put(&url)
        .header("content-type", "application/wasm")
        .body(wasm_bytes)
        .send()
        .await
        .map_err(|e| format!("failed to upload artifact: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "failed to upload artifact: status {}",
            response.status()
        ));
    }

    info!(sha256, port = artifact_port, "artifact uploaded");
    Ok(())
}

// ── App Deployment ───────────────────────────────────────────────────

/// Build a default `AppConfig` for testing.
///
/// Creates a configuration with the given `app_id`, fuel quota, memory
/// limit, and max instances. All other fields use sensible defaults.
pub fn build_app_config(
    app_id: &str,
    fuel_quota: u64,
    memory_pages: u32,
    max_instances: u32,
) -> common::types::AppConfig {
    build_app_config_with_namespace(app_id, fuel_quota, memory_pages, max_instances, "default")
}

pub fn build_app_config_with_namespace(
    app_id: &str,
    fuel_quota: u64,
    memory_pages: u32,
    max_instances: u32,
    namespace: &str,
) -> common::types::AppConfig {
    common::types::AppConfig {
        id: common::types::AppId(app_id.to_string()),
        fuel_quota: common::types::FuelQuota(fuel_quota),
        memory_limit: common::types::MemoryPages(memory_pages),
        max_instances,
        idle_timeout_secs: 300,
        wasm_bind_port: 8080,
        env_vars: std::collections::HashMap::new(),
        secret_keys: Vec::new(),
        extended_limits: None,
        health_check_path: None,
        db_max_connections: None,
        rate_limit: None,
        tenant_id: None,
        policy: None,
        namespace: namespace.to_string(),
    }
}

/// Deploy an app via NATS event bus.
///
/// Publishes a `DeployApp` event with the given configuration and artifact
/// URL. The event is consumed by all nodes in the cluster, which download
/// the artifact and start instances.
pub async fn deploy_app(
    bus: &NatsBus,
    app_id: &str,
    artifact_url: String,
    expected_hash: String,
    size_bytes: u64,
    config: common::types::AppConfig,
) -> Result<(), String> {
    let event = messaging::events::Event::DeployApp {
        app_id: common::types::AppId(app_id.to_string()),
        config,
        artifact_url: artifact_url.clone(),
        artifact_auth_token: None,
        artifact_transfer_manifest: None,
        expected_hash: Some(expected_hash.clone()),
        size_bytes,
    };

    info!(
        app_id,
        artifact_url,
        expected_hash,
        subject = event.subject(),
        "publishing DeployApp event"
    );

    bus.publish(&event)
        .await
        .map_err(|e| format!("deploy publish failed: {e}"))?;

    // Wait for the event to be processed
    tokio::time::sleep(Duration::from_millis(500)).await;

    info!(app_id, "DeployApp event published");
    Ok(())
}

/// Add a route via NATS event bus.
///
/// Publishes a `RouteAdd` event that maps the given host to the app.
/// All nodes in the cluster receive this event and update their host
/// router.
pub async fn add_route(bus: &NatsBus, host: &str, app_id: &str) -> Result<(), String> {
    let route = common::types::Route {
        host: host.to_string(),
        app_id: common::types::AppId(app_id.to_string()),
        path_prefix: "/".to_string(),
        strip_prefix: false,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    };

    let event = messaging::events::Event::RouteAdd { route };

    bus.publish(&event)
        .await
        .map_err(|e| format!("route add publish failed: {e}"))?;

    // Wait for the route to propagate
    tokio::time::sleep(Duration::from_millis(200)).await;

    info!(host, app_id, "RouteAdd event published");
    Ok(())
}

/// Remove an app via NATS event bus.
///
/// Publishes a `RemoveApp` event that triggers instance shutdown and
/// billing record creation on all nodes.
pub async fn remove_app(bus: &NatsBus, app_id: &str) -> Result<(), String> {
    let event = messaging::events::Event::RemoveApp {
        app_id: common::types::AppId(app_id.to_string()),
    };

    bus.publish(&event)
        .await
        .map_err(|e| format!("remove app publish failed: {e}"))?;

    // Wait for the removal to process
    tokio::time::sleep(Duration::from_secs(1)).await;

    info!(app_id, "RemoveApp event published");
    Ok(())
}

/// Remove a route via NATS event bus.
///
/// Publishes a `RouteRemove` event that deletes the route from all nodes.
pub async fn remove_route(bus: &NatsBus, host: &str) -> Result<(), String> {
    let event = messaging::events::Event::RouteRemove {
        host: host.to_string(),
    };

    bus.publish(&event)
        .await
        .map_err(|e| format!("route remove publish failed: {e}"))?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    info!(host, "RouteRemove event published");
    Ok(())
}

/// Set a gateway config for an app via NATS event bus.
///
/// Publishes a `GatewayConfigUpdate` event that updates the gateway
/// route configuration for the given app. The event is persisted to
/// JetStream so all nodes receive it.
pub async fn set_gateway_config(
    bus: &NatsBus,
    app_id: &str,
    config: common::types::GatewayRouteConfig,
) -> Result<(), String> {
    let event = messaging::events::Event::GatewayConfigUpdate {
        app_id: common::types::AppId(app_id.to_string()),
        config,
    };
    bus.publish(&event)
        .await
        .map_err(|e| format!("gateway config publish failed: {e}"))?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    info!(app_id, "GatewayConfigUpdate event published");
    Ok(())
}

/// Wait for a gateway config to be persisted on a node.
///
/// Polls the node's admin API `GET /admin/gateway/{app_id}` until it
/// returns 200 OK, confirming the node has processed and saved the config.
pub async fn wait_for_gateway_config(
    admin_addr: &str,
    app_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    // URL-encode the app_id so colons (e.g. "echo-service:v1") don't break the path.
    let encoded: String = app_id
        .chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            ' ' => "%20".to_string(),
            c => c.to_string(),
        })
        .collect();
    let url = format!("http://{admin_addr}/admin/gateway/{encoded}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let start = std::time::Instant::now();
    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(app_id, "gateway config confirmed on node");
                return Ok(());
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                info!(app_id, url, status = %status, body, "gateway config not yet saved");
                if start.elapsed() > timeout {
                    return Err(format!(
                        "gateway config for {app_id} not saved on node: status {status}, body: {body}"
                    ));
                }
            }
            Err(e) => {
                info!(app_id, url, error = %e, "gateway config poll failed");
                if start.elapsed() > timeout {
                    return Err(format!(
                        "gateway config for {app_id} not saved on node: {e}"
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Ensure a hostname resolves to 127.0.0.1 by adding it to /etc/hosts.
///
/// Tries multiple privilege-escalation strategies so tests work in
/// containers (root), WSL (`wsl -u root`), and dev machines (sudo).
/// Returns `true` if the entry was added or already present.
/// Returns `false` only if the file doesn't exist (non-Unix).
pub fn ensure_hosts_entry(hostname: &str) -> Result<bool, String> {
    let entry = format!("127.0.0.1 {}\n", hostname);
    let hosts_path = std::path::Path::new("/etc/hosts");

    if !hosts_path.exists() {
        return Ok(false);
    }

    let contents = std::fs::read_to_string(hosts_path)
        .map_err(|e| format!("failed to read /etc/hosts: {e}"))?;
    if contents.contains(&format!("127.0.0.1 {}", hostname)) {
        return Ok(false); // already present
    }

    // Strategy 1: direct write (works when running as root, e.g. CI containers)
    if let Ok(()) = try_append_hosts(&entry) {
        info!(hostname, "added to /etc/hosts (direct write)");
        return Ok(true);
    }

    // Strategy 2: WSL — escalate via `wsl -u root`
    if is_wsl() {
        let cmd = format!("echo '127.0.0.1 {}' >> /etc/hosts", hostname);
        match std::process::Command::new("wsl")
            .args(["-u", "root", "sh", "-c", &cmd])
            .output()
        {
            Ok(output) if output.status.success() => {
                info!(hostname, "added to /etc/hosts via WSL root");
                return Ok(true);
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                info!(hostname, stderr = %stderr, "WSL root write failed");
            }
            Err(e) => info!(hostname, error = %e, "WSL root command failed"),
        }
    }

    // Strategy 3: passwordless sudo (dev machines with NOPASSWD)
    match std::process::Command::new("sudo")
        .args(["-n", "tee", "-a", "/etc/hosts"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                use std::io::Write;
                let _ = stdin.write_all(entry.as_bytes());
            }
            match child.wait() {
                Ok(status) if status.success() => {
                    info!(hostname, "added to /etc/hosts via sudo");
                    return Ok(true);
                }
                Ok(_) => info!(
                    hostname,
                    "sudo tee failed (wrong password or no sudo access)"
                ),
                Err(e) => info!(hostname, error = %e, "sudo tee command failed"),
            }
        }
        Err(e) => info!(hostname, error = %e, "sudo command spawn failed"),
    }

    // Strategy 4: pkexec (GUI privilege escalation, Linux desktops)
    match std::process::Command::new("pkexec")
        .args(["tee", "-a", "/etc/hosts"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                use std::io::Write;
                let _ = stdin.write_all(entry.as_bytes());
            }
            match child.wait() {
                Ok(status) if status.success() => {
                    info!(hostname, "added to /etc/hosts via pkexec");
                    return Ok(true);
                }
                Ok(_) => info!(hostname, "pkexec tee failed (cancelled or no polkit)"),
                Err(e) => info!(hostname, error = %e, "pkexec tee command failed"),
            }
        }
        Err(e) => info!(hostname, error = %e, "pkexec command spawn failed"),
    }

    // All strategies exhausted — warn the user
    info!(
        hostname,
        "could not modify /etc/hosts — ensure the test runner has root privileges, \
         or add '127.0.0.1 {}' to /etc/hosts manually before running tests",
        hostname
    );
    Err(format!(
        "failed to add {} to /etc/hosts (tried direct write, WSL root, sudo, pkexec). \
         Run with root/sudo or add the entry manually.",
        hostname
    ))
}

fn try_append_hosts(entry: &str) -> Result<(), std::io::Error> {
    let hosts_path = std::path::Path::new("/etc/hosts");
    let mut file = std::fs::OpenOptions::new().append(true).open(hosts_path)?;
    use std::io::Write;
    file.write_all(entry.as_bytes())
}

/// Detect if we're running inside WSL (Windows Subsystem for Linux).
fn is_wsl() -> bool {
    // WSL sets these env vars; checking both covers WSL1 and WSL2.
    std::env::var("WSL_DISTRO_NAME").is_ok() || std::env::var("WSLENV").is_ok()
}

// ── WASM Binary Discovery ────────────────────────────────────────────

/// Find the hello-axum WASM test app.
///
/// Searches in `target/wasm32-wasip2/release/hello-axum.wasm` relative to
/// the workspace root. If not found or stale, attempts to build it.
///
/// **Must run inside WSL** because the WASI target requires a Unix-like
/// toolchain.
pub fn find_hello_axum_wasm() -> Result<PathBuf, String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));

    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm");

    // Check if needs rebuild
    let needs_rebuild = if !wasm_path.exists() {
        true
    } else {
        let wasm_modified = std::fs::metadata(&wasm_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let src_modified = std::fs::metadata(workspace_root.join("apps/hello-axum/src/main.rs"))
            .ok()
            .and_then(|m| m.modified().ok());

        match (wasm_modified, src_modified) {
            (Some(wasm), Some(src)) => wasm < src,
            _ => false,
        }
    };

    if needs_rebuild {
        info!("building hello-axum.wasm...");

        let output = std::process::Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "hello-axum",
            ])
            .current_dir(workspace_root)
            .output()
            .map_err(|e| format!("failed to run cargo build: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("failed to build hello-axum: {stderr}"));
        }
    }

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("build succeeded but hello-axum.wasm not found".to_string())
    }
}

/// Find the echo-service WASM test app.
///
/// Same logic as `find_hello_axum_wasm` but for the echo-service app.
pub fn find_echo_service_wasm() -> Result<PathBuf, String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));

    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/echo-service.wasm");

    let needs_rebuild = if !wasm_path.exists() {
        true
    } else {
        let wasm_modified = std::fs::metadata(&wasm_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let src_modified = std::fs::metadata(workspace_root.join("apps/echo-service/src/main.rs"))
            .ok()
            .and_then(|m| m.modified().ok());

        match (wasm_modified, src_modified) {
            (Some(wasm), Some(src)) => wasm < src,
            _ => false,
        }
    };

    if needs_rebuild {
        info!("building echo-service.wasm...");

        let output = std::process::Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "echo-service",
            ])
            .current_dir(workspace_root)
            .output()
            .map_err(|e| format!("failed to run cargo build: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("failed to build echo-service: {stderr}"));
        }
    }

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("build succeeded but echo-service.wasm not found".to_string())
    }
}

// ── Billing Helpers ──────────────────────────────────────────────────

/// Count billing records via the admin API.
///
/// Returns the number of billing records stored on the node. Useful for
/// verifying that billing records survive a node restart.
pub async fn count_billing_records(admin_addr: &str) -> Result<u64, String> {
    let url = format!("http://{admin_addr}/admin/billing/count");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("billing count request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("billing count returned status {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(body.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
}

// ── Retry Helper ─────────────────────────────────────────────────────

/// Retry an async operation with exponential backoff.
///
/// Tries the operation up to `max_retries` times. The delay starts at
/// `initial_delay` and doubles each retry. Returns the first successful
/// result or the last error.
pub async fn retry<F, Fut, T>(
    name: &str,
    max_retries: u32,
    initial_delay: Duration,
    f: F,
) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut delay = initial_delay;
    let mut last_err = String::new();

    for attempt in 0..=max_retries {
        match f().await {
            Ok(val) => {
                if attempt > 0 {
                    info!(name, attempt, "retry succeeded");
                }
                return Ok(val);
            }
            Err(e) => {
                last_err = e;
                if attempt < max_retries {
                    warn!(
                        name,
                        attempt,
                        delay_ms = delay.as_millis(),
                        error = %last_err,
                        "retry: attempt failed, waiting before next"
                    );
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
            }
        }
    }

    Err(format!(
        "{name}: all {} retries failed — last error: {last_err}",
        max_retries
    ))
}

// ── Setup Helpers (used by chaos scenarios) ──────────────────────────

/// Deploy a test app to a cluster fixture and wait for it to be ready.
///
/// This is a convenience function that:
/// 1. Finds the hello-axum WASM binary
/// 2. Computes its SHA-256 hash
/// 3. Uploads the artifact to the first node
/// 4. Publishes a DeployApp event
/// 5. Adds a route for the given host
/// 6. Waits for the app to be ready
///
/// Returns `Ok(())` if the app is deployed and serving traffic.
pub async fn setup_deploy_app(
    fixture: &crate::fixture::ClusterFixture,
    app_id: &str,
    host: &str,
) -> Result<(), String> {
    let wasm_path = find_hello_axum_wasm()?;
    let sha256 = sha256_file(&wasm_path)?;
    let size_bytes = std::fs::metadata(&wasm_path)
        .map_err(|e| format!("failed to read wasm metadata: {e}"))?
        .len();

    // Upload artifact to the first node
    upload_artifact(fixture.nodes[0].artifact_port, &wasm_path, &sha256).await?;

    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        fixture.nodes[0].artifact_port, sha256
    );

    // Deploy via NATS
    let bus = fixture.connect_bus().await?;
    let config = build_app_config(app_id, 100_000_000, 100, 1);

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config).await?;

    // Add route
    add_route(&bus, host, app_id).await?;

    // Wait for the app to be ready on the first node
    wait_for_app_ready(fixture.nodes[0].proxy_port, host, 60).await?;

    info!(app_id, host, "test app deployed and ready");
    Ok(())
}

/// Deploy a test app without adding a route or waiting for readiness.
///
/// Use this when you need to control the deployment steps individually
/// (e.g., add a route later, or wait for instances on specific nodes).
pub async fn setup_deploy_app_only(
    fixture: &crate::fixture::ClusterFixture,
    app_id: &str,
) -> Result<String, String> {
    let wasm_path = find_hello_axum_wasm()?;
    let sha256 = sha256_file(&wasm_path)?;
    let size_bytes = std::fs::metadata(&wasm_path)
        .map_err(|e| format!("failed to read wasm metadata: {e}"))?
        .len();

    upload_artifact(fixture.nodes[0].artifact_port, &wasm_path, &sha256).await?;

    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        fixture.nodes[0].artifact_port, sha256
    );

    let bus = fixture.connect_bus().await?;
    let config = build_app_config(app_id, 100_000_000, 100, 1);

    deploy_app(
        &bus,
        app_id,
        artifact_url,
        sha256.clone(),
        size_bytes,
        config,
    )
    .await?;

    Ok(sha256)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_bytes_known() {
        // SHA-256 of empty string
        let hash = sha256_bytes(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_bytes_hello() {
        let hash = sha256_bytes(b"hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_sha256_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let hash = sha256_file(&file_path).unwrap();
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_sha256_file_missing() {
        let result = sha256_file(Path::new("/nonexistent/file.wasm"));
        assert!(result.is_err());
    }

    #[test]
    fn test_build_app_config() {
        let config = build_app_config("test-app:v1", 100_000_000, 100, 1);
        assert_eq!(config.id.0, "test-app:v1");
        assert_eq!(config.fuel_quota.0, 100_000_000);
        assert_eq!(config.memory_limit.0, 100);
        assert_eq!(config.max_instances, 1);
        assert!(config.env_vars.is_empty());
        assert!(config.secret_keys.is_empty());
    }

    #[test]
    fn test_check_tcp_refused() {
        // Port 1 is almost certainly not listening
        assert!(!check_tcp("127.0.0.1", 1));
    }

    #[tokio::test]
    async fn test_wait_for_tcp_timeout() {
        let result = wait_for_tcp("127.0.0.1", 1, Duration::from_millis(100)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not reachable"));
    }

    #[tokio::test]
    async fn test_retry_success_first_try() {
        let result = retry("test", 3, Duration::from_millis(10), || async { Ok(42u64) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_success_after_failures() {
        let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempt_clone = attempt.clone();

        let result = retry("test", 3, Duration::from_millis(10), move || {
            let a = attempt_clone.clone();
            async move {
                let n = a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err(format!("attempt {n} failed"))
                } else {
                    Ok(n)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_retry_all_fail() {
        let result: Result<u64, String> = retry("test", 2, Duration::from_millis(10), || async {
            Err::<u64, _>("always fails".to_string())
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("all 2 retries failed"));
    }

    #[test]
    fn test_find_hello_axum_wasm_returns_error_if_missing() {
        // This test just verifies the function doesn't panic when the
        // WASM file isn't available. In CI, the file should be pre-built.
        let result = find_hello_axum_wasm();
        // It may succeed or fail depending on the build environment
        if let Err(e) = &result {
            assert!(e.contains("build") || e.contains("not found"));
        }
    }
}
