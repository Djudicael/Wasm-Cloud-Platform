#![allow(dead_code)]

use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use common::container_runtime::{
    reserve_host_port, NatsContainer as HostNatsContainer,
    PostgresContainer as HostPostgresContainer,
};
/// E2E Test Harness
///
/// This module provides utilities for running end-to-end tests:
/// - Starting/stopping NATS containers
/// - Starting/stopping wasm-node processes
/// - Deploying applications
/// - Sending HTTP requests
use messaging::{events::Event, NatsBus};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

pub fn reserve_test_port() -> Result<u16, Box<dyn std::error::Error>> {
    reserve_host_port().map_err(|e| e.into())
}

/// A running NATS container
#[allow(dead_code)]
pub struct NatsContainer {
    pub url: String,
    pub port: u16,
    _container: HostNatsContainer,
}

/// A running PostgreSQL container
#[allow(dead_code)]
pub struct PostgresContainer {
    pub url: String,
    pub port: u16,
    _container: HostPostgresContainer,
}

/// A simple HTTP file server for serving WASM artifacts
pub struct FileServer {
    pub url: String,
}

impl NatsContainer {
    /// Start a NATS container with JetStream enabled
    pub async fn start(port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let container = HostNatsContainer::start(port)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        let url = container.url.clone();

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
        let container = HostPostgresContainer::start(port, password)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        let url = container.url.clone();

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
    pub deploy_port: u16,
    pub db_path: PathBuf,
    _temp_dir: TempDir,
    process: Child,
}

#[allow(dead_code)]
pub struct DeployIngressProcess {
    pub ingress_id: String,
    pub deploy_port: u16,
    pub artifact_port: u16,
    pub db_path: PathBuf,
    pub audit_path: PathBuf,
    _temp_dir: TempDir,
    process: Child,
}

fn advertised_node_host() -> String {
    static HOSTNAME: OnceLock<String> = OnceLock::new();
    HOSTNAME
        .get_or_init(|| {
            let ip_output = Command::new("sh")
                .arg("-c")
                .arg("hostname -I 2>/dev/null")
                .output()
                .ok();

            let non_loopback_ip = ip_output
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .and_then(|value| {
                    value
                        .split_whitespace()
                        .find(|candidate| {
                            candidate
                                .parse::<std::net::IpAddr>()
                                .ok()
                                .is_some_and(|ip| {
                                    !ip.is_loopback() && matches!(ip, std::net::IpAddr::V4(_))
                                })
                        })
                        .map(str::to_string)
                });

            non_loopback_ip.unwrap_or_else(|| {
                let output = Command::new("hostname").output().ok();
                output
                    .and_then(|out| String::from_utf8(out.stdout).ok())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "localhost".to_string())
            })
        })
        .clone()
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
        let admin_port = reserve_test_port()?;
        Self::start_with_admin(node_id, nats_url, proxy_port, artifact_port, admin_port).await
    }

    /// Start a wasm-node process with custom admin port
    pub async fn start_with_admin(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        admin_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_admin_and_options(
            node_id,
            nats_url,
            proxy_port,
            artifact_port,
            admin_port,
            &[],
            &[],
        )
        .await
    }

    /// Start a wasm-node process with custom admin port and extra CLI/env options.
    pub async fn start_with_admin_and_options(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        admin_port: u16,
        extra_args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let deploy_port = reserve_test_port()?;
        let temp_dir = tempfile::tempdir()?;
        let db_path = temp_dir.path().join("node.db");
        let process = Self::spawn_process(
            node_id,
            nats_url,
            proxy_port,
            artifact_port,
            admin_port,
            deploy_port,
            &db_path,
            extra_args,
            extra_env,
        )?;
        wait_for_node_ready(admin_port, proxy_port, artifact_port).await?;

        eprintln!("✓ Node startup wait complete");

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            proxy_port,
            artifact_port,
            admin_port,
            deploy_port,
            db_path,
            _temp_dir: temp_dir,
            process,
        })
    }

    fn spawn_process(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        admin_port: u16,
        deploy_port: u16,
        db_path: &Path,
        extra_args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<Child, Box<dyn std::error::Error>> {
        let advertised_host = advertised_node_host();

        let node_binary = find_node_binary()?;

        eprintln!(
            "Starting node {} (proxy:{}, artifact:{}, admin:{})",
            node_id, proxy_port, artifact_port, admin_port
        );

        let mut command = Command::new(&node_binary);
        command
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
            .arg("--deploy-ingress-port")
            .arg(deploy_port.to_string())
            .arg("--admin-advertised-host")
            .arg(&advertised_host)
            .arg("--db-path")
            .arg(db_path)
            .args(extra_args)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        for (key, value) in extra_env {
            command.env(key, value);
        }

        let mut process = command.spawn()?;

        // Check if process started successfully
        if let Some(status) = process.try_wait()? {
            return Err(format!("Node process exited immediately with status: {}", status).into());
        }
        Ok(process)
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
        Self::start_with_db_and_admin_and_options(
            node_id,
            nats_url,
            proxy_port,
            artifact_port,
            admin_port,
            db_path,
            _temp_dir,
            &[],
            &[],
        )
        .await
    }

    /// Start a node with existing database, custom admin port, and extra CLI/env options.
    pub async fn start_with_db_and_admin_and_options(
        node_id: &str,
        nats_url: &str,
        proxy_port: u16,
        artifact_port: u16,
        admin_port: u16,
        db_path: PathBuf,
        _temp_dir: TempDir,
        extra_args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let deploy_port = reserve_test_port()?;
        eprintln!("Restarting node {} with existing DB", node_id);

        let process = Self::spawn_process(
            node_id,
            nats_url,
            proxy_port,
            artifact_port,
            admin_port,
            deploy_port,
            &db_path,
            extra_args,
            extra_env,
        )?;

        wait_for_node_ready(admin_port, proxy_port, artifact_port).await?;
        eprintln!("✓ Node restart complete");

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            proxy_port,
            artifact_port,
            admin_port,
            deploy_port,
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

impl DeployIngressProcess {
    pub async fn start(
        ingress_id: &str,
        nats_url: &str,
        deploy_port: u16,
        artifact_port: u16,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_env(ingress_id, nats_url, deploy_port, artifact_port, &[]).await
    }

    pub async fn start_with_env(
        ingress_id: &str,
        nats_url: &str,
        deploy_port: u16,
        artifact_port: u16,
        extra_env: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let db_path = temp_dir.path().join("deploy-ingress.db");
        let audit_path = temp_dir.path().join("audit.jsonl");
        let ingress_binary = find_deploy_ingress_binary()?;
        let advertised_artifact_url = format!("http://127.0.0.1:{artifact_port}");

        eprintln!(
            "Starting deploy ingress {} (deploy:{}, artifact:{})",
            ingress_id, deploy_port, artifact_port
        );

        let mut process = Command::new(&ingress_binary);
        process
            .arg("--ingress-id")
            .arg(ingress_id)
            .arg("--nats-url")
            .arg(nats_url)
            .arg("--deploy-port")
            .arg(deploy_port.to_string())
            .arg("--artifact-port")
            .arg(artifact_port.to_string())
            .arg("--advertised-artifact-url")
            .arg(&advertised_artifact_url)
            .arg("--db-path")
            .arg(&db_path)
            .env(
                "WASM_DEPLOY_INGRESS_AUDIT_PATH",
                audit_path.to_string_lossy().to_string(),
            )
            .env("RUST_LOG", "info")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        for (key, value) in extra_env {
            process.env(key, value);
        }

        let mut process = process.spawn()?;

        if let Some(status) = process.try_wait()? {
            return Err(format!(
                "Deploy ingress process exited immediately with status: {}",
                status
            )
            .into());
        }

        wait_for_health(deploy_port, Duration::from_secs(60)).await?;
        wait_for_tcp(artifact_port, Duration::from_secs(10)).await?;

        Ok(DeployIngressProcess {
            ingress_id: ingress_id.to_string(),
            deploy_port,
            artifact_port,
            db_path,
            audit_path,
            _temp_dir: temp_dir,
            process,
        })
    }

    pub fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("Stopping deploy ingress {}", self.ingress_id);
        self.process.kill()?;
        self.process.wait()?;
        Ok(())
    }
}

async fn wait_for_node_ready(
    admin_port: u16,
    proxy_port: u16,
    artifact_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    wait_for_health(admin_port, Duration::from_secs(60)).await?;
    wait_for_tcp(proxy_port, Duration::from_secs(10)).await?;
    wait_for_tcp(artifact_port, Duration::from_secs(10)).await?;
    Ok(())
}

async fn wait_for_health(
    admin_port: u16,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let deadline = tokio::time::Instant::now() + timeout;
    let url = format!("http://127.0.0.1:{admin_port}/health");

    loop {
        match client.get(&url).send().await {
            Ok(response) if response.status() == StatusCode::OK => return Ok(()),
            _ if tokio::time::Instant::now() >= deadline => {
                return Err(format!("health endpoint did not become ready: {url}").into());
            }
            _ => sleep(Duration::from_millis(200)).await,
        }
    }
}

async fn wait_for_tcp(port: u16, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(_) => return Ok(()),
            Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(format!("tcp listener not ready on 127.0.0.1:{port}").into());
            }
            Err(_) => sleep(Duration::from_millis(100)).await,
        }
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

impl Drop for DeployIngressProcess {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

/// Helper to find the wasm-node binary
fn find_node_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();
    let debug_binary = workspace_root.join("target/debug/wasm-node");
    ensure_helper_binary_built_once("wasm-node", workspace_root, &["build", "-p", "node"])?;
    Ok(debug_binary)
}

/// Helper to find the wasm-ctl binary
pub fn find_ctl_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();
    let debug_binary = workspace_root.join("target/debug/wasm-ctl");
    ensure_helper_binary_built_once("wasm-ctl", workspace_root, &["build", "-p", "ctl"])?;
    Ok(debug_binary)
}

fn find_deploy_ingress_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();
    let debug_binary = workspace_root.join("target/debug/wasm-deploy-ingress");
    ensure_helper_binary_built_once(
        "wasm-deploy-ingress",
        workspace_root,
        &["build", "-p", "deploy-ingress"],
    )?;
    Ok(debug_binary)
}

fn ensure_helper_binary_built_once(
    binary_name: &'static str,
    workspace_root: &Path,
    cargo_args: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    static NODE_BUILD: OnceLock<Result<(), String>> = OnceLock::new();
    static CTL_BUILD: OnceLock<Result<(), String>> = OnceLock::new();
    static DEPLOY_INGRESS_BUILD: OnceLock<Result<(), String>> = OnceLock::new();

    let cell = match binary_name {
        "wasm-node" => &NODE_BUILD,
        "wasm-ctl" => &CTL_BUILD,
        "wasm-deploy-ingress" => &DEPLOY_INGRESS_BUILD,
        _ => return Err(format!("unsupported helper binary: {binary_name}").into()),
    };

    let result = cell.get_or_init(|| {
        eprintln!("Building {}...", binary_name);
        match std::process::Command::new("cargo")
            .args(cargo_args)
            .current_dir(workspace_root)
            .status()
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("failed to build {}: {}", binary_name, status)),
            Err(err) => Err(format!(
                "failed to spawn cargo build for {}: {}",
                binary_name, err
            )),
        }
    });

    result
        .as_ref()
        .map(|_| ())
        .map_err(|err| -> Box<dyn std::error::Error> { err.clone().into() })
}

fn ensure_wasm_package_built_once(
    package_name: &'static str,
    workspace_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    static HELLO_AXUM_BUILD: OnceLock<Result<(), String>> = OnceLock::new();
    static ECHO_SERVICE_BUILD: OnceLock<Result<(), String>> = OnceLock::new();
    static POSTGRES_APP_BUILD: OnceLock<Result<(), String>> = OnceLock::new();
    static HTTP_HELLO_COMPONENT_BUILD: OnceLock<Result<(), String>> = OnceLock::new();
    static WASI_GRPC_ECHO_BUILD: OnceLock<Result<(), String>> = OnceLock::new();

    let cell = match package_name {
        "hello-axum" => &HELLO_AXUM_BUILD,
        "echo-service" => &ECHO_SERVICE_BUILD,
        "postgres-app" => &POSTGRES_APP_BUILD,
        "http-hello-component" => &HTTP_HELLO_COMPONENT_BUILD,
        "wasi-grpc-echo" => &WASI_GRPC_ECHO_BUILD,
        _ => return Err(format!("unsupported wasm package: {package_name}").into()),
    };

    let result = cell.get_or_init(|| {
        eprintln!("Building {}.wasm...", package_name);
        match std::process::Command::new("cargo")
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                package_name,
            ])
            .current_dir(workspace_root)
            .status()
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("failed to build {}: {}", package_name, status)),
            Err(err) => Err(format!(
                "failed to spawn cargo build for {}: {}",
                package_name, err
            )),
        }
    });

    result
        .as_ref()
        .map(|_| ())
        .map_err(|err| -> Box<dyn std::error::Error> { err.clone().into() })
}

pub fn run_ctl(
    args: &[&str],
    nats_url: &str,
    node_api: &str,
    deploy_api: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let ctl_binary = find_ctl_binary()?;
    let output = Command::new(&ctl_binary)
        .args(args)
        .env("WASM_CTL_NATS_URL", nats_url)
        .env("WASM_CTL_NODE_API", node_api)
        .env("WASM_CTL_DEPLOY_API", deploy_api)
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "wasm-ctl failed with status {}.\nstdout:\n{}\nstderr:\n{}",
        output.status, stdout, stderr
    )
    .into())
}

pub async fn run_ctl_async(
    args: Vec<String>,
    nats_url: String,
    node_api: String,
    deploy_api: String,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_ctl(&arg_refs, &nats_url, &node_api, &deploy_api).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("failed to join wasm-ctl task: {e}"))?
}

/// Helper to find the hello-axum.wasm test app
#[allow(dead_code)]
pub fn find_hello_axum_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();
    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm");
    ensure_wasm_package_built_once("hello-axum", workspace_root)?;

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
    ensure_wasm_package_built_once("echo-service", workspace_root)?;

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
    ensure_wasm_package_built_once("postgres-app", workspace_root)?;

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("Build succeeded but wasm file not found".into())
    }
}

/// Helper to find the http-hello-component.wasm test app
#[allow(dead_code)]
pub fn find_http_hello_component_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();
    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/http_hello_component.wasm");
    ensure_wasm_package_built_once("http-hello-component", workspace_root)?;

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("Build succeeded but http-hello-component.wasm not found".into())
    }
}

pub fn find_wasi_grpc_echo_wasm() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap().parent().unwrap();
    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/wasi_grpc_echo.wasm");
    ensure_wasm_package_built_once("wasi-grpc-echo", workspace_root)?;

    if wasm_path.exists() {
        Ok(wasm_path)
    } else {
        Err("Build succeeded but wasi-grpc-echo.wasm not found".into())
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

pub async fn upload_and_authorize_artifact_for_node(
    node: &NodeProcess,
    wasm_path: &Path,
) -> Result<
    (
        String,
        String,
        u64,
        Vec<common::artifact_transfer::ArtifactManifestAudienceBinding>,
    ),
    Box<dyn std::error::Error>,
> {
    let sha256 = compute_sha256(wasm_path)?;
    let size_bytes = std::fs::metadata(wasm_path)?.len();

    upload_artifact(node.artifact_port, wasm_path, &sha256).await?;

    let manifests =
        authorize_artifact_for_audiences(node, &sha256, std::slice::from_ref(&node.node_id))
            .await?;

    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    Ok((artifact_url, sha256, size_bytes, manifests))
}

pub async fn authorize_artifact_for_audiences(
    node: &NodeProcess,
    sha256: &str,
    audiences: &[String],
) -> Result<
    Vec<common::artifact_transfer::ArtifactManifestAudienceBinding>,
    Box<dyn std::error::Error>,
> {
    let authorize_url = format!(
        "http://127.0.0.1:{}/artifacts/{}/authorize",
        node.artifact_port, sha256
    );

    let response = reqwest::Client::new()
        .post(&authorize_url)
        .json(&ArtifactManifestBatchRequest {
            audiences: audiences.to_vec(),
        })
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!(
            "artifact authorize failed: HTTP {} from {}",
            response.status(),
            authorize_url
        )
        .into());
    }

    Ok(response
        .json::<ArtifactManifestBatchResponse>()
        .await?
        .manifests)
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
    deploy_app_with_manifests(
        bus,
        app_id,
        artifact_url,
        expected_hash,
        size_bytes,
        config,
        Vec::new(),
    )
    .await
}

pub async fn deploy_app_with_manifests(
    bus: &NatsBus,
    app_id: &str,
    artifact_url: String,
    expected_hash: String,
    size_bytes: u64,
    config: common::types::AppConfig,
    artifact_transfer_manifests: Vec<common::artifact_transfer::ArtifactManifestAudienceBinding>,
) -> Result<(), Box<dyn std::error::Error>> {
    let event = Event::DeployApp {
        app_id: common::types::AppId(app_id.to_string()),
        config,
        artifact_url: artifact_url.clone(),
        artifact_transfer_manifests,
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
