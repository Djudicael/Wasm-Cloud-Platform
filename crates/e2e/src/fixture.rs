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

use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostContainerRuntime {
    Podman,
    Docker,
}

fn select_host_container_runtime(
    docker_host: Option<&str>,
    podman_available: bool,
    docker_available: bool,
) -> Option<HostContainerRuntime> {
    if docker_host.is_some_and(|host| host.contains("podman.sock")) {
        return podman_available.then_some(HostContainerRuntime::Podman);
    }
    if docker_host.is_some_and(|host| host.contains("docker.sock")) {
        return docker_available.then_some(HostContainerRuntime::Docker);
    }
    if podman_available {
        Some(HostContainerRuntime::Podman)
    } else if docker_available {
        Some(HostContainerRuntime::Docker)
    } else {
        None
    }
}

pub(crate) fn detect_host_container_runtime() -> Option<HostContainerRuntime> {
    let podman_available = Command::new("podman")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    let docker_available = Command::new("docker")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    select_host_container_runtime(
        std::env::var("DOCKER_HOST").ok().as_deref(),
        podman_available,
        docker_available,
    )
}

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
            info!("Configured Podman socket for host container runtime (uid={uid})");
        }
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
    use std::sync::atomic::Ordering;
    let idx = CLUSTER_COUNTER.fetch_add(1, Ordering::SeqCst);
    // Randomize the first allocation using the process ID so that stale
    // processes from previous test runs don't collide.
    if idx == 0 {
        let seed = std::process::id() as u16;
        CLUSTER_COUNTER.store(2, Ordering::SeqCst);
        return seed.saturating_mul(2) % 30_000; // keep within ephemeral range
    }
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

pub struct NodeProcessStartConfig<'a> {
    pub node_id: &'a str,
    pub nats_url: &'a str,
    pub admin_port: u16,
    pub proxy_port: u16,
    pub artifact_port: u16,
    pub port_start: u16,
    pub port_end: u16,
    pub seed_local_state: bool,
}

impl NodeProcess {
    pub fn port_start(&self) -> u16 {
        self.port_start
    }

    pub fn port_end(&self) -> u16 {
        self.port_end
    }

    /// Start a wasm-node process with the given configuration.
    ///
    /// The process is spawned with `Stdio::null()` for stdout to prevent
    /// pipe-buffer backpressure deadlock on high log volume, and
    /// `Stdio::inherit()` for stderr so panics are visible.
    pub async fn start(config: NodeProcessStartConfig<'_>) -> Result<Self, String> {
        let NodeProcessStartConfig {
            node_id,
            nats_url,
            admin_port,
            proxy_port,
            artifact_port,
            port_start,
            port_end,
            seed_local_state,
        } = config;
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
        let gateway_port = admin_port.saturating_add(1000);
        let dns_stub_port = admin_port.saturating_add(2000);

        if seed_local_state {
            let store = storage::Store::open(&db_path)
                .map_err(|e| format!("failed to open seeded fixture store: {e}"))?;
            store
                .store_artifact(&common::types::AppId::new("__fixture", "v1"), b"fixture")
                .map_err(|e| format!("failed to seed fixture artifact: {e}"))?;
        }

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
level = "info"
format = "text"
output = "/tmp/wasm-node-e2e.log"

[dns]
stub_enabled = true
stub_port = {dns_stub_port}

[ebpf]
gateway_port = {gateway_port}

[health]
check_interval_secs = 2
"#,
            db_path.display(),
            gateway_port = gateway_port,
            dns_stub_port = dns_stub_port
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
            .env("NODE_ID", node_id)
            // stdout=null avoids pipe-buffer backpressure deadlock when
            // RUST_LOG=debug produces high log volume. stderr is inherited
            // so panics and eprintln! still appear in test output.
            .stdout(Stdio::null())
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
            Duration::from_secs(60),
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
            .env("NODE_ID", &self.node_id)
            .stdout(Stdio::null())
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

const NATS_IMAGE: &str = "docker.io/library/nats:2.10-alpine";
const NATS_OWNER_LABEL: &str = "dev.wasm-cloud-platform.e2e.nats";

struct OwnedNatsContainer {
    runtime: String,
    container_id: String,
    owner: String,
}

fn owned_nats_id(text: &str, owner: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid NATS inspection: {e}"))?;
    let items = value.as_array().ok_or("NATS inspection must be an array")?;
    if items.len() != 1 || items[0]["Config"]["Labels"][NATS_OWNER_LABEL].as_str() != Some(owner) {
        return Err("refusing unowned NATS container".into());
    }
    let id = items[0]["Id"].as_str().ok_or("missing NATS container ID")?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("invalid NATS container ID".into());
    }
    Ok(id.into())
}

fn nats_create_command(runtime: &str, owner: &str, port: u16) -> Command {
    let mut command = Command::new(runtime);
    command.args([
        "create",
        "--name",
        &format!("wcp-e2e-nats-{owner}"),
        "--label",
        &format!("{NATS_OWNER_LABEL}={owner}"),
        "--publish",
        &format!("127.0.0.1:{port}:4222"),
        NATS_IMAGE,
        "-js",
        "--port",
        "4222",
    ]);
    command
}

fn nats_published_address(text: &str, expected_port: u16) -> Result<SocketAddr, String> {
    let address: SocketAddr = text
        .trim()
        .parse()
        .map_err(|e| format!("invalid NATS port publication: {e}"))?;
    if address.ip() != std::net::Ipv4Addr::LOCALHOST
        || address.port() == 0
        || address.port() != expected_port
    {
        return Err("NATS publication differs from its requested loopback endpoint".into());
    }
    Ok(address)
}

impl OwnedNatsContainer {
    fn inspect(runtime: &str, lookup: &str, owner: &str) -> Result<Self, String> {
        let output = Command::new(runtime)
            .args(["inspect", lookup])
            .output()
            .map_err(|e| format!("failed to inspect NATS container: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "{runtime} could not inspect owned NATS container {lookup}"
            ));
        }
        let container_id = owned_nats_id(&String::from_utf8_lossy(&output.stdout), owner)?;
        Ok(Self {
            runtime: runtime.into(),
            container_id,
            owner: owner.into(),
        })
    }

    fn verify(&self) -> Result<(), String> {
        let output = Command::new(&self.runtime)
            .args(["inspect", &self.container_id])
            .output()
            .map_err(|e| format!("failed to inspect NATS ownership: {e}"))?;
        if !output.status.success()
            || owned_nats_id(&String::from_utf8_lossy(&output.stdout), &self.owner)?
                != self.container_id
        {
            return Err("NATS container identity changed or disappeared".into());
        }
        Ok(())
    }

    fn action(&self, action: &str) -> Result<(), String> {
        self.verify()?;
        let output = Command::new(&self.runtime)
            .args([action, &self.container_id])
            .output()
            .map_err(|e| format!("failed to {action} owned NATS container: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "{} {action} {} failed: {}",
                self.runtime,
                self.container_id,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn address(&self, expected_port: u16) -> Result<SocketAddr, String> {
        self.verify()?;
        let output = Command::new(&self.runtime)
            .args(["port", &self.container_id, "4222/tcp"])
            .output()
            .map_err(|e| format!("failed to inspect NATS publication: {e}"))?;
        if !output.status.success() {
            return Err("cannot inspect owned NATS port publication".into());
        }
        nats_published_address(&String::from_utf8_lossy(&output.stdout), expected_port)
    }
}

impl Drop for OwnedNatsContainer {
    fn drop(&mut self) {
        if let Err(error) = self.verify() {
            warn!(container_id = %self.container_id, %error, "NATS cleanup refused or already absent");
            return;
        }
        match Command::new(&self.runtime)
            .args(["rm", "-f", &self.container_id])
            .output()
        {
            Ok(output) if output.status.success() => {
                info!(container_id = %self.container_id, "owned NATS container removed");
            }
            Ok(_) => warn!(container_id = %self.container_id, "owned NATS removal failed"),
            Err(error) => {
                warn!(container_id = %self.container_id, %error, "owned NATS removal failed")
            }
        }
    }
}

/// A running NATS container with a verified loopback-published port.
///
/// Host networking can refer to a rootless runtime's own network namespace in
/// WSL. Explicit publication is reachable by native test/node processes. Every
/// fixture has a unique owner label and is operated on only by its exact ID.
pub struct NatsContainer {
    pub url: String,
    pub port: u16,
    container: OwnedNatsContainer,
}

impl NatsContainer {
    async fn start_with_runtime(runtime: &str, requested_port: u16) -> Result<Self, String> {
        // Choose a fixed publication even for port=0 so stop/resume preserves
        // the endpoint. A publication race fails closed; no resource is replaced.
        let port = if requested_port == 0 {
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .and_then(|listener| listener.local_addr())
                .map_err(|e| format!("cannot allocate local NATS port: {e}"))?
                .port()
        } else {
            requested_port
        };
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("wcp-e2e-nats-{owner}");
        let output = nats_create_command(runtime, &owner, port)
            .output()
            .map_err(|e| format!("failed to create NATS container via {runtime}: {e}"))?;
        // Establish exact ownership before checking create/start outcomes. This
        // guard also removes a partially created fixture if starting it fails.
        let container = OwnedNatsContainer::inspect(runtime, &name, &owner)
            .map_err(|e| format!("NATS creation could not establish ownership ({name}): {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "{runtime} create failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        container.action("start")?;
        let address = container.address(port)?;
        crate::helpers::wait_for_tcp(
            &address.ip().to_string(),
            address.port(),
            Duration::from_secs(15),
        )
        .await
        .map_err(|e| format!("owned NATS publication {address} not reachable: {e}"))?;
        let fixture = Self {
            url: format!("nats://{address}"),
            port: address.port(),
            container,
        };
        // Verify protocol and JetStream initialization, not just an open socket.
        let _bus = fixture.connect().await?;
        info!(%address, runtime, "NATS fixture ready through verified publication");
        Ok(fixture)
    }

    /// Start isolated JetStream NATS at the requested loopback port, or choose
    /// an available fixed port when zero is supplied. The port survives resume.
    pub async fn start(port: u16) -> Result<Self, String> {
        setup_container_runtime();
        let runtime = match detect_host_container_runtime() {
            Some(HostContainerRuntime::Podman) => "podman",
            Some(HostContainerRuntime::Docker) => "docker",
            None => return Err("neither podman nor docker is available for NATS".into()),
        };
        Self::start_with_runtime(runtime, port).await
    }

    /// Connect to this NATS instance and return a `NatsBus`.
    pub async fn connect(&self) -> Result<NatsBus, String> {
        NatsBus::connect(&self.url)
            .await
            .map_err(|e| format!("failed to connect to NATS: {e}"))
    }

    pub fn stop(&self) -> Result<(), String> {
        self.container.action("stop")
    }

    pub async fn resume(&self) -> Result<(), String> {
        self.container.action("start")?;
        let address = self.container.address(self.port)?;
        if self.url != format!("nats://{address}") {
            return Err("resumed NATS publication changed".into());
        }
        crate::helpers::wait_for_tcp(
            &address.ip().to_string(),
            address.port(),
            Duration::from_secs(15),
        )
        .await
        .map_err(|e| format!("resumed NATS publication {address} not reachable: {e}"))?;
        let _bus = self.connect().await?;
        Ok(())
    }
}

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
            let node = NodeProcess::start(NodeProcessStartConfig {
                node_id: &node_id,
                nats_url: &nats_url,
                admin_port,
                proxy_port,
                artifact_port,
                port_start,
                port_end,
                seed_local_state: node_count > 1,
            })
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

    /// Deploy an app with a custom configuration.
    pub async fn deploy_app_with_config(
        &self,
        app_id: &str,
        wasm_path: &std::path::Path,
        config: common::types::AppConfig,
    ) -> Result<(), String> {
        let bus = self.connect_bus().await?;

        let sha256 = crate::helpers::sha256_file(wasm_path)?;
        let size_bytes = std::fs::metadata(wasm_path)
            .map_err(|e| format!("failed to read wasm metadata: {e}"))?
            .len();

        crate::helpers::upload_artifact(self.nodes[0].artifact_port, wasm_path, &sha256).await?;

        let artifact_url = format!(
            "http://127.0.0.1:{}/artifacts/{}",
            self.nodes[0].artifact_port, sha256
        );

        crate::helpers::deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
            .await
            .map_err(|e| format!("deploy failed: {e}"))
    }

    /// Set a gateway config for an app via NATS.
    pub async fn set_gateway_config(
        &self,
        app_id: &str,
        config: common::types::GatewayRouteConfig,
    ) -> Result<(), String> {
        let bus = self.connect_bus().await?;
        crate::helpers::set_gateway_config(&bus, app_id, config).await
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

    let target_dir = cargo_target_dir(workspace_root);
    let debug_binary = target_dir.join("debug/wasm-node");
    let release_binary = target_dir.join("release/wasm-node");

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
    debug_binary.to_string_lossy().to_string()
}

fn cargo_target_dir(workspace_root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .unwrap_or_else(|| workspace_root.join("target"))
}

#[cfg(test)]
mod tests {
    use super::{select_host_container_runtime, HostContainerRuntime};

    #[test]
    fn nats_publication_must_be_one_exact_nonzero_loopback_endpoint() {
        assert_eq!(
            super::nats_published_address("127.0.0.1:42461\n", 42461).unwrap(),
            "127.0.0.1:42461".parse().unwrap()
        );
        for mapping in [
            "0.0.0.0:42461",
            "172.18.0.1:42461",
            "[::1]:42461",
            "127.0.0.1:4222",
            "127.0.0.1:42461\n127.0.0.1:42462",
            "invalid",
        ] {
            assert!(super::nats_published_address(mapping, 42461).is_err());
        }
        assert!(super::nats_published_address("127.0.0.1:0", 0).is_err());
    }

    #[test]
    fn nats_ownership_requires_exact_label_and_full_container_id() {
        let owner = "fixture-owner";
        let id = "ab".repeat(32);
        let inspected = serde_json::json!([{
            "Id":id,
            "Config":{"Labels":{(super::NATS_OWNER_LABEL):owner}}
        }]);
        assert_eq!(
            super::owned_nats_id(&inspected.to_string(), owner).unwrap(),
            id
        );
        assert!(super::owned_nats_id(&inspected.to_string(), "foreign-owner").is_err());
        let mut bad_id = inspected.clone();
        bad_id[0]["Id"] = serde_json::json!("abc123");
        assert!(super::owned_nats_id(&bad_id.to_string(), owner).is_err());
        bad_id[0]["Id"] = serde_json::json!("../container");
        assert!(super::owned_nats_id(&bad_id.to_string(), owner).is_err());
        assert!(super::owned_nats_id("[]", owner).is_err());
        assert!(super::owned_nats_id(
            &serde_json::json!([inspected[0], inspected[0]]).to_string(),
            owner
        )
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_nats_start_removes_only_the_exact_created_container() {
        use std::os::unix::fs::PermissionsExt;
        for failure in ["create", "start", "publication"] {
            let dir = tempfile::tempdir().unwrap();
            let executable = dir.path().join("fake-runtime");
            std::fs::write(dir.path().join("failure"), failure).unwrap();
            std::fs::write(&executable, r#"#!/bin/sh
set -eu
cd -- "$(dirname "$0")"
printf '%s\n' "$*" >> commands
id=abababababababababababababababababababababababababababababababab
mode=$(cat failure)
case "$1" in
 create)
  shift
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --label ]; then shift; printf '%s' "${1#*=}" > owner; fi
    shift
  done
  printf '%s\n' "$id"
  if [ "$mode" = create ]; then exit 17; fi
  ;;
 inspect)
  test ! -f removed
  printf '{"Id":"%s","Config":{"Labels":{"dev.wasm-cloud-platform.e2e.nats":"%s"}}}\n' "$id" "$(cat owner)" | sed 's/^/[/;s/$/]/'
  ;;
 start)
  if [ "$mode" = start ]; then exit 19; fi
  ;;
 port)
  printf '0.0.0.0:42461\n'
  ;;
 rm)
  test "$2" = -f
  test "$3" = "$id"
  touch removed
  ;;
 *) exit 21 ;;
esac
"#).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result =
                super::NatsContainer::start_with_runtime(executable.to_str().unwrap(), 42461).await;
            assert!(result.is_err(), "{failure} must fail");
            assert!(
                dir.path().join("removed").exists(),
                "{failure} must clean its created container"
            );
            let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
            assert!(commands.contains("--publish 127.0.0.1:42461:4222"));
            assert!(!commands.contains("--network=host"));
            let removals: Vec<_> = commands
                .lines()
                .filter(|line| line.starts_with("rm "))
                .collect();
            assert_eq!(
                removals,
                ["rm -f abababababababababababababababababababababababababababababababab"]
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires local Podman/Docker; owns one UUID-labelled NATS container"]
    async fn published_nats_lifecycle_preserves_endpoint_and_removes_exact_container() {
        let fixture = super::NatsContainer::start(0)
            .await
            .expect("start isolated NATS");
        assert!(fixture.port > 0);
        assert_eq!(fixture.url, format!("nats://127.0.0.1:{}", fixture.port));
        let url = fixture.url.clone();
        let runtime = fixture.container.runtime.clone();
        let id = fixture.container.container_id.clone();
        let bus = fixture.connect().await.expect("NATS protocol before stop");
        bus.client().flush().await.expect("flush before stop");
        drop(bus);
        fixture.stop().expect("stop exact fixture");
        fixture.resume().await.expect("resume exact fixture");
        assert_eq!(fixture.url, url);
        let bus = fixture.connect().await.expect("NATS protocol after resume");
        bus.client().flush().await.expect("flush after resume");
        drop(bus);
        drop(fixture);
        let inspection = std::process::Command::new(runtime)
            .args(["inspect", &id])
            .output()
            .unwrap();
        assert!(
            !inspection.status.success(),
            "dropped fixture container still exists"
        );
    }
    #[test]
    fn test_select_host_container_runtime_prefers_podman_socket_hint() {
        assert_eq!(
            select_host_container_runtime(
                Some("unix:///run/user/1000/podman/podman.sock"),
                true,
                true
            ),
            Some(HostContainerRuntime::Podman)
        );
    }

    #[test]
    fn test_select_host_container_runtime_prefers_available_podman_without_hint() {
        assert_eq!(
            select_host_container_runtime(None, true, true),
            Some(HostContainerRuntime::Podman)
        );
    }

    #[test]
    fn test_select_host_container_runtime_falls_back_to_docker() {
        assert_eq!(
            select_host_container_runtime(None, false, true),
            Some(HostContainerRuntime::Docker)
        );
    }
}
