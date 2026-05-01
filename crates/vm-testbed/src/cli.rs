//! CLI tool for the VM testbed.
//!
//! Provides commands to:
//! - Spawn individual microVMs
//! - Spawn full clusters
//! - Run health checks
//! - Inject failures (chaos testing)
//! - Teardown resources
//!
//! ## Usage
//!
//! ```bash
//! # Spawn a single node VM
//! vm-testbed-cli spawn-node --id node-1 --ip 172.20.0.2
//!
//! # Spawn a full cluster (1 NATS + 3 nodes)
//! vm-testbed-cli spawn-cluster --nodes 3
//!
//! # Check health of a running node
//! vm-testbed-cli health --ip 172.20.0.2
//!
//! # Kill a node (chaos test)
//! vm-testbed-cli kill --id node-1
//!
//! # Teardown everything
//! vm-testbed-cli teardown
//! ```

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, error};

#[derive(Parser)]
#[command(name = "vm-testbed-cli")]
#[command(about = "MicroVM testbed CLI for the Wasm Cloud Platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn a single wasm-node microVM
    SpawnNode {
        /// VM identifier
        #[arg(short, long)]
        id: String,
        /// IP address for the VM
        #[arg(short, long)]
        ip: String,
        /// Memory in MiB
        #[arg(short, long, default_value = "512")]
        memory: usize,
        /// Number of vCPUs
        #[arg(short, long, default_value = "2")]
        vcpus: usize,
        /// Path to kernel image
        #[arg(long, env = "VM_KERNEL_PATH")]
        kernel: Option<PathBuf>,
        /// Path to rootfs image
        #[arg(long, env = "VM_NODE_ROOTFS")]
        rootfs: Option<PathBuf>,
        /// NATS URL for the node to connect to
        #[arg(long, default_value = "nats://172.20.0.10:4222")]
        nats_url: String,
    },

    /// Spawn a full cluster (NATS + N nodes)
    SpawnCluster {
        /// Number of wasm-node VMs
        #[arg(short, long, default_value = "3")]
        nodes: usize,
        /// Memory per node in MiB
        #[arg(long, default_value = "512")]
        node_memory: usize,
        /// vCPUs per node
        #[arg(long, default_value = "2")]
        node_vcpus: usize,
        /// Memory for NATS VM in MiB
        #[arg(long, default_value = "256")]
        nats_memory: usize,
        /// vCPUs for NATS VM
        #[arg(long, default_value = "1")]
        nats_vcpus: usize,
    },

    /// Check health of a running node
    Health {
        /// IP address of the node
        #[arg(short, long)]
        ip: String,
        /// Admin port
        #[arg(short, long, default_value = "9090")]
        port: u16,
        /// Timeout in seconds
        #[arg(short, long, default_value = "30")]
        timeout: u64,
    },

    /// List running VMs
    List,

    /// Kill a VM (simulate power loss)
    Kill {
        /// VM identifier
        #[arg(short, long)]
        id: String,
    },

    /// Teardown all testbed resources
    Teardown {
        /// Bridge name
        #[arg(short, long, default_value = "br-wasm")]
        bridge: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::SpawnNode { id, ip, memory, vcpus, kernel, rootfs, nats_url } => {
            info!(%id, %ip, memory, vcpus, "Spawning single node");

            let kernel_path = kernel.unwrap_or_else(|| find_kernel());
            let rootfs_path = rootfs.unwrap_or_else(|| find_node_rootfs());
            let tap = format!("tap-{}", id);

            let config = vm_testbed::VmConfig {
                id: id.clone(),
                kernel_path,
                rootfs_path,
                data_drive_path: None,
                memory_mb: memory,
                vcpus,
                ip: ip.clone(),
                gateway: "172.20.0.1".to_string(),
                tap_device: tap,
                mmds_data: Some(serde_json::json!({
                    "node_config": {
                        "node_id": id,
                        "nats_url": nats_url,
                    }
                })),
            };

            let mut vm = vm_testbed::MicroVm::spawn(config).await?;
            info!(%id, "VM spawned, waiting for health...");

            vm.wait_for_health(Duration::from_secs(60)).await?;
            info!(%id, "Node is healthy!");

            // Keep running until Ctrl+C
            tokio::signal::ctrl_c().await?;
            info!("Shutting down...");
            vm.shutdown().await?;
        }

        Commands::SpawnCluster { nodes, node_memory, node_vcpus, nats_memory, nats_vcpus } => {
            info!(nodes, "Spawning cluster");

            let mut cluster = vm_testbed::ClusterFixture::new("cli-cluster").await?;
            cluster.start_nats(nats_memory, nats_vcpus).await?;

            for i in 0..nodes {
                let id = cluster.start_node(node_memory, node_vcpus).await?;
                info!(%id, "Node started");
            }

            cluster.wait_for_all_healthy(Duration::from_secs(120)).await?;
            info!("All nodes healthy!");

            // Print cluster info
            println!("\n=== Cluster Ready ===");
            println!("NATS URL: {}", cluster.nats_url()?);
            for id in cluster.node_ids() {
                if let Ok(node) = cluster.get_node(&id) {
                    println!("Node {}: admin={}, proxy={}", id, node.admin_addr(), node.proxy_addr());
                }
            }

            // Keep running until Ctrl+C
            tokio::signal::ctrl_c().await?;
            info!("Tearing down cluster...");
            cluster.teardown().await?;
        }

        Commands::Health { ip, port, timeout } => {
            let addr = format!("{}:{}", ip, port);
            info!(%addr, "Checking health...");

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?;

            let url = format!("http://{}/healthz", addr);
            let resp = client.get(&url).send().await?;

            if resp.status().is_success() {
                println!("✅ Node at {} is healthy", addr);
            } else {
                println!("❌ Node at {} returned status {}", addr, resp.status());
                std::process::exit(1);
            }
        }

        Commands::List => {
            println!("Running VMs: (not yet implemented - use `ps aux | grep firecracker`)");
        }

        Commands::Kill { id } => {
            info!(%id, "Kill command not yet implemented via CLI");
        }

        Commands::Teardown { bridge } => {
            info!(%bridge, "Tearing down network");
            vm_testbed::network::teardown_network(&bridge, "172.20.0.0/24")?;
            info!("Teardown complete");
        }
    }

    Ok(())
}

fn find_kernel() -> PathBuf {
    let candidates = [
        "./assets/vmlinux-6.1",
        "/opt/vm-testbed/vmlinux-6.1",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() { return p; }
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
        if p.exists() { return p; }
    }
    panic!("Rootfs not found. Set VM_NODE_ROOTFS.")
}
