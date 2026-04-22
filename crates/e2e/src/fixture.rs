//! Cluster fixture for chaos testing.
//!
//! Provides `NodeProcess` for managing a single wasm-node process and
//! `ClusterFixture` for orchestrating a complete test cluster (NATS + N nodes).
//!
//! All process management uses Unix APIs (`SIGKILL`, `SIGTERM`) and therefore
//! **must run inside WSL or on a native Linux host**.

use messaging::NatsBus;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

use tokio::time::sleep;
use tracing::{info, warn};

/// Configure Podman socket if available (for WSL users).
///
/// Detects the current UID dynamically so it works for any user,
/// not just UID 1000.
pub fn setup_container_runtime() {
    if std::env::var("DOCKER_HOST").is_ok() {
        return;
    }

    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        let socket_path = format!("/run/user/{uid}/podman/podman.sock");
        let podman_socket = Path::new(&socket_path);
        if podman_socket.exists() {
            std::env::set_var("DOCKER_HOST", format!("unix://{socket_path}"));
            info!("Configured testcontainers to use Podman (uid={uid})");
        }
    }

    if std::env::var("TESTCONTAINERS_RYUK_DISABLED").is_err() {
        std::env::set_var("TESTCONTAINERS_RYUK_DISABLED", "true");
    }
}

// ── Port allocation ──────────────────────────────────────────────────

/// Base port offsets for chaos test clusters.
/// Each cluster gets a unique base to avoid collisions when tests run sequentially.
const NATS_PORT_BASE: u16 = 14250;
const PROXY_PORT_BASE: u16 = 18080;
const ADMIN_PORT_BASE: u16 = 19090;
const ARTIFACT_PORT_BASE: u16 = 19100;

static CLUSTER_COUNTER: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// Allocate a unique port range for a cluster instance.
fn allocate_port_base() -> u16 {
    let idx = CLUSTER_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    idx * 350 // 350 ports per cluster (enough for 3 nodes * 101 WASI ports + admin/proxy/artifact ports)
}

// ── NodeProcess ──────────────────────────────────────────────────────

/// A running wasm-node process managed by the test harness.
///
/// Wraps a `std::process::Child` with chaos-specific operations:
/// - `kill()` — hard kill (`SIGKILL`), simulates OOM / hardware fault
/// - `terminate()` — graceful shutdown (`SIGTERM`)
/// - `restart()` — kill + re-spawn with the same configuration
/// - `is_running()` — non-blocking liveness check
pub struct NodeProcess {
    pub node_id: String,
    pub process: Child,
    pub admin_addr: SocketAddr,
    pub proxy_addr: SocketAddr,
    pub proxy_port: u16,
    pub admin_port: u16,
    pub artifact_port: u16,
    pub db_path: PathBuf,
    pub config_path: PathBuf,
    _temp_dir: TempDir,
    nats_url: String,
    port_start: u16,
    port_end: u16,
}

impl NodeProcess {
    /// Start a wasm-node process with the given configuration.
    ///
    /// The process is spawned with `Stdio::piped()` so that output does not
    /// clutter the test runner's stdout (unless `RUST_LOG=debug` is set, in
    /// which case the caller may switch to `Stdio::inherit()`).
    pub async fn start(
        node_id: &str,
        nats_url: &str,
        admin_port: u16,
        proxy_port: u16,
        artifact_port: u16,
        port_start: u16,
        port_end: u16,
    ) -> Result<Self, String> {
        info!(
            node_id,
            admin_port,
            proxy_port,
            artifact_port,
            port_start,
            port_end,
            nats_url,
            "NodeProcess::start called"
        );

        let temp_dir =
            tempfile::tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;
        let db_path = temp_dir.path().join(format!("chaos_{node_id}.redb"));
        let config_path = temp_dir.path().join(format!("chaos_{node_id}.toml"));

        info!(node_id, config_path = %config_path.display(), db_path = %db_path.display(), "node paths configured");

        let config_content = format!(
            r#"
[node]
node_id = "{node_id}"

[storage]
db_path = "{}"

[nats]
url = "{nats_url}"

[proxy]
http_port = {proxy_port}
https_port = 0

[admin]
port = {admin_port}

[artifact]
port = {artifact_port}

[logging]
level = "debug"

[health]
check_interval_secs = 2
"#,
            db_path.display()
        );
        std::fs::write(&config_path, &config_content)
            .map_err(|e| format!("failed to write config: {e}"))?;

        let binary_path = std::env::var("WASM_NODE_BINARY").unwrap_or_else(|_| find_node_binary());

        info!(
            node_id,
            admin_port, proxy_port, artifact_port, "starting wasm-node"
        );

        info!(node_id, binary = %binary_path, "spawning wasm-node process");

        // Verify NATS is still reachable right before spawning the node.
        // The container may have been reaped between ClusterFixture setup
        // and this point, especially under Podman rootless mode.
        if let Some(nats_port) = nats_url
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
        {
            if let Err(e) =
                crate::helpers::wait_for_tcp("127.0.0.1", nats_port, Duration::from_secs(5)).await
            {
                warn!(nats_url, nats_port, error = %e,
                    "NATS port not reachable before spawning node — container may have died");
            } else {
                info!(nats_port, "NATS port verified reachable before node spawn");
            }
        }

        let mut process = Command::new(&binary_path)
            .arg("--config")
            .arg(&config_path)
            .arg("--node-id")
            .arg(node_id)
            .arg("--nats-url")
            .arg(nats_url)
            .arg("--db-path")
            .arg(&db_path)
            .arg("--proxy-port")
            .arg(proxy_port.to_string())
            .arg("--proxy-https-port")
            .arg("0")
            .arg("--admin-port")
            .arg(admin_port.to_string())
            .arg("--artifact-port")
            .arg(artifact_port.to_string())
            .arg("--port-start")
            .arg(port_start.to_string())
            .arg("--port-end")
            .arg(port_end.to_string())
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to start wasm-node: {e}"))?;

        info!(node_id, pid = process.id(), "wasm-node process spawned");

        let admin_addr: SocketAddr = format!("127.0.0.1:{admin_port}")
            .parse()
            .map_err(|e| format!("invalid admin addr: {e}"))?;
        let proxy_addr: SocketAddr = format!("127.0.0.1:{proxy_port}")
            .parse()
            .map_err(|e| format!("invalid proxy addr: {e}"))?;

        // Wait for the node to become ready
        info!(node_id, admin_port, "waiting for node health endpoint");
        crate::helpers::wait_for_health(
            &format!("127.0.0.1:{admin_port}"),
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| {
            // Check if the process is still running — if not, it crashed
            let status_note = if let Ok(Some(status)) = process.try_wait() {
                format!(" (process exited with {status})")
            } else {
                " (process still running)".to_string()
            };
            format!("node {node_id} did not become healthy on port {admin_port}: {e}{status_note}")
        })?;

        Ok(NodeProcess {
            node_id: node_id.to_string(),
            process,
            admin_addr,
            proxy_addr,
            proxy_port,
            admin_port,
            artifact_port,
            db_path,
            config_path,
            _temp_dir: temp_dir,
            nats_url: nats_url.to_string(),
            port_start,
            port_end,
        })
    }

    /// Send `SIGKILL` to the process (hard kill, no graceful shutdown).
    ///
    /// Simulates an OOM kill or hardware fault. The process has no chance to
    /// run cleanup code — this is the most realistic crash simulation.
    pub fn kill(&mut self) -> Result<(), String> {
        info!(node_id = %self.node_id, "sending SIGKILL to wasm-node");
        self.process
            .kill()
            .map_err(|e| format!("failed to kill process: {e}"))
    }

    /// Send `SIGTERM` to the process (graceful shutdown).
    ///
    /// Allows the node to flush pending writes, close connections, and
    /// persist billing records before exiting.
    pub fn terminate(&mut self) -> Result<(), String> {
        info!(node_id = %self.node_id, "sending SIGTERM to wasm-node");
        #[cfg(unix)]
        {
            let pid = self.process.id() as i32;
            // SAFETY: libc::kill is a well-known POSIX API. We are sending
            // SIGTERM to our own child process.
            let ret = unsafe { libc::kill(pid, libc::SIGTERM) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(format!("failed to SIGTERM process: {err}"));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            // On non-Unix, fall back to kill (SIGKILL equivalent)
            warn!("SIGTERM not available on this platform, falling back to kill");
            self.kill()
        }
    }

    /// Wait for the process to exit and return the exit status.
    pub fn wait(&mut self) -> Result<std::process::ExitStatus, String> {
        self.process
            .wait()
            .map_err(|e| format!("failed to wait for process: {e}"))
    }

    /// Check if the process is still running (non-blocking).
    pub fn is_running(&mut self) -> bool {
        match self.process.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Restart the process with the same configuration.
    ///
    /// If the process is still running, it is killed first. The same config
    /// file and database path are reused so the node restores state from redb.
    pub async fn restart(&mut self) -> Result<(), String> {
        info!(node_id = %self.node_id, "restarting wasm-node");

        // Kill if still running
        if self.is_running() {
            self.kill()?;
            let _ = self.process.wait();
        }

        let binary_path = std::env::var("WASM_NODE_BINARY").unwrap_or_else(|_| find_node_binary());

        self.process = Command::new(&binary_path)
            .arg("--config")
            .arg(&self.config_path)
            .arg("--node-id")
            .arg(&self.node_id)
            .arg("--nats-url")
            .arg(&self.nats_url)
            .arg("--db-path")
            .arg(&self.db_path)
            .arg("--proxy-port")
            .arg(self.proxy_port.to_string())
            .arg("--proxy-https-port")
            .arg("0")
            .arg("--admin-port")
            .arg(self.admin_port.to_string())
            .arg("--artifact-port")
            .arg(self.artifact_port.to_string())
            .arg("--port-start")
            .arg(self.port_start.to_string())
            .arg("--port-end")
            .arg(self.port_end.to_string())
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to restart wasm-node: {e}"))?;

        // Wait for the restarted node to become healthy
        // Use a longer timeout since the node needs to do integrity checks
        crate::helpers::wait_for_health(
            &format!("127.0.0.1:{}", self.admin_port),
            Duration::from_secs(60),
        )
        .await
        .map_err(|e| {
            format!(
                "restarted node {} did not become healthy: {e}",
                self.node_id
            )
        })
    }

    /// Get the admin address as a string for HTTP requests.
    pub fn admin_addr_str(&self) -> String {
        format!("127.0.0.1:{}", self.admin_port)
    }

    /// Get the proxy address as a string for HTTP requests.
    pub fn proxy_addr_str(&self) -> String {
        format!("127.0.0.1:{}", self.proxy_port)
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.kill();
            let _ = self.process.wait();
        }
        // Clean up the database file (best-effort)
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.config_path);
    }
}

// ── NatsContainer ────────────────────────────────────────────────────

/// A running NATS container managed via `podman run --network=host`.
///
/// Uses host networking directly instead of testcontainers because Podman
/// rootless mode's `slirp4netns` port mappings are only reachable from the
/// process that created the container. With `--network=host`, NATS binds
/// directly to the host port and any child process (like `wasm-node`) can
/// connect.
pub struct NatsContainer {
    pub url: String,
    pub port: u16,
    container_id: String,
}

impl NatsContainer {
    /// Start a NATS container with JetStream enabled on the given host port.
    ///
    /// Runs `podman run -d --network=host` so that the NATS server binds
    /// directly to the host's network namespace. The `--port` flag tells
    /// NATS to listen on the requested port instead of the default 4222.
    pub async fn start(port: u16) -> Result<Self, String> {
        setup_container_runtime();

        info!(port, "starting NATS container on host port");

        let container_name = format!("nats-chaos-{port}");

        // Remove any leftover container with the same name from a previous run
        let _ = Command::new("podman")
            .args(["rm", "-f", &container_name])
            .output();

        // Run NATS directly via podman with --network=host.
        let output = Command::new("podman")
            .args([
                "run",
                "-d",
                "--network=host",
                "--name",
                &container_name,
                "docker.io/library/nats:2.10-alpine",
                "-js",
                "--port",
                &port.to_string(),
            ])
            .output()
            .map_err(|e| format!("failed to run podman: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("podman run failed: {stderr}"));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

        info!(port, %container_id, "NATS container started, waiting for readiness");

        // Wait for NATS to be ready
        sleep(Duration::from_secs(2)).await;

        crate::helpers::wait_for_tcp("127.0.0.1", port, Duration::from_secs(10))
            .await
            .map_err(|e| format!("NATS not reachable on port {port}: {e}"))?;

        // Double-check: verify the port is still listening after a brief pause
        sleep(Duration::from_millis(500)).await;
        if let Err(e) =
            crate::helpers::wait_for_tcp("127.0.0.1", port, Duration::from_secs(5)).await
        {
            warn!(port, error = %e, "NATS port became unreachable shortly after startup — container may have crashed");
        }

        let url = format!("nats://127.0.0.1:{port}");

        info!(port, url, "NATS container ready");

        Ok(NatsContainer {
            url,
            port,
            container_id,
        })
    }

    /// Connect to this NATS instance and return a `NatsBus`.
    pub async fn connect(&self) -> Result<NatsBus, String> {
        let bus = NatsBus::connect(&self.url)
            .await
            .map_err(|e| format!("failed to connect to NATS: {e}"))?;
        Ok(bus)
    }
}

impl Drop for NatsContainer {
    fn drop(&mut self) {
        let container_id = self.container_id.clone();
        match Command::new("podman")
            .args(["rm", "-f", &container_id])
            .output()
        {
            Ok(_) => info!(%container_id, "NATS container removed"),
            Err(e) => warn!(%container_id, error = %e, "failed to remove NATS container"),
        }
    }
}

// ── ClusterFixture ───────────────────────────────────────────────────

/// A complete test cluster with NATS and one or more wasm-node instances.
///
/// Each cluster gets:
/// - A fresh NATS container (isolated JetStream streams)
/// - N wasm-node processes with unique node IDs and non-overlapping ports
/// - Fresh redb files in temp directories (cleaned up on Drop)
///
/// Tests run sequentially (`--test-threads=1`) to avoid port conflicts and
/// resource contention.
pub struct ClusterFixture {
    pub nats_container: NatsContainer,
    pub nats_url: String,
    pub nodes: Vec<NodeProcess>,
    pub http: reqwest::Client,
    #[allow(dead_code)]
    port_base: u16,
}

impl ClusterFixture {
    /// Create a new cluster with NATS and the specified number of nodes.
    ///
    /// Each node gets a unique node ID (`chaos-node-0`, `chaos-node-1`, etc.)
    /// and non-overlapping port ranges derived from an allocated base.
    pub async fn new(node_count: usize) -> Result<Self, String> {
        let port_base = allocate_port_base();

        // Start NATS container
        let nats_port = NATS_PORT_BASE + port_base;
        info!(nats_port, port_base, "starting NATS container for cluster");
        let nats = NatsContainer::start(nats_port).await?;
        let nats_url = nats.url.clone();
        info!(
            nats_url,
            "NATS container started, proceeding with JetStream setup"
        );

        // Setup JetStream
        let bus = nats
            .connect()
            .await
            .map_err(|e| format!("NATS connect failed: {e}"))?;
        bus.setup_jetstream()
            .await
            .map_err(|e| format!("JetStream setup failed: {e}"))?;

        // Verify NATS is still reachable after JetStream setup.
        // The container may have crashed or been reaped between the
        // initial wait_for_tcp and now.
        crate::helpers::wait_for_tcp("127.0.0.1", nats_port, Duration::from_secs(5))
            .await
            .map_err(|e| {
                format!(
                "NATS container on port {nats_port} became unreachable after JetStream setup: {e}. \
                 The container may have crashed — check `podman logs` for details."
            )
            })?;
        info!(nats_port, "NATS still alive after JetStream setup");

        // Start nodes
        let mut nodes = Vec::with_capacity(node_count);
        for i in 0..node_count {
            let node_id = format!("chaos-node-{i}");
            let admin_port = ADMIN_PORT_BASE + port_base + i as u16;
            let proxy_port = PROXY_PORT_BASE + port_base + i as u16;
            let artifact_port = ARTIFACT_PORT_BASE + port_base + i as u16;

            info!(
                node_id,
                admin_port, proxy_port, artifact_port, "starting cluster node"
            );
            // Each node gets a unique WASI port range so they don't collide
            // on the shared host OS. Range size = 101 ports per node.
            let port_start = 10000 + port_base + (i as u16) * 101;
            let port_end = port_start + 100;

            info!(
                node_id,
                admin_port,
                proxy_port,
                artifact_port,
                port_start,
                port_end,
                "starting cluster node"
            );
            let node = NodeProcess::start(
                &node_id,
                &nats_url,
                admin_port,
                proxy_port,
                artifact_port,
                port_start,
                port_end,
            )
            .await?;

            nodes.push(node);
        }

        info!(node_count, "cluster fixture ready");

        Ok(ClusterFixture {
            nats_container: nats,
            nats_url,
            nodes,
            http: reqwest::Client::new(),
            port_base,
        })
    }

    /// Create a single-node cluster (convenience shortcut).
    pub async fn single() -> Result<Self, String> {
        Self::new(1).await
    }

    /// Create a two-node cluster (convenience shortcut for L3/L4 tests).
    pub async fn dual() -> Result<Self, String> {
        Self::new(2).await
    }

    /// Create a three-node cluster (convenience shortcut for L6 tests).
    pub async fn triple() -> Result<Self, String> {
        Self::new(3).await
    }

    /// Get a reference to a specific node by index.
    pub fn node(&self, index: usize) -> &NodeProcess {
        &self.nodes[index]
    }

    /// Get a mutable reference to a specific node by index.
    pub fn node_mut(&mut self, index: usize) -> &mut NodeProcess {
        &mut self.nodes[index]
    }

    /// Connect to the cluster's NATS instance and return a `NatsBus`.
    pub async fn connect_bus(&self) -> Result<NatsBus, String> {
        self.nats_container.connect().await
    }

    /// Deploy a test app to the cluster via NATS.
    pub async fn deploy_app(&self, app_id: &str, wasm_path: &Path) -> Result<(), String> {
        let bus = self.connect_bus().await?;

        // Upload artifact to the first node
        let sha256 = crate::helpers::sha256_file(wasm_path)?;
        let size_bytes = std::fs::metadata(wasm_path)
            .map_err(|e| format!("failed to read wasm metadata: {e}"))?
            .len();

        crate::helpers::upload_artifact(self.nodes[0].artifact_port, wasm_path, &sha256).await?;

        let artifact_url = format!(
            "http://127.0.0.1:{}/artifacts/{}",
            self.nodes[0].artifact_port, sha256
        );

        let config = crate::helpers::build_app_config(app_id, 100_000_000, 100, 1);

        crate::helpers::deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
            .await
            .map_err(|e| format!("deploy failed: {e}"))
    }

    /// Add a route to the cluster via NATS.
    pub async fn add_route(&self, host: &str, app_id: &str) -> Result<(), String> {
        let bus = self.connect_bus().await?;
        crate::helpers::add_route(&bus, host, app_id).await
    }
}

impl Drop for ClusterFixture {
    fn drop(&mut self) {
        // Kill all node processes
        for node in &mut self.nodes {
            if node.is_running() {
                let _ = node.kill();
                let _ = node.process.wait();
            }
            // Clean up database files
            let _ = std::fs::remove_file(&node.db_path);
            let _ = std::fs::remove_file(&node.config_path);
        }
        // NATS container is cleaned up by its own Drop impl (podman rm -f)
        info!("cluster fixture cleaned up");
    }
}

// ── Binary Discovery ─────────────────────────────────────────────────

/// Find the wasm-node binary, building it if necessary.
///
/// Looks in `target/debug/wasm-node` and `target/release/wasm-node` relative
/// to the workspace root. If not found, attempts to build it.
pub fn find_node_binary() -> String {
    // Try the WASM_NODE_BINARY env var first
    if let Ok(path) = std::env::var("WASM_NODE_BINARY") {
        if Path::new(&path).exists() {
            return path;
        }
    }

    // Walk up from the current executable's directory to find the workspace root
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));

    let debug_binary = workspace_root.join("target/debug/wasm-node");
    let release_binary = workspace_root.join("target/release/wasm-node");

    // Prefer debug binary — it's what `cargo build` produces by default
    // and is always fresher than a potentially stale release build.
    if debug_binary.exists() {
        return debug_binary.to_string_lossy().to_string();
    }

    if release_binary.exists() {
        return release_binary.to_string_lossy().to_string();
    }

    // Fall back — the caller will get a clear error from Command::new
    warn!("wasm-node binary not found, will attempt to build");
    "target/debug/wasm-node".to_string()
}
