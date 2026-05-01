//! Host network setup for microVM connectivity.
//!
//! This module manages Linux bridge and TAP interfaces so that microVMs
//! can communicate with each other and the host. It requires `CAP_NET_ADMIN`
//! (typically via `sudo`) because it manipulates the host network stack.
//!
//! ## Network Topology
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                         Host                                 │
//! │  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐     │
//! │  │  br-wasm    │◄──►│  tap-node1  │◄──►│  MicroVM 1  │     │
//! │  │  172.20.0.1 │    │  no IP      │    │  172.20.0.2 │     │
//! │  └──────┬──────┘    └─────────────┘    └─────────────┘     │
//! │         │                                                    │
//! │  ┌──────┴──────┐    ┌─────────────┐    ┌─────────────┐     │
//! │  │  tap-node2  │◄──►│  MicroVM 2  │    │  tap-nats   │◄──►│ NATS VM
//! │  │  no IP      │    │  172.20.0.3 │    │  no IP      │    │ 172.20.0.10
//! │  └─────────────┘    └─────────────┘    └─────────────┘     │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! Each microVM gets:
//! - A TAP interface on the host (e.g., `tap-node1`)
//! - An IP in the `172.20.0.0/24` subnet
//! - NAT/MASQUERADE for outbound internet access (optional)
//!
//! ## Privilege Requirements
//!
//! - `ip link add` → `CAP_NET_ADMIN`
//! - `ip tuntap add` → `CAP_NET_ADMIN`
//! - `iptables` → `CAP_NET_ADMIN`
//!
//! Run with `sudo` or grant the capability to your test binary.

use std::process::Command;
use tracing::{debug, info};

/// Default bridge name for the Wasm testbed network.
pub const DEFAULT_BRIDGE: &str = "br-wasm";
/// Default subnet for the testbed network.
pub const DEFAULT_SUBNET: &str = "172.20.0.0/24";
/// Default bridge IP (gateway for microVMs).
pub const DEFAULT_BRIDGE_IP: &str = "172.20.0.1/24";
/// Base IP for microVM allocation.
pub const VM_IP_BASE: u8 = 2; // 172.20.0.2, .3, .4, ...

/// Error type for network operations.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("Command failed: {cmd}\nstdout: {stdout}\nstderr: {stderr}")]
    CommandFailed {
        cmd: String,
        stdout: String,
        stderr: String,
    },
    #[error("Bridge {0} already exists")]
    BridgeExists(String),
    #[error("TAP device {0} already exists")]
    TapExists(String),
    #[error("IP allocation exhausted in subnet {0}")]
    IpExhausted(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type alias for network operations.
pub type Result<T> = std::result::Result<T, NetworkError>;

/// Run a shell command and return an error if it fails.
fn run(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().to_string();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let cmd_str = format!("{} {}", program, args.join(" "));

    debug!(%cmd_str, "Running command");

    let output = cmd.output()?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // Some commands "fail" with exit code 2 when the device already exists —
        // that's not a real error for our idempotent setup.
        if stderr.contains("already exists") || stderr.contains("File exists") {
            debug!(%cmd_str, "Device already exists, ignoring");
            return Ok(());
        }
        return Err(NetworkError::CommandFailed {
            cmd: cmd_str,
            stdout,
            stderr,
        });
    }
    Ok(())
}

// ── Bridge Management ────────────────────────────────────────────────

/// Create the testbed bridge if it doesn't exist.
///
/// This sets up a Linux bridge with IP forwarding enabled so microVMs
/// can communicate with each other and (optionally) the outside world.
pub fn create_bridge(bridge_name: &str, bridge_ip: &str) -> Result<()> {
    info!(%bridge_name, %bridge_ip, "Creating bridge");

    // Check if bridge already exists
    let check = Command::new("ip")
        .args(["link", "show", bridge_name])
        .output()?;

    if check.status.success() {
        info!(%bridge_name, "Bridge already exists");
        return Ok(());
    }

    // Create bridge
    run(Command::new("ip").args(["link", "add", bridge_name, "type", "bridge"]))?;

    // Assign IP
    run(Command::new("ip").args(["addr", "add", bridge_ip, "dev", bridge_name]))?;

    // Bring up
    run(Command::new("ip").args(["link", "set", bridge_name, "up"]))?;

    // Enable IP forwarding
    run(Command::new("sysctl").args(["-w", "net.ipv4.ip_forward=1"]))?;

    info!(%bridge_name, "Bridge created and configured");
    Ok(())
}

/// Remove the testbed bridge and all associated TAP devices.
pub fn remove_bridge(bridge_name: &str) -> Result<()> {
    info!(%bridge_name, "Removing bridge");

    // Find and remove all TAP devices attached to this bridge
    let output = Command::new("ip")
        .args(["link", "show", "master", bridge_name])
        .output()?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains(":") {
                if let Some(iface) = line.split(':').nth(1) {
                    let tap = iface.trim();
                    if tap.starts_with("tap-") {
                        let _ = remove_tap(tap);
                    }
                }
            }
        }
    }

    // Bring down and delete bridge
    let _ = run(Command::new("ip").args(["link", "set", bridge_name, "down"]));
    let _ = run(Command::new("ip").args(["link", "del", bridge_name]));

    info!(%bridge_name, "Bridge removed");
    Ok(())
}

// ── TAP Device Management ────────────────────────────────────────────

/// Create a TAP device and attach it to the bridge.
///
/// # Arguments
/// * `tap_name` — Name for the TAP device (e.g., `"tap-node1"`)
/// * `bridge_name` — Bridge to attach to
pub fn create_tap(tap_name: &str, bridge_name: &str) -> Result<()> {
    info!(%tap_name, %bridge_name, "Creating TAP device");

    // Check if TAP already exists
    let check = Command::new("ip")
        .args(["link", "show", tap_name])
        .output()?;

    if check.status.success() {
        info!(%tap_name, "TAP device already exists");
        // Ensure it's attached to the bridge
        let _ = run(Command::new("ip").args(["link", "set", tap_name, "master", bridge_name]));
        return Ok(());
    }

    // Create TAP device
    run(Command::new("ip")
        .args(["tuntap", "add", tap_name, "mode", "tap"]))?;

    // Bring up
    run(Command::new("ip").args(["link", "set", tap_name, "up"]))?;

    // Attach to bridge
    run(Command::new("ip").args(["link", "set", tap_name, "master", bridge_name]))?;

    info!(%tap_name, "TAP device created and attached");
    Ok(())
}

/// Remove a TAP device.
pub fn remove_tap(tap_name: &str) -> Result<()> {
    info!(%tap_name, "Removing TAP device");
    let _ = run(Command::new("ip").args(["link", "set", tap_name, "down"]));
    let _ = run(Command::new("ip").args(["tuntap", "del", tap_name, "mode", "tap"]));
    Ok(())
}

// ── NAT / MASQUERADE ─────────────────────────────────────────────────

/// Enable NAT so microVMs can reach the internet through the host.
///
/// This adds an iptables MASQUERADE rule for the bridge subnet.
/// Required if microVMs need to download artifacts or reach external NATS.
pub fn enable_nat(bridge_name: &str, subnet: &str) -> Result<()> {
    info!(%bridge_name, %subnet, "Enabling NAT for microVMs");

    run(Command::new("iptables")
        .args(["-t", "nat", "-A", "POSTROUTING", "-s", subnet, "!", "-o", bridge_name, "-j", "MASQUERADE"]))?;

    // Allow forwarding from bridge
    run(Command::new("iptables")
        .args(["-A", "FORWARD", "-i", bridge_name, "-j", "ACCEPT"]))?;
    run(Command::new("iptables")
        .args(["-A", "FORWARD", "-o", bridge_name, "-j", "ACCEPT"]))?;

    info!("NAT enabled");
    Ok(())
}

/// Disable NAT rules (cleanup).
pub fn disable_nat(bridge_name: &str, subnet: &str) -> Result<()> {
    info!(%bridge_name, "Disabling NAT");

    let _ = run(Command::new("iptables")
        .args(["-t", "nat", "-D", "POSTROUTING", "-s", subnet, "!", "-o", bridge_name, "-j", "MASQUERADE"]));

    let _ = run(Command::new("iptables")
        .args(["-D", "FORWARD", "-i", bridge_name, "-j", "ACCEPT"]));
    let _ = run(Command::new("iptables")
        .args(["-D", "FORWARD", "-o", bridge_name, "-j", "ACCEPT"]));

    Ok(())
}

// ── IP Allocation ────────────────────────────────────────────────────

/// Allocate an IP address for a microVM.
///
/// Uses a simple counter-based allocation from the subnet base.
/// Returns the IP as a string (e.g., `"172.20.0.5"`).
pub fn allocate_ip(subnet_prefix: &str, index: u8) -> Result<String> {
    if index >= 250 {
        return Err(NetworkError::IpExhausted(subnet_prefix.to_string()));
    }
    let ip = format!("{}.{}", subnet_prefix.trim_end_matches(".0"), VM_IP_BASE + index);
    Ok(ip)
}

/// Generate a guest MAC address from an index.
///
/// Uses the locally administered address range (AA:FC prefix).
pub fn guest_mac(index: u8) -> String {
    format!("AA:FC:00:00:00:{:02X}", index + 1)
}

// ── Convenience: Full Setup / Teardown ───────────────────────────────

/// Set up the complete network environment for microVM testing.
///
/// Creates the bridge, enables IP forwarding, and optionally enables NAT.
pub fn setup_network(bridge_name: &str, bridge_ip: &str, enable_nat_outbound: bool) -> Result<()> {
    create_bridge(bridge_name, bridge_ip)?;
    if enable_nat_outbound {
        let subnet = bridge_ip.rsplit_once('/').map(|(ip, _)| format!("{}/24", ip.trim_end_matches(".1").trim_end_matches("."))).unwrap_or_else(|| "172.20.0.0/24".to_string());
        enable_nat(bridge_name, &subnet)?;
    }
    Ok(())
}

/// Tear down the complete network environment.
pub fn teardown_network(bridge_name: &str, subnet: &str) -> Result<()> {
    let _ = disable_nat(bridge_name, subnet);
    remove_bridge(bridge_name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guest_mac() {
        assert_eq!(guest_mac(0), "AA:FC:00:00:00:01");
        assert_eq!(guest_mac(1), "AA:FC:00:00:00:02");
        assert_eq!(guest_mac(254), "AA:FC:00:00:00:FF");
    }

    #[test]
    fn test_allocate_ip() {
        let ip = allocate_ip("172.20.0.0", 0).unwrap();
        assert_eq!(ip, "172.20.0.2");
        let ip = allocate_ip("172.20.0.0", 5).unwrap();
        assert_eq!(ip, "172.20.0.7");
    }
}
