/// E2E Test Harness
///
/// This module provides utilities for running end-to-end tests:
/// - Starting/stopping NATS containers
/// - Starting/stopping wasm-node processes
/// - Deploying applications
/// - Sending HTTP requests
use messaging::{events::Event, NatsBus};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use testcontainers::{core::ContainerPort, runners::AsyncRunner, GenericImage, ImageExt};
use tokio::time::sleep;

/// Configure Podman socket if available (for WSL users)
pub fn setup_container_runtime() {
    if std::env::var("DOCKER_HOST").is_ok() {
        return;
    }

    let podman_socket = std::path::Path::new("/run/user/1000/podman/podman.sock");
    if podman_socket.exists() {
        std::env::set_var("DOCKER_HOST", "unix:///run/user/1000/podman/podman.sock");
        eprintln!("✓ Configured testcontainers to use Podman");
    }

    if std::env::var("TESTCONTAINERS_RYUK_DISABLED").is_err() {
        std::env::set_var("TESTCONTAINERS_RYUK_DISABLED", "true");
    }
}

/// A running NATS container
pub struct NatsContainer {
    pub url: String,
    pub port: u16,
    _container: testcontainers::ContainerAsync<GenericImage>,
}

impl NatsContainer {
    /// Start a NATS container with JetStream enabled
    pub async fn start(port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        setup_container_runtime();

        let image = GenericImage::new("nats", "2.10-alpine")
            .with_mapped_port(port, ContainerPort::Tcp(4222))
            .with_cmd(vec!["-js"]);

        let container = image.start().await?;

        // Wait for NATS to be ready
        sleep(Duration::from_secs(2)).await;

        let url = format!("nats://127.0.0.1:{}", port);

        Ok(NatsContainer {
            url,
            port,
            _container: container,
        })
    }

    /// Connect to this NATS instance
    pub async fn connect(&self) -> Result<NatsBus, Box<dyn std::error::Error>> {
        Ok(NatsBus::connect(&self.url).await?)
    }
}

/// A running wasm-node process
pub struct NodeProcess {
    pub node_id: String,
    pub proxy_port: u16,
    pub artifact_port: u16,
    pub db_path: PathBuf,
    _temp_dir: TempDir,
    process: Child,
}

impl NodeProcess {
    /// Start a wasm-node process
    pub async fn start(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let db_path = temp_dir.path().join("node.db");

        let node_binary = find_node_binary()?;

        eprintln!(
            "Starting node {} (proxy:{}, artifact:{})",
            node_id, proxy_port, artifact_port
        );

        let process = Command::new(&node_binary)
            .arg("--node-id")
            .arg(node_id)
            .arg("--nats-url")
            .arg(nats_url)
            .arg("--proxy-port")
            .arg(proxy_port.to_string())
            .arg("--artifact-server-port")
            .arg(artifact_port.to_string())
            .arg("--db-path")
            .arg(&db_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Wait for node to start up
        sleep(Duration::from_secs(3)).await;

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            proxy_port,
            artifact_port,
            db_path,
            _temp_dir: temp_dir,
            process,
        })
    }

    /// Start a node with an existing database (for restart tests)
    pub async fn start_with_db(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        db_path: PathBuf,
        temp_dir: TempDir,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let node_binary = find_node_binary()?;

        eprintln!("Restarting node {} with existing DB", node_id);

        let process = Command::new(&node_binary)
            .arg("--node-id")
            .arg(node_id)
            .arg("--nats-url")
            .arg(nats_url)
            .arg("--proxy-port")
            .arg(proxy_port.to_string())
            .arg("--artifact-server-port")
            .arg(artifact_port.to_string())
            .arg("--db-path")
            .arg(&db_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        sleep(Duration::from_secs(3)).await;

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            proxy_port,
            artifact_port,
            db_path,
            _temp_dir: temp_dir,
            process,
        })
    }

    /// Extract temp dir and db path (for restart tests)
    pub fn extract_db(mut self) -> (PathBuf, TempDir) {
        // Kill process first to avoid Drop interference
        let _ = self.process.kill();
        let _ = self.process.wait();

        // Use take to move out of self
        let db_path = std::mem::take(&mut self.db_path);
        let temp_dir = std::mem::replace(&mut self._temp_dir, tempfile::tempdir().unwrap());

        std::mem::forget(self); // Prevent Drop from running
        (db_path, temp_dir)
    }

    /// Stop the node gracefully
    pub fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("Stopping node {}", self.node_id);
        self.process.kill()?;
        self.process.wait()?;
        Ok(())
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

/// Helper to find the wasm-node binary
fn find_node_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();

    let debug_binary = workspace_root.join("target/debug/wasm-node");
    let release_binary = workspace_root.join("target/release/wasm-node");

    if release_binary.exists() {
        Ok(release_binary)
    } else if debug_binary.exists() {
        Ok(debug_binary)
    } else {
        Err("wasm-node binary not found. Build it with: cargo build --bin wasm-node".into())
    }
}

/// Helper to find the hello-axum.wasm test app
pub fn find_hello_axum_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();

    // Try both hello-axum.wasm and hello_axum.wasm
    let wasm_path_dash = workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm");
    let wasm_path_underscore = workspace_root.join("target/wasm32-wasip2/release/hello_axum.wasm");

    if wasm_path_dash.exists() {
        Ok(wasm_path_dash)
    } else if wasm_path_underscore.exists() {
        Ok(wasm_path_underscore)
    } else {
        Err(
            "hello-axum.wasm not found. Build it with: \
             RUSTFLAGS='--cfg tokio_unstable' cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release"
                .into(),
        )
    }
}

/// Compute SHA-256 hash of a file
pub fn compute_sha256(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let hash = Sha256::digest(&bytes);
    Ok(hex::encode(hash))
}

/// Upload a WASM artifact to a node's artifact server
pub async fn upload_artifact(
    artifact_port: u16,
    wasm_path: &Path,
    sha256: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/artifacts/{}", artifact_port, sha256);

    let wasm_bytes = std::fs::read(wasm_path)?;

    let response = client
        .put(&url)
        .header("content-type", "application/wasm")
        .body(wasm_bytes)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("Failed to upload artifact: {}", response.status()).into());
    }

    Ok(())
}

/// Build a default AppConfig for testing
pub fn build_app_config(
    app_id: &str,
    fuel_quota: u64,
    memory_pages: u32,
    max_instances: u32,
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
    }
}

/// Deploy an app via NATS
pub async fn deploy_app(
    bus: &NatsBus,
    app_id: &str,
    artifact_url: String,
    expected_hash: String,
    size_bytes: u64,
    config: common::types::AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let event = Event::DeployApp {
        app_id: common::types::AppId(app_id.to_string()),
        config,
        artifact_url,
        expected_hash: Some(expected_hash),
        size_bytes,
    };

    bus.publish(&event).await?;

    // Wait for deployment to process
    sleep(Duration::from_millis(500)).await;

    Ok(())
}

/// Add a route via NATS
pub async fn add_route(
    bus: &NatsBus,
    host: &str,
    app_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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

    let event = Event::RouteAdd { route };

    bus.publish(&event).await?;

    // Wait for route to be added
    sleep(Duration::from_millis(200)).await;

    Ok(())
}

/// Send an HTTP request to the proxy
pub async fn send_request(
    proxy_port: u16,
    host: &str,
    path: &str,
) -> Result<reqwest::Response, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}{}", proxy_port, path);

    let response = client
        .get(&url)
        .header("host", host)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;

    Ok(response)
}

/// Wait for an app to be ready by sending requests until one succeeds
pub async fn wait_for_app_ready(
    proxy_port: u16,
    host: &str,
    max_attempts: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    for i in 0..max_attempts {
        match send_request(proxy_port, host, "/").await {
            Ok(resp) if resp.status().is_success() => {
                eprintln!("✓ App ready after {} attempts", i + 1);
                return Ok(());
            }
            _ => {
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    Err("App did not become ready in time".into())
}
