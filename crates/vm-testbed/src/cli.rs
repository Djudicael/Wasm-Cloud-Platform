use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;
use vm_testbed::{network, ClusterFixture, MicroVm, VmConfig};

#[derive(Parser)]
#[command(name = "vm-testbed-cli")]
#[command(about = "MicroVM testbed CLI for the Wasm Cloud Platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Bring up a named topology for manual validation.
    Up {
        /// Cluster name used for VMs, bridges, and state.
        #[arg(long, default_value = "manual")]
        name: String,
        /// Topology profile to start.
        #[arg(long, value_enum, default_value = "single-node")]
        profile: TopologyProfile,
        /// Override the node count for profiles that support it.
        #[arg(long)]
        nodes: Option<usize>,
        /// Memory per node in MiB.
        #[arg(long, default_value = "512")]
        node_memory: usize,
        /// vCPUs per node.
        #[arg(long, default_value = "2")]
        node_vcpus: usize,
        /// Memory for the NATS VM in MiB.
        #[arg(long, default_value = "256")]
        nats_memory: usize,
        /// vCPUs for the NATS VM.
        #[arg(long, default_value = "1")]
        nats_vcpus: usize,
        /// Write or read cluster state from this file.
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        /// Keep the CLI attached and teardown on Ctrl+C instead of detaching.
        #[arg(long)]
        attach: bool,
    },

    /// Tear down a previously detached topology.
    Down {
        /// State file created by `up`.
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
    },

    /// Show status for a detached topology.
    Status {
        /// State file created by `up`.
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
    },

    /// Kill one VM from a detached topology.
    Kill {
        /// VM identifier.
        #[arg(short, long)]
        id: String,
        /// State file created by `up`.
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
    },

    /// Check health of a running node.
    Health {
        #[arg(short, long)]
        ip: String,
        #[arg(short, long, default_value = "9090")]
        port: u16,
    },

    /// Run the existing VM asset/setup helpers through one command surface.
    Assets {
        #[command(subcommand)]
        command: AssetCommands,
    },

    /// Spawn a single wasm-node microVM in the foreground.
    SpawnNode {
        #[arg(short, long)]
        id: String,
        #[arg(short, long)]
        ip: String,
        #[arg(short, long, default_value = "512")]
        memory: usize,
        #[arg(short, long, default_value = "2")]
        vcpus: usize,
        #[arg(long, env = "VM_KERNEL_PATH")]
        kernel: Option<PathBuf>,
        #[arg(long, env = "VM_NODE_ROOTFS")]
        rootfs: Option<PathBuf>,
        #[arg(long, default_value = "nats://172.20.0.10:4222")]
        nats_url: String,
        #[arg(long, default_value = "br-wasm")]
        bridge: String,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
enum TopologyProfile {
    SingleNode,
    MultiNode,
    ChaosReady,
}

#[derive(Subcommand)]
enum AssetCommands {
    InstallFirecracker,
    BuildKernel,
    BuildNodeRootfs,
    BuildNatsRootfs,
    BuildAllImages,
    SetupNetwork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedVm {
    id: String,
    pid: u32,
    ip: String,
    tap_device: String,
    admin_addr: String,
    proxy_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedClusterState {
    name: String,
    profile: TopologyProfile,
    bridge_name: String,
    subnet: String,
    gateway: String,
    nats_url: String,
    nats: Option<PersistedVm>,
    nodes: Vec<PersistedVm>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    match Cli::parse().command {
        Commands::Up {
            name,
            profile,
            nodes,
            node_memory,
            node_vcpus,
            nats_memory,
            nats_vcpus,
            state_file,
            attach,
        } => {
            let requested_nodes = match profile {
                TopologyProfile::SingleNode => 1,
                TopologyProfile::MultiNode => nodes.unwrap_or(3).max(2),
                TopologyProfile::ChaosReady => nodes.unwrap_or(3).max(2),
            };

            let mut cluster = ClusterFixture::new(name.clone()).await?;
            cluster.start_nats(nats_memory, nats_vcpus).await?;
            for _ in 0..requested_nodes {
                cluster.start_node(node_memory, node_vcpus).await?;
            }
            cluster
                .wait_for_all_healthy(Duration::from_secs(120))
                .await?;

            let state = detach_cluster_state(&mut cluster, profile)?;
            write_state(&state_file, &state)?;
            print_state_summary(&state);

            if attach {
                info!("Topology attached; press Ctrl+C to teardown");
                tokio::signal::ctrl_c().await?;
                let _ = down_from_state(&state_file).await;
            }
        }

        Commands::Down { state_file } => {
            down_from_state(&state_file).await?;
        }

        Commands::Status { state_file } => {
            let state = read_state(&state_file)?;
            print_state_summary(&state);
            print_runtime_status(&state).await?;
        }

        Commands::Kill { id, state_file } => {
            let state = read_state(&state_file)?;
            let vm = state
                .nats
                .iter()
                .chain(state.nodes.iter())
                .find(|vm| vm.id == id)
                .ok_or_else(|| anyhow!("VM {id} not found in {}", state_file.display()))?;
            signal_pid(vm.pid, "-TERM")?;
            sleep(Duration::from_secs(1)).await;
            if process_alive(vm.pid) {
                signal_pid(vm.pid, "-KILL")?;
            }
            println!("Killed {} (pid {})", vm.id, vm.pid);
        }

        Commands::Health { ip, port } => {
            let addr = format!("{ip}:{port}");
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?;
            let resp = client.get(format!("http://{addr}/healthz")).send().await?;
            if resp.status().is_success() {
                println!("healthy: {addr}");
            } else {
                bail!("node at {addr} returned {}", resp.status());
            }
        }

        Commands::Assets { command } => run_asset_command(command)?,

        Commands::SpawnNode {
            id,
            ip,
            memory,
            vcpus,
            kernel,
            rootfs,
            nats_url,
            bridge,
        } => {
            let kernel_path = kernel.unwrap_or_else(find_kernel);
            let rootfs_path = rootfs.unwrap_or_else(find_node_rootfs);
            let tap = format!("tap-{id}");

            let config = VmConfig {
                id: id.clone(),
                kernel_path,
                rootfs_path,
                data_drive_path: None,
                memory_mb: memory,
                vcpus,
                ip: ip.clone(),
                gateway: "172.20.0.1".to_string(),
                bridge_name: bridge,
                tap_device: tap,
                mmds_data: Some(serde_json::json!({
                    "node_config": {
                        "node_id": id,
                        "nats_url": nats_url,
                    }
                })),
            };

            let mut vm = MicroVm::spawn(config).await?;
            vm.wait_for_health(Duration::from_secs(60)).await?;
            println!("node healthy at {}", vm.admin_addr());
            tokio::signal::ctrl_c().await?;
            vm.shutdown().await?;
        }
    }

    Ok(())
}

fn detach_cluster_state(
    cluster: &mut ClusterFixture,
    profile: TopologyProfile,
) -> Result<PersistedClusterState> {
    let nats = cluster.nats.as_mut().map(detach_vm_state);
    let mut nodes = BTreeMap::new();
    for (id, vm) in &mut cluster.nodes {
        nodes.insert(id.clone(), detach_vm_state(vm));
    }

    Ok(PersistedClusterState {
        name: cluster.name.clone(),
        profile,
        bridge_name: cluster.bridge_name.clone(),
        subnet: cluster.subnet.clone(),
        gateway: cluster.gateway.clone(),
        nats_url: cluster.nats_url()?,
        nats,
        nodes: nodes.into_values().collect(),
    })
}

fn detach_vm_state(vm: &mut MicroVm) -> PersistedVm {
    vm.disable_cleanup_on_drop();
    PersistedVm {
        id: vm.config.id.clone(),
        pid: vm.pid(),
        ip: vm.config.ip.clone(),
        tap_device: vm.config.tap_device.clone(),
        admin_addr: vm.admin_addr(),
        proxy_addr: vm.proxy_addr(),
    }
}

async fn down_from_state(state_file: &Path) -> Result<()> {
    let state = read_state(state_file)?;

    for vm in state.nats.iter().chain(state.nodes.iter()) {
        if process_alive(vm.pid) {
            let _ = signal_pid(vm.pid, "-TERM");
        }
    }
    sleep(Duration::from_secs(1)).await;
    for vm in state.nats.iter().chain(state.nodes.iter()) {
        if process_alive(vm.pid) {
            let _ = signal_pid(vm.pid, "-KILL");
        }
    }

    network::teardown_network(&state.bridge_name, &state.subnet)?;
    if state_file.exists() {
        std::fs::remove_file(state_file)?;
    }

    println!("Torn down {}", state.name);
    Ok(())
}

async fn print_runtime_status(state: &PersistedClusterState) -> Result<()> {
    for vm in state.nats.iter().chain(state.nodes.iter()) {
        let alive = process_alive(vm.pid);
        if vm.admin_addr.ends_with(":9090") {
            let health = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()?
                .get(format!("http://{}/healthz", vm.admin_addr))
                .send()
                .await
                .ok()
                .map(|resp| resp.status().to_string())
                .unwrap_or_else(|| "unreachable".to_string());
            println!(
                "{} pid={} alive={} admin={} proxy={} health={}",
                vm.id, vm.pid, alive, vm.admin_addr, vm.proxy_addr, health
            );
        } else {
            println!("{} pid={} alive={}", vm.id, vm.pid, alive);
        }
    }
    Ok(())
}

fn print_state_summary(state: &PersistedClusterState) {
    println!("name: {}", state.name);
    println!("profile: {:?}", state.profile);
    println!("bridge: {}", state.bridge_name);
    println!("subnet: {}", state.subnet);
    println!("nats: {}", state.nats_url);
    for vm in &state.nodes {
        println!(
            "node {} admin={} proxy={} pid={}",
            vm.id, vm.admin_addr, vm.proxy_addr, vm.pid
        );
    }
}

fn write_state(path: &Path, state: &PersistedClusterState) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

fn read_state(path: &Path) -> Result<PersistedClusterState> {
    let raw = std::fs::read(path)
        .with_context(|| format!("failed to read state file {}", path.display()))?;
    Ok(serde_json::from_slice(&raw)?)
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn signal_pid(pid: u32, signal: &str) -> Result<()> {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .with_context(|| format!("failed to send {signal} to pid {pid}"))?;
    if !status.success() {
        bail!("kill {signal} {pid} failed with status {status}");
    }
    Ok(())
}

fn run_asset_command(command: AssetCommands) -> Result<()> {
    let script = match command {
        AssetCommands::InstallFirecracker => "scripts/vm/install-firecracker.sh",
        AssetCommands::BuildKernel => "scripts/vm/build-kernel.sh",
        AssetCommands::BuildNodeRootfs => "scripts/vm/build-node-rootfs.sh",
        AssetCommands::BuildNatsRootfs => "scripts/vm/build-nats-rootfs.sh",
        AssetCommands::BuildAllImages => "scripts/vm/build-all-images.sh",
        AssetCommands::SetupNetwork => "scripts/vm/setup-network.sh",
    };
    let status = Command::new("bash").arg(script).status()?;
    if !status.success() {
        bail!("{script} failed with status {status}");
    }
    Ok(())
}

fn find_kernel() -> PathBuf {
    let candidates = ["./assets/vmlinux-6.1", "/opt/vm-testbed/vmlinux-6.1"];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    panic!("Kernel not found. Set VM_KERNEL_PATH.")
}

fn find_node_rootfs() -> PathBuf {
    let candidates = [
        "./assets/wasm-node-rootfs.ext4",
        "/opt/vm-testbed/wasm-node-rootfs.ext4",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    panic!("Rootfs not found. Set VM_NODE_ROOTFS.")
}
