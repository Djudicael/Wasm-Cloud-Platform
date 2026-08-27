use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use common::policy::{FilesystemPolicyConfig, NetworkPolicyConfig, PolicyConfig};
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages, Route};
use messaging::{events::Event, NatsBus};
use serde::{Deserialize, Serialize};
use sha2::Digest;
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
// This human-facing CLI intentionally keeps deploy options explicit so clap
// generates complete help text for policy-validation rehearsals.
#[allow(clippy::large_enum_variant)]
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

    /// Add one node to a detached topology.
    AddNode {
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        #[arg(long)]
        memory: Option<usize>,
        #[arg(long)]
        vcpus: Option<usize>,
    },

    /// Remove one node from a detached topology.
    RemoveNode {
        #[arg(short, long)]
        id: String,
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
    },

    /// Restart one detached platform node from the configured rootfs image.
    RestartNode {
        #[arg(short, long)]
        id: String,
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        /// Inject one deterministic eBPF failure into this local test node.
        #[arg(long, value_enum)]
        ebpf_test_fault: Option<EbpfTestFault>,
        /// Configure eBPF as mandatory for this restart.
        #[arg(long)]
        ebpf_required: bool,
        /// Persist a running VM that is expected not to become healthy.
        #[arg(long, requires = "ebpf_required")]
        expect_unhealthy: bool,
        /// Use a one-restart kernel override without changing topology state.
        #[arg(long)]
        kernel: Option<PathBuf>,
        /// Start the guest node with an empty Linux capability bounding set.
        #[arg(long)]
        drop_ebpf_capabilities: bool,
    },

    /// Scale a detached topology to the requested node count.
    Scale {
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        #[arg(long)]
        nodes: usize,
    },

    /// Add a non-platform service microVM to a detached topology.
    AddService {
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        ip: String,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        rootfs: PathBuf,
        #[arg(long, default_value = "512")]
        memory: usize,
        #[arg(long, default_value = "1")]
        vcpus: usize,
        #[arg(long, default_value = "120")]
        timeout: u64,
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

    /// Deploy a Wasm application into a detached topology.
    DeployApp {
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        #[arg(long)]
        app: String,
        #[arg(long)]
        version: String,
        #[arg(long, default_value = "default")]
        namespace: String,
        #[arg(long)]
        wasm: PathBuf,
        #[arg(long)]
        route_host: Option<String>,
        /// Route path prefix; repeat to attach multiple prefixes to this app.
        #[arg(long = "route-path")]
        route_paths: Vec<String>,
        #[arg(long)]
        target_node: Option<String>,
        #[arg(long, default_value = "500000000")]
        fuel: u64,
        #[arg(long, default_value = "128")]
        memory_mb: u32,
        #[arg(long, default_value = "10")]
        max_instances: u32,
        #[arg(long, default_value = "100")]
        max_outbound_connections: u32,
        /// Restrict outbound traffic to these CIDRs; repeat as needed.
        #[arg(long = "allowed-cidr")]
        allowed_cidrs: Vec<String>,
        /// Explicitly deny outbound traffic to these CIDRs; repeat as needed.
        #[arg(long = "denied-cidr")]
        denied_cidrs: Vec<String>,
        /// Writable host path exposed to the component; repeat as needed.
        #[arg(long = "allowed-filesystem-path")]
        allowed_filesystem_paths: Vec<String>,
        #[arg(long, default_value = "300")]
        idle_timeout: u64,
        #[arg(long, default_value = "8080")]
        bind_port: u16,
        /// Runtime health path, or `none` for apps without a health endpoint.
        #[arg(long, default_value = "/health")]
        health_check_path: String,
        #[arg(long = "env", value_parser = parse_env_var)]
        env_vars: Vec<(String, String)>,
        #[arg(long = "secret")]
        secret_keys: Vec<String>,
    },

    /// Remove an application from a detached topology.
    UndeployApp {
        #[arg(long, default_value = ".vm-testbed-state.json")]
        state_file: PathBuf,
        #[arg(long)]
        app_id: String,
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

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum EbpfTestFault {
    MissingCapability,
    PermissionDenied,
    ProgramRejected,
    ProbeUnavailable,
    MissingBtf,
    ConsumerExit,
}

impl EbpfTestFault {
    fn as_kernel_value(self) -> &'static str {
        match self {
            Self::MissingCapability => "missing_capability",
            Self::PermissionDenied => "permission_denied",
            Self::ProgramRejected => "program_rejected",
            Self::ProbeUnavailable => "probe_unavailable",
            Self::MissingBtf => "missing_btf",
            Self::ConsumerExit => "consumer_exit",
        }
    }
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
    artifact_addr: String,
    proxy_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedService {
    id: String,
    kind: String,
    pid: u32,
    ip: String,
    port: u16,
    tap_device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedClusterState {
    name: String,
    profile: TopologyProfile,
    bridge_name: String,
    subnet: String,
    gateway: String,
    nats_url: String,
    kernel_path: PathBuf,
    node_rootfs_path: PathBuf,
    node_data_drive_path: Option<PathBuf>,
    node_memory_mb: usize,
    node_vcpus: usize,
    next_node_index: u8,
    nats: Option<PersistedVm>,
    nodes: Vec<PersistedVm>,
    #[serde(default)]
    services: Vec<PersistedService>,
}

struct DeployRequest {
    app: String,
    version: String,
    namespace: String,
    wasm: PathBuf,
    route_host: Option<String>,
    route_paths: Vec<String>,
    target_node: Option<String>,
    fuel: u64,
    memory_mb: u32,
    max_instances: u32,
    max_outbound_connections: u32,
    allowed_cidrs: Vec<String>,
    denied_cidrs: Vec<String>,
    allowed_filesystem_paths: Vec<String>,
    idle_timeout: u64,
    bind_port: u16,
    health_check_path: Option<String>,
    env_vars: Vec<(String, String)>,
    secret_keys: Vec<String>,
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

            let state = detach_cluster_state(&mut cluster, profile, node_memory, node_vcpus)?;
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

        Commands::AddNode {
            state_file,
            memory,
            vcpus,
        } => {
            let mut state = read_state(&state_file)?;
            let default_memory = state.node_memory_mb;
            let default_vcpus = state.node_vcpus;
            let vm = spawn_node_from_state(
                &mut state,
                memory.unwrap_or(default_memory),
                vcpus.unwrap_or(default_vcpus),
            )
            .await?;
            write_state(&state_file, &state)?;
            println!(
                "Added {} admin={} proxy={} pid={}",
                vm.id, vm.admin_addr, vm.proxy_addr, vm.pid
            );
        }

        Commands::RemoveNode { id, state_file } => {
            let mut state = read_state(&state_file)?;
            remove_node_from_state(&mut state, &id).await?;
            write_state(&state_file, &state)?;
            println!("Removed {}", id);
        }

        Commands::RestartNode {
            id,
            state_file,
            ebpf_test_fault,
            ebpf_required,
            expect_unhealthy,
            kernel,
            drop_ebpf_capabilities,
        } => {
            let mut state = read_state(&state_file)?;
            let vm = restart_node_from_state(
                &mut state,
                &id,
                ebpf_test_fault,
                ebpf_required,
                expect_unhealthy,
                kernel,
                drop_ebpf_capabilities,
            )
            .await?;
            write_state(&state_file, &state)?;
            println!(
                "Restarted {} admin={} proxy={} pid={}",
                vm.id, vm.admin_addr, vm.proxy_addr, vm.pid
            );
        }

        Commands::Scale { state_file, nodes } => {
            let mut state = read_state(&state_file)?;
            let default_memory = state.node_memory_mb;
            let default_vcpus = state.node_vcpus;
            while state.nodes.len() < nodes {
                let vm = spawn_node_from_state(&mut state, default_memory, default_vcpus).await?;
                println!("Added {}", vm.id);
                write_state(&state_file, &state)?;
            }
            while state.nodes.len() > nodes {
                let victim = state
                    .nodes
                    .last()
                    .map(|vm| vm.id.clone())
                    .ok_or_else(|| anyhow!("no nodes available to remove"))?;
                remove_node_from_state(&mut state, &victim).await?;
                println!("Removed {}", victim);
                write_state(&state_file, &state)?;
            }
        }

        Commands::AddService {
            state_file,
            id,
            kind,
            ip,
            port,
            rootfs,
            memory,
            vcpus,
            timeout,
        } => {
            let mut state = read_state(&state_file)?;
            let service = spawn_service_from_state(
                &state,
                id,
                kind,
                ip,
                port,
                rootfs,
                memory,
                vcpus,
                Duration::from_secs(timeout),
            )
            .await?;
            state.services.push(service.clone());
            write_state(&state_file, &state)?;
            println!(
                "Added service {} kind={} address={}:{} pid={}",
                service.id, service.kind, service.ip, service.port, service.pid
            );
        }

        Commands::Kill { id, state_file } => {
            let state = read_state(&state_file)?;
            let vm = state
                .nats
                .iter()
                .chain(state.nodes.iter())
                .find(|vm| vm.id == id)
                .map(|vm| (vm.id.as_str(), vm.pid))
                .or_else(|| {
                    state
                        .services
                        .iter()
                        .find(|service| service.id == id)
                        .map(|service| (service.id.as_str(), service.pid))
                })
                .ok_or_else(|| anyhow!("VM {id} not found in {}", state_file.display()))?;
            signal_pid(vm.1, "-TERM")?;
            sleep(Duration::from_secs(1)).await;
            if process_alive(vm.1) {
                signal_pid(vm.1, "-KILL")?;
            }
            println!("Killed {} (pid {})", vm.0, vm.1);
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

        Commands::DeployApp {
            state_file,
            app,
            version,
            namespace,
            wasm,
            route_host,
            route_paths,
            target_node,
            fuel,
            memory_mb,
            max_instances,
            max_outbound_connections,
            allowed_cidrs,
            denied_cidrs,
            allowed_filesystem_paths,
            idle_timeout,
            bind_port,
            health_check_path,
            env_vars,
            secret_keys,
        } => {
            let state = read_state(&state_file)?;
            deploy_app_to_state(
                &state,
                DeployRequest {
                    app,
                    version,
                    namespace,
                    wasm,
                    route_host,
                    route_paths,
                    target_node,
                    fuel,
                    memory_mb,
                    max_instances,
                    max_outbound_connections,
                    allowed_cidrs,
                    denied_cidrs,
                    allowed_filesystem_paths,
                    idle_timeout,
                    bind_port,
                    health_check_path: match health_check_path.as_str() {
                        "none" => None,
                        path => Some(path.to_string()),
                    },
                    env_vars,
                    secret_keys,
                },
            )
            .await?;
        }

        Commands::UndeployApp { state_file, app_id } => {
            let state = read_state(&state_file)?;
            undeploy_app_from_state(&state, &app_id).await?;
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
            let tap = network::tap_name_for_id(&id);

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
                extra_kernel_args: Vec::new(),
                mmds_data: Some(serde_json::json!({
                    "node_config": {
                        "node_id": id,
                        "nats_url": nats_url,
                        "ip": ip,
                        "gateway": "172.20.0.1",
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
    node_memory_mb: usize,
    node_vcpus: usize,
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
        kernel_path: cluster.kernel_path.clone(),
        node_rootfs_path: cluster.node_rootfs.clone(),
        node_data_drive_path: cluster.node_data_drive.clone(),
        node_memory_mb,
        node_vcpus,
        next_node_index: cluster.node_count() as u8,
        nats,
        nodes: nodes.into_values().collect(),
        services: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn spawn_service_from_state(
    state: &PersistedClusterState,
    id: String,
    kind: String,
    ip: String,
    port: u16,
    rootfs: PathBuf,
    memory_mb: usize,
    vcpus: usize,
    timeout: Duration,
) -> Result<PersistedService> {
    if state.services.iter().any(|service| service.id == id) {
        bail!("service {id} already exists");
    }
    if state.nodes.iter().any(|node| node.ip == ip)
        || state.nats.iter().any(|nats| nats.ip == ip)
        || state.services.iter().any(|service| service.ip == ip)
    {
        bail!("IP address {ip} is already recorded in the topology");
    }
    let rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("service rootfs does not exist: {}", rootfs.display()))?;
    let config = VmConfig {
        id: id.clone(),
        kernel_path: state.kernel_path.clone(),
        rootfs_path: rootfs,
        data_drive_path: None,
        memory_mb,
        vcpus,
        ip: ip.clone(),
        gateway: state.gateway.clone(),
        bridge_name: state.bridge_name.clone(),
        tap_device: network::tap_name_for_id(&id),
        extra_kernel_args: Vec::new(),
        mmds_data: Some(serde_json::json!({
            "service_config": {
                "id": id,
                "kind": kind,
                "ip": ip,
                "gateway": state.gateway,
                "port": port,
            }
        })),
    };
    let mut vm = MicroVm::spawn(config).await?;
    wait_for_tcp(&vm.config.ip, port, timeout).await?;
    let service = PersistedService {
        id: vm.config.id.clone(),
        kind,
        pid: vm.pid(),
        ip: vm.config.ip.clone(),
        port,
        tap_device: vm.config.tap_device.clone(),
    };
    vm.disable_cleanup_on_drop();
    Ok(service)
}

async fn wait_for_tcp(ip: &str, port: u16, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        if tokio::net::TcpStream::connect((ip, port)).await.is_ok() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            bail!("service at {ip}:{port} did not become reachable within {timeout:?}");
        }
        sleep(Duration::from_millis(500)).await;
    }
}

fn detach_vm_state(vm: &mut MicroVm) -> PersistedVm {
    let admin_addr = vm.admin_addr();
    vm.disable_cleanup_on_drop();
    PersistedVm {
        id: vm.config.id.clone(),
        pid: vm.pid(),
        ip: vm.config.ip.clone(),
        tap_device: vm.config.tap_device.clone(),
        admin_addr: admin_addr.clone(),
        artifact_addr: format!("{}:9091", vm.config.ip),
        proxy_addr: vm.proxy_addr(),
    }
}

async fn spawn_node_from_state(
    state: &mut PersistedClusterState,
    memory_mb: usize,
    vcpus: usize,
) -> Result<PersistedVm> {
    let (index, ip) = loop {
        let index = state.next_node_index;
        state.next_node_index = state
            .next_node_index
            .checked_add(1)
            .ok_or_else(|| anyhow!("node index space exhausted"))?;
        let ip = network::allocate_ip(&state.subnet, index + 10)
            .map_err(|e| anyhow!("failed to allocate IP: {e}"))?;
        let in_use = state.nats.iter().any(|vm| vm.ip == ip)
            || state.nodes.iter().any(|vm| vm.ip == ip)
            || state.services.iter().any(|service| service.ip == ip);
        if !in_use {
            break (index, ip);
        }
    };

    let node_id = format!("{}-node-{}", state.name, index);
    let tap = network::tap_name_for_id(&node_id);

    let config = VmConfig {
        id: node_id.clone(),
        kernel_path: state.kernel_path.clone(),
        rootfs_path: state.node_rootfs_path.clone(),
        data_drive_path: state.node_data_drive_path.clone(),
        memory_mb,
        vcpus,
        ip: ip.clone(),
        gateway: state.gateway.clone(),
        bridge_name: state.bridge_name.clone(),
        tap_device: tap,
        extra_kernel_args: Vec::new(),
        mmds_data: Some(serde_json::json!({
            "node_config": {
                "node_id": node_id,
                "nats_url": state.nats_url,
                "ip": ip,
                "gateway": state.gateway,
                "proxy_port": 8080,
                "admin_port": 9090,
                "artifact_port": 9091,
            }
        })),
    };

    let mut vm = MicroVm::spawn(config).await?;
    vm.wait_for_health(Duration::from_secs(120)).await?;
    let persisted = detach_vm_state(&mut vm);
    state.nodes.push(persisted.clone());
    Ok(persisted)
}

async fn remove_node_from_state(state: &mut PersistedClusterState, id: &str) -> Result<()> {
    let index = state
        .nodes
        .iter()
        .position(|vm| vm.id == id)
        .ok_or_else(|| anyhow!("node {id} not found"))?;
    let vm = state.nodes.remove(index);
    if process_alive(vm.pid) {
        let _ = signal_pid(vm.pid, "-TERM");
        sleep(Duration::from_secs(1)).await;
        if process_alive(vm.pid) {
            let _ = signal_pid(vm.pid, "-KILL");
        }
    }
    let _ = network::remove_tap(&vm.tap_device);
    Ok(())
}

async fn restart_node_from_state(
    state: &mut PersistedClusterState,
    id: &str,
    ebpf_test_fault: Option<EbpfTestFault>,
    ebpf_required: bool,
    expect_unhealthy: bool,
    kernel: Option<PathBuf>,
    drop_ebpf_capabilities: bool,
) -> Result<PersistedVm> {
    let index = state
        .nodes
        .iter()
        .position(|vm| vm.id == id)
        .ok_or_else(|| anyhow!("node {id} not found"))?;
    let previous = state.nodes[index].clone();

    if process_alive(previous.pid) {
        let _ = signal_pid(previous.pid, "-TERM");
        sleep(Duration::from_secs(2)).await;
        if process_alive(previous.pid) {
            let _ = signal_pid(previous.pid, "-KILL");
        }
    }
    network::remove_tap(&previous.tap_device)
        .map_err(|error| anyhow!("failed to remove TAP {}: {error}", previous.tap_device))?;

    let mut extra_kernel_args = Vec::new();
    if let Some(fault) = ebpf_test_fault {
        extra_kernel_args.push(format!("wcp.ebpf_test_fault={}", fault.as_kernel_value()));
    }
    if ebpf_required {
        extra_kernel_args.push("wcp.ebpf_required=1".to_string());
    }
    if drop_ebpf_capabilities {
        extra_kernel_args.push("wcp.ebpf_drop_capabilities=1".to_string());
    }

    let kernel_path = match kernel {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("kernel override does not exist: {}", path.display()))?,
        None => state.kernel_path.clone(),
    };

    let config = VmConfig {
        id: previous.id.clone(),
        kernel_path,
        rootfs_path: state.node_rootfs_path.clone(),
        data_drive_path: state.node_data_drive_path.clone(),
        memory_mb: state.node_memory_mb,
        vcpus: state.node_vcpus,
        ip: previous.ip.clone(),
        gateway: state.gateway.clone(),
        bridge_name: state.bridge_name.clone(),
        tap_device: previous.tap_device.clone(),
        extra_kernel_args,
        mmds_data: Some(serde_json::json!({
            "node_config": {
                "node_id": previous.id,
                "nats_url": state.nats_url,
                "ip": previous.ip,
                "gateway": state.gateway,
                "proxy_port": 8080,
                "admin_port": 9090,
                "artifact_port": 9091,
            }
        })),
    };

    let mut vm = MicroVm::spawn(config).await?;
    if expect_unhealthy {
        if vm.wait_for_health(Duration::from_secs(12)).await.is_ok() {
            bail!("node {id} became healthy but an unhealthy startup was expected");
        }
        if vm.vmm_process.try_wait()?.is_some() {
            bail!("node {id} VMM exited during the expected-unhealthy test");
        }
    } else {
        vm.wait_for_health(Duration::from_secs(120)).await?;
    }
    let persisted = detach_vm_state(&mut vm);
    state.nodes[index] = persisted.clone();
    Ok(persisted)
}

async fn deploy_app_to_state(state: &PersistedClusterState, req: DeployRequest) -> Result<()> {
    let node = select_target_vm(state, req.target_node.as_deref())?;
    let mut bus = NatsBus::connect(&state.nats_url).await?;
    bus.set_node_id("vm-testbed-cli".to_string());
    let mut http_builder = reqwest::Client::builder();
    if let Ok(token) = std::env::var("WASM_CTL_AUTH_TOKEN") {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("WASM_CTL_AUTH_TOKEN contains invalid header characters")?,
        );
        http_builder = http_builder.default_headers(headers);
    }
    let http = http_builder.build()?;

    let wasm_bytes = tokio::fs::read(&req.wasm)
        .await
        .with_context(|| format!("failed to read {}", req.wasm.display()))?;
    let sha = hex::encode(sha2::Sha256::digest(&wasm_bytes));
    let upload_url = format!("http://{}/artifacts/{}", node.artifact_addr, sha);
    let upload_resp = http
        .put(&upload_url)
        .body(wasm_bytes.clone())
        .send()
        .await?;
    if !upload_resp.status().is_success() {
        bail!("artifact upload failed: {}", upload_resp.status());
    }

    let registry_url = format!("http://{}/admin/cluster/nodes", node.admin_addr);
    let registry_resp = http.get(&registry_url).send().await?;
    let (peer_targets, manifests) = if registry_resp.status().is_success() {
        let registry: ClusterNodeRegistryResponse = registry_resp.json().await?;
        let peers = select_active_peer_node_ids(
            registry.nodes,
            Some(&node.id),
            registry.active_staleness_secs,
        );
        if peers.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let authorize_url =
                format!("http://{}/artifacts/{}/authorize", node.artifact_addr, sha);
            let response = http
                .post(&authorize_url)
                .json(&ArtifactManifestBatchRequest {
                    audiences: peers.clone(),
                })
                .send()
                .await?;
            if !response.status().is_success() {
                bail!("artifact authorize failed: {}", response.status());
            }
            let body: ArtifactManifestBatchResponse = response.json().await?;
            (peers, body.manifests)
        }
    } else {
        (Vec::new(), Vec::new())
    };

    let app_id = AppId::new_namespaced(&req.namespace, &req.app, &req.version);
    let artifact_url = format!("http://{}/artifacts/{}", node.artifact_addr, sha);
    let policy = PolicyConfig {
        network: Some(NetworkPolicyConfig {
            max_outbound_connections: Some(req.max_outbound_connections),
            allowed_cidrs: (!req.allowed_cidrs.is_empty()).then_some(req.allowed_cidrs),
            denied_cidrs: (!req.denied_cidrs.is_empty()).then_some(req.denied_cidrs),
            ..NetworkPolicyConfig::default()
        }),
        filesystem: (!req.allowed_filesystem_paths.is_empty()).then_some(FilesystemPolicyConfig {
            allowed_paths: Some(req.allowed_filesystem_paths),
            allow_file_create: Some(true),
            allow_file_delete: Some(true),
            ..FilesystemPolicyConfig::default()
        }),
    };
    let config = AppConfig {
        id: app_id.clone(),
        fuel_quota: FuelQuota(req.fuel),
        memory_limit: MemoryPages((req.memory_mb * 1024 * 1024) / 65536),
        max_instances: req.max_instances,
        idle_timeout_secs: req.idle_timeout,
        wasm_bind_port: req.bind_port,
        env_vars: req.env_vars.into_iter().collect(),
        secret_keys: req.secret_keys,
        extended_limits: None,
        health_check_path: req.health_check_path,
        db_max_connections: None,
        rate_limit: None,
        tenant_id: None,
        policy: Some(policy),
        namespace: req.namespace.clone(),
    };

    bus.publish(&Event::DeployApp {
        app_id: app_id.clone(),
        config,
        artifact_url,
        artifact_transfer_manifests: manifests,
        expected_hash: Some(sha.clone()),
        size_bytes: wasm_bytes.len() as u64,
    })
    .await?;

    if let Some(host) = req.route_host {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let route_paths = if req.route_paths.is_empty() {
            vec!["/".to_string()]
        } else {
            req.route_paths
        };
        for path_prefix in route_paths {
            if !path_prefix.starts_with('/') {
                bail!("route path must start with '/': {path_prefix}");
            }
            bus.publish(&Event::RouteAdd {
                route: Route {
                    host: host.clone(),
                    app_id: app_id.clone(),
                    path_prefix: path_prefix.clone(),
                    strip_prefix: false,
                    created_at: now,
                    updated_at: now,
                },
            })
            .await?;
            println!("Route added: {host}{path_prefix} -> {}", app_id.0);
        }
    }

    println!(
        "Deployed {} via {} (peer manifests for {} node(s))",
        app_id.0,
        node.id,
        peer_targets.len()
    );
    Ok(())
}

async fn undeploy_app_from_state(state: &PersistedClusterState, app_id: &str) -> Result<()> {
    let mut bus = NatsBus::connect(&state.nats_url).await?;
    bus.set_node_id("vm-testbed-cli".to_string());
    let app_id = AppId::new_validate(app_id).map_err(|e| anyhow!(e))?;
    bus.publish(&Event::RemoveApp {
        app_id: app_id.clone(),
    })
    .await?;
    println!("Undeploy requested for {}", app_id.0);
    Ok(())
}

fn select_target_vm<'a>(
    state: &'a PersistedClusterState,
    requested: Option<&str>,
) -> Result<&'a PersistedVm> {
    match requested {
        Some(id) => state
            .nodes
            .iter()
            .find(|vm| vm.id == id)
            .ok_or_else(|| anyhow!("node {id} not found")),
        None => state
            .nodes
            .first()
            .ok_or_else(|| anyhow!("no node available in detached topology")),
    }
}

#[derive(serde::Deserialize)]
struct ClusterNodeRegistryResponse {
    nodes: Vec<common::types::ClusterNodeRecord>,
    #[serde(default = "default_cluster_node_staleness_secs")]
    active_staleness_secs: u64,
}

fn default_cluster_node_staleness_secs() -> u64 {
    120
}

fn select_active_peer_node_ids(
    nodes: Vec<common::types::ClusterNodeRecord>,
    upload_source_node_id: Option<&str>,
    max_staleness_secs: u64,
) -> Vec<String> {
    let mut peers: Vec<String> = nodes
        .into_iter()
        .filter(|node| !node.is_stale(max_staleness_secs))
        .map(|node| node.node_id)
        .filter(|node_id| upload_source_node_id != Some(node_id.as_str()))
        .collect();
    peers.sort();
    peers
}

async fn down_from_state(state_file: &Path) -> Result<()> {
    let state = read_state(state_file)?;

    for vm in state.nats.iter().chain(state.nodes.iter()) {
        if process_alive(vm.pid) {
            let _ = signal_pid(vm.pid, "-TERM");
        }
    }
    for service in &state.services {
        if process_alive(service.pid) {
            let _ = signal_pid(service.pid, "-TERM");
        }
    }
    sleep(Duration::from_secs(1)).await;
    for vm in state.nats.iter().chain(state.nodes.iter()) {
        if process_alive(vm.pid) {
            let _ = signal_pid(vm.pid, "-KILL");
        }
    }
    for service in &state.services {
        if process_alive(service.pid) {
            let _ = signal_pid(service.pid, "-KILL");
        }
    }

    network::teardown_network(&state.bridge_name, &state.subnet)?;
    for id in state
        .nats
        .iter()
        .map(|vm| vm.id.as_str())
        .chain(state.nodes.iter().map(|vm| vm.id.as_str()))
        .chain(state.services.iter().map(|service| service.id.as_str()))
    {
        let run_dir = std::env::temp_dir().join(format!("vm-testbed-{id}"));
        if run_dir.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(run_dir);
        }
    }
    if state_file.exists() {
        std::fs::remove_file(state_file)?;
    }

    println!("Torn down {}", state.name);
    Ok(())
}

async fn print_runtime_status(state: &PersistedClusterState) -> Result<()> {
    if let Some(nats) = &state.nats {
        let alive = process_alive(nats.pid);
        let nats_addr = state
            .nats_url
            .strip_prefix("nats://")
            .unwrap_or(&state.nats_url);
        let tcp_status = match tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(nats_addr),
        )
        .await
        {
            Ok(Ok(_)) => "connected",
            Ok(Err(_)) => "unreachable",
            Err(_) => "timeout",
        };
        println!(
            "{} pid={} alive={} nats={} tcp={}",
            nats.id, nats.pid, alive, state.nats_url, tcp_status
        );
    }

    for vm in &state.nodes {
        let alive = process_alive(vm.pid);
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
    }
    for service in &state.services {
        println!(
            "{} kind={} pid={} alive={} address={}:{}",
            service.id,
            service.kind,
            service.pid,
            process_alive(service.pid),
            service.ip,
            service.port
        );
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
            "node {} admin={} artifact={} proxy={} pid={}",
            vm.id, vm.admin_addr, vm.artifact_addr, vm.proxy_addr, vm.pid
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
    Path::new("/proc").join(pid.to_string()).exists()
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

fn parse_env_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got: {s}"))
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
