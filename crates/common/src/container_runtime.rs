use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostContainerRuntime {
    Podman,
    Docker,
}

pub fn detect_host_container_runtime() -> Option<HostContainerRuntime> {
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

    if podman_available {
        Some(HostContainerRuntime::Podman)
    } else if docker_available {
        Some(HostContainerRuntime::Docker)
    } else {
        None
    }
}

pub fn reserve_host_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("failed to bind ephemeral port: {e}"))?
        .local_addr()
        .map_err(|e| format!("failed to read ephemeral port: {e}"))
        .map(|addr| addr.port())
}

pub fn wait_for_tcp(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match TcpStream::connect((host, port)) {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() >= deadline => {
                return Err(format!("tcp listener not ready on {host}:{port}"));
            }
            Err(_) => sleep(Duration::from_millis(100)),
        }
    }
}

fn runtime_bin(runtime: HostContainerRuntime) -> &'static str {
    match runtime {
        HostContainerRuntime::Podman => "podman",
        HostContainerRuntime::Docker => "docker",
    }
}

fn unique_name(prefix: &str, port: u16) -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{pid}-{port}-{nanos}")
}

struct ManagedContainer {
    runtime: HostContainerRuntime,
    container_id: String,
}

impl ManagedContainer {
    fn remove(&self) {
        let _ = Command::new(runtime_bin(self.runtime))
            .args(["rm", "-f", &self.container_id])
            .output();
    }

    fn stop(&self) -> Result<(), String> {
        let output = Command::new(runtime_bin(self.runtime))
            .args(["stop", &self.container_id])
            .output()
            .map_err(|e| format!("failed to stop container: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    fn start(&self) -> Result<(), String> {
        let output = Command::new(runtime_bin(self.runtime))
            .args(["start", &self.container_id])
            .output()
            .map_err(|e| format!("failed to start container: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

pub struct NatsContainer {
    pub url: String,
    pub port: u16,
    inner: ManagedContainer,
}

impl NatsContainer {
    pub fn start(port: u16) -> Result<Self, String> {
        let runtime = detect_host_container_runtime()
            .ok_or_else(|| "neither podman nor docker is available".to_string())?;
        let name = unique_name("wcp-nats", port);
        let runtime_bin = runtime_bin(runtime);

        let _ = Command::new(runtime_bin).args(["rm", "-f", &name]).output();

        let output = Command::new(runtime_bin)
            .args([
                "run",
                "-d",
                "--network=host",
                "--name",
                &name,
                "docker.io/library/nats:2.10-alpine",
                "-js",
                "--port",
                &port.to_string(),
            ])
            .output()
            .map_err(|e| format!("failed to run {runtime_bin}: {e}"))?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        wait_for_tcp("127.0.0.1", port, Duration::from_secs(10))?;
        sleep(Duration::from_secs(1));
        Ok(Self {
            url: format!("nats://127.0.0.1:{port}"),
            port,
            inner: ManagedContainer {
                runtime,
                container_id,
            },
        })
    }

    pub fn stop(&self) -> Result<(), String> {
        self.inner.stop()
    }

    pub fn resume(&self) -> Result<(), String> {
        self.inner.start()?;
        wait_for_tcp("127.0.0.1", self.port, Duration::from_secs(10))
    }
}

impl Drop for NatsContainer {
    fn drop(&mut self) {
        self.inner.remove();
    }
}

pub struct PostgresContainer {
    pub url: String,
    pub port: u16,
    inner: ManagedContainer,
}

impl PostgresContainer {
    pub fn start(port: u16, password: &str) -> Result<Self, String> {
        let runtime = detect_host_container_runtime()
            .ok_or_else(|| "neither podman nor docker is available".to_string())?;
        let name = unique_name("wcp-postgres", port);
        let runtime_bin = runtime_bin(runtime);

        let _ = Command::new(runtime_bin).args(["rm", "-f", &name]).output();

        let output = Command::new(runtime_bin)
            .args([
                "run",
                "-d",
                "-p",
                &format!("{port}:5432"),
                "--name",
                &name,
                "-e",
                &format!("POSTGRES_PASSWORD={password}"),
                "-e",
                "POSTGRES_USER=postgres",
                "-e",
                "POSTGRES_DB=postgres",
                "docker.io/library/postgres:17-alpine",
            ])
            .output()
            .map_err(|e| format!("failed to run {runtime_bin}: {e}"))?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        wait_for_tcp("127.0.0.1", port, Duration::from_secs(15))?;
        sleep(Duration::from_secs(2));
        Ok(Self {
            url: format!("postgres://postgres:{password}@127.0.0.1:{port}/postgres"),
            port,
            inner: ManagedContainer {
                runtime,
                container_id,
            },
        })
    }
}

impl Drop for PostgresContainer {
    fn drop(&mut self) {
        self.inner.remove();
    }
}
