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
#[allow(dead_code)]
pub struct NatsContainer {
    pub url: String,
    pub port: u16,
    _container: testcontainers::ContainerAsync<GenericImage>,
}

/// A running PostgreSQL container
#[allow(dead_code)]
pub struct PostgresContainer {
    pub url: String,
    pub port: u16,
    _container: testcontainers::ContainerAsync<GenericImage>,
}

/// A simple HTTP file server for serving WASM artifacts
pub struct FileServer {
    pub url: String,
}

impl NatsContainer {
    /// Start a NATS container with JetStream enabled
    pub async fn start(port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        setup_container_runtime();

        // Use host networking so the container shares the host's network
        // namespace. This is critical for Podman rootless mode where port
        // mappings are only reachable from the process that created the
        // container (slirp4netns limitation). With --network=host, NATS
        // binds directly to the host port and any child process can connect.
        // We pass --port to NATS so it listens on the requested host port
        // instead of the default 4222.
        // We use `with_host_config_modifier` to set `network_mode = "host"`
        // directly on the Docker/Podman HostConfig, because `with_network("host")`
        // would try to *create* a Docker network named "host" which conflicts
        // with the reserved network mode.
        let image = GenericImage::new("nats", "2.10-alpine")
            .with_host_config_modifier(|config| {
                config.network_mode = Some("host".to_string());
            })
            .with_cmd(vec!["-js", "--port", &port.to_string()]);

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

#[allow(dead_code)]
impl PostgresContainer {
    /// Start a PostgreSQL container
    pub async fn start(port: u16, password: &str) -> Result<Self, Box<dyn std::error::Error>> {
        setup_container_runtime();

        let image = GenericImage::new("postgres", "17-alpine")
            .with_mapped_port(port, ContainerPort::Tcp(5432))
            .with_env_var("POSTGRES_PASSWORD", password)
            .with_env_var("POSTGRES_USER", "postgres")
            .with_env_var("POSTGRES_DB", "postgres");

        let container = image.start().await?;

        // Wait for PostgreSQL to be ready
        sleep(Duration::from_secs(3)).await;

        let url = format!(
            "postgres://postgres:{}@127.0.0.1:{}/postgres",
            password, port
        );

        Ok(PostgresContainer {
            url,
            port,
            _container: container,
        })
    }
}

#[allow(dead_code)]
impl FileServer {
    /// Start embedded HTTP file server using tokio
    pub async fn start(port: u16, wasm_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // Read WASM file
        let wasm_bytes = std::fs::read(wasm_path)?;
        let wasm_filename = wasm_path.file_name().unwrap().to_str().unwrap().to_string();

        // Start simple HTTP server in background
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

        tokio::spawn(async move {
            use axum::{response::IntoResponse, routing::get, Router};

            let wasm_data = wasm_bytes.clone();
            let filename_clone = wasm_filename.clone();

            let app = Router::new().route(
                &format!("/{}", filename_clone),
                get(move || async move {
                    ([("content-type", "application/wasm")], wasm_data.clone()).into_response()
                }),
            );

            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });

        // Wait for server to start
        sleep(Duration::from_millis(500)).await;

        let url = format!("http://127.0.0.1:{}", port);
        eprintln!("✓ File server ready at {}", url);

        Ok(FileServer { url })
    }

    /// Get URL for the WASM file
    pub fn wasm_url(&self, filename: &str) -> String {
        format!("{}/{}", self.url, filename)
    }
}

/// A running wasm-node process
#[allow(dead_code)]
pub struct NodeProcess {
    pub node_id: String,
    pub proxy_port: u16,
    pub artifact_port: u16,
    pub admin_port: u16,
    pub db_path: PathBuf,
    _temp_dir: TempDir,
    process: Child,
}

impl NodeProcess {
    #[allow(dead_code)]
    /// Start a wasm-node process (use start_with_admin for custom admin port)
    pub async fn start(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_admin(node_id, nats_url, proxy_port, artifact_port, 9190).await
    }

    /// Start a wasm-node process with custom admin port
    pub async fn start_with_admin(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        admin_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let db_path = temp_dir.path().join("node.db");

        let node_binary = find_node_binary()?;

        eprintln!(
            "Starting node {} (proxy:{}, artifact:{}, admin:{})",
            node_id, proxy_port, artifact_port, admin_port
        );

        let mut process = Command::new(&node_binary)
            .arg("--node-id")
            .arg(node_id)
            .arg("--nats-url")
            .arg(nats_url)
            .arg("--proxy-port")
            .arg(proxy_port.to_string())
            .arg("--proxy-https-port")
            .arg("0")
            .arg("--admin-port")
            .arg(admin_port.to_string())
            .arg("--artifact-port")
            .arg(artifact_port.to_string())
            .arg("--db-path")
            .arg(&db_path)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;

        // Check if process started successfully
        if let Some(status) = process.try_wait()? {
            return Err(format!("Node process exited immediately with status: {}", status).into());
        }

        // Wait for node to start up and all servers (proxy, artifact) to be ready
        // The node needs time to:
        // 1. Initialize the database
        // 2. Start the artifact server
        // 3. Start the proxy server
        // 4. Connect to NATS
        sleep(Duration::from_secs(8)).await;

        eprintln!("✓ Node startup wait complete");

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            proxy_port,
            artifact_port,
            admin_port,
            db_path,
            _temp_dir: temp_dir,
            process,
        })
    }

    /// Start a node with an existing database (for restart tests)
    #[allow(dead_code)]
    pub async fn start_with_db(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        db_path: PathBuf,
        _temp_dir: TempDir,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_db_and_admin(
            node_id,
            nats_url,
            proxy_port,
            artifact_port,
            9190,
            db_path,
            _temp_dir,
        )
        .await
    }

    /// Start a node with existing database and custom admin port
    #[allow(dead_code)]
    pub async fn start_with_db_and_admin(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        admin_port: u16,
        db_path: PathBuf,
        _temp_dir: TempDir,
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
            .arg("--proxy-https-port")
            .arg("0")
            .arg("--admin-port")
            .arg(admin_port.to_string())
            .arg("--artifact-port")
            .arg(artifact_port.to_string())
            .arg("--db-path")
            .arg(&db_path)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;

        sleep(Duration::from_secs(12)).await;
        eprintln!("✓ Node restart complete");

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            proxy_port,
            artifact_port,
            admin_port,
            db_path,
            _temp_dir,
            process,
        })
    }

    /// Extract temp dir and db path (for restart tests)
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
        eprintln!("⚠️ wasm-node not found, building...");
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", "node"])
            .current_dir(workspace_root)
            .status()?;

        if !status.success() {
            return Err("Failed to build wasm-node".into());
        }

        Ok(release_binary)
    }
}

/// Helper to find the hello-axum.wasm test app
#[allow(dead_code)]
pub fn find_hello_axum_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();

    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm");

    // Check if needs rebuild
    let needs_rebuild = if !wasm_path.exists() {
        true
    } else {
        let wasm_modified = std::fs::metadata(&wasm_path)?.modified()?;
        let main_modified =
            std::fs::metadata(workspace_root.join("apps/hello-axum/src/main.rs"))?.modified()?;
        wasm_modified < main_modified
    };

    if needs_rebuild {
        eprintln!("⚠️ Building hello-axum.wasm...");

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
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Build error: {}", stderr);
            return Err(format!("Failed to build hello-axum: {}", stderr).into());
        }

        eprintln!("Build stdout: {}", String::from_utf8_lossy(&output.stdout));
    }

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("Build succeeded but wasm file not found".into())
    }
}

/// Helper to find the echo-service.wasm test app
#[allow(dead_code)]
pub fn find_echo_service_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();

    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/echo-service.wasm");

    // Check if needs rebuild
    let needs_rebuild = if !wasm_path.exists() {
        true
    } else {
        let wasm_modified = std::fs::metadata(&wasm_path)?.modified()?;
        let main_modified =
            std::fs::metadata(workspace_root.join("apps/echo-service/src/main.rs"))?.modified()?;
        wasm_modified < main_modified
    };

    if needs_rebuild {
        eprintln!("⚠️ Building echo-service.wasm...");

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
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Build error: {}", stderr);
            return Err(format!("Failed to build echo-service: {}", stderr).into());
        }

        eprintln!("Build stdout: {}", String::from_utf8_lossy(&output.stdout));
    }

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("Build succeeded but wasm file not found".into())
    }
}

/// Helper to find the postgres-app.wasm test app
#[allow(dead_code)]
pub fn find_postgres_app_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();

    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/postgres-app.wasm");

    let needs_rebuild = if !wasm_path.exists() {
        true
    } else {
        let wasm_modified = std::fs::metadata(&wasm_path)?.modified()?;
        let main_modified =
            std::fs::metadata(workspace_root.join("apps/postgres-app/src/main.rs"))?.modified()?;
        wasm_modified < main_modified
    };

    if needs_rebuild {
        eprintln!("⚠️ Building postgres-app.wasm...");

        let output = std::process::Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "postgres-app",
            ])
            .current_dir(workspace_root)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Build error: {}", stderr);
            return Err(format!("Failed to build postgres-app: {}", stderr).into());
        }

        eprintln!("Build stdout: {}", String::from_utf8_lossy(&output.stdout));
    }

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("Build succeeded but wasm file not found".into())
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
#[allow(dead_code)]
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
        tenant_id: None,
        policy: None,
        namespace: "default".to_string(),
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
        artifact_url: artifact_url.clone(),
        artifact_transfer_manifest: None,
        expected_hash: Some(expected_hash.clone()),
        size_bytes,
    };

    eprintln!("📤 Publishing DeployApp event:");
    eprintln!("   app_id: {}", app_id);
    eprintln!("   artifact_url: {}", artifact_url);
    eprintln!("   expected_hash: {}", expected_hash);
    eprintln!("   subject: {}", event.subject());

    bus.publish(&event).await?;
    eprintln!("✓ DeployApp event published");

    // Wait for deployment to process
    sleep(Duration::from_millis(500)).await;

    Ok(())
}

/// Set a gateway config for an app via NATS
pub async fn set_gateway_config(
    bus: &NatsBus,
    app_id: &str,
    config: common::types::GatewayRouteConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let event = messaging::events::Event::GatewayConfigUpdate {
        app_id: common::types::AppId(app_id.to_string()),
        config,
    };
    bus.publish(&event).await?;
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

/// Remove an app via NATS (triggers instance shutdown and billing record creation)
#[allow(dead_code)]
pub async fn remove_app(bus: &NatsBus, app_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let event = Event::RemoveApp {
        app_id: common::types::AppId(app_id.to_string()),
    };

    bus.publish(&event).await?;

    // Wait for app to be removed
    sleep(Duration::from_secs(1)).await;

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

/// Add a localhost entry to /etc/hosts so *.internal names resolve.
/// This is needed for East-West traffic tests where Wasm apps connect
/// to services via .internal hostnames.
/// Returns true if the entry was added (or already present).
pub fn ensure_hosts_entry(hostname: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let entry = format!("127.0.0.1 {}\n", hostname);
    let hosts_path = std::path::Path::new("/etc/hosts");

    if !hosts_path.exists() {
        return Err("/etc/hosts not found — cannot resolve .internal names".into());
    }

    let contents = std::fs::read_to_string(hosts_path)?;
    if contents.contains(&format!("127.0.0.1 {}", hostname)) {
        return Ok(false); // already present
    }

    // Try to append directly first (may fail without root)
    match std::fs::OpenOptions::new().append(true).open(hosts_path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(entry.as_bytes())?;
            eprintln!("✓ Added {} to /etc/hosts", hostname);
            Ok(true)
        }
        Err(e) => {
            eprintln!(
                "⚠️ Could not write /etc/hosts ({}). Try running with sudo, or add '{}' manually.",
                e,
                entry.trim()
            );
            // Many test environments (CI, containers) cannot modify /etc/hosts.
            // The test apps connect to 127.0.0.1 directly for *.internal names,
            // so this is not fatal. Return Ok so the test can continue.
            Ok(false)
        }
    }
}
