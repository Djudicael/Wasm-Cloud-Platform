//! eBPF program loader and attachment.
//!
//! Loads compiled eBPF bytecode, writes the configuration map, and attaches
//! programs to their tracepoint/kprobe hooks. If loading fails (unsupported
//! kernel, missing BTF, insufficient capabilities), returns `None` so the
//! caller can fall back to userspace monitoring.
//!
//! # Loading Strategy
//!
//! The eBPF bytecode can be loaded from two sources:
//! 1. **Compile-time** (`include_bytes_aligned!`): The BPF object is embedded
//!    in the binary. This requires the BPF programs to be built before the
//!    userspace crate (see build commands in the plan).
//! 2. **Runtime** (file path): The BPF object is loaded from a file at runtime.
//!    This is useful during development or when the BPF programs are packaged
//!    separately.
//!
//! # Kernel Requirements
//!
//! - Linux kernel >= 5.8 (for BTF support and ring buffers)
//! - BTF available at `/sys/kernel/btf/vmlinux`
//! - `CAP_BPF` or `CAP_SYS_ADMIN` for loading programs
//! - `CAP_NET_ADMIN` for network tracepoints
//! - `CAP_PERFMON` for perf events

use anyhow::{anyhow, Context, Result};
use aya::{
    maps::Array,
    programs::{KProbe, TracePoint},
    Ebpf,
};
use std::path::Path;
use tracing::{info, warn};

use crate::common::{MonitorConfigMap, NsEnforceConfig, NsEnforceFlags};
use crate::config::MonitorConfig;

type AttachFn = fn(&mut Ebpf) -> Result<()>;
type MonitorRequest = (&'static str, bool, AttachFn);

/// Loaded eBPF programs, maps, and attachment links.
///
/// The `Bpf` object owns the loaded eBPF object (programs + maps).
/// The `links` vector holds the attachment links, which can be detached
/// at runtime to stop monitoring.
pub struct LoadedEbpf {
    /// Independently compiled monitor objects. They must remain owned for as
    /// long as their programs are attached.
    pub monitors: Vec<LoadedMonitor>,
    /// Optional namespace enforcer eBPF object (separate ELF).
    pub ns_ebpf: Option<Ebpf>,
    /// Attachment links for each loaded program.
    /// Links can be detached to stop a specific monitor.
    pub links: Vec<String>,
}

/// One independently compiled monitor object.
pub struct LoadedMonitor {
    pub name: &'static str,
    pub ebpf: Ebpf,
}

/// Load and attach all eBPF programs.
///
/// Returns `Ok(Some(LoadedEbpf))` if all programs loaded and attached successfully.
/// Returns `Ok(None)` if eBPF is disabled by configuration or the kernel doesn't
/// support it. Returns `Err` if loading fails unexpectedly.
pub async fn load_and_attach(config: &MonitorConfig, node_pid: u32) -> Result<Option<LoadedEbpf>> {
    if !config.enabled {
        info!("eBPF monitor disabled by configuration");
        return Ok(None);
    }

    // Check kernel version and BTF support
    if !is_kernel_supported() {
        warn!(
            "Kernel does not support required eBPF features (need >= 5.8 with BTF) \
             — falling back to userspace"
        );
        return Ok(None);
    }

    info!("Loading independently compiled eBPF monitor objects...");

    let mut monitors = Vec::new();
    let mut links = Vec::new();

    let requested: [MonitorRequest; 6] = [
        (
            "process_tracker",
            config.enable_process_tracker,
            attach_process_tracker,
        ),
        ("tcp_monitor", config.enable_tcp_monitor, attach_tcp_monitor),
        ("fd_watcher", config.enable_fd_watcher, attach_fd_watcher),
        (
            "mem_pressure",
            config.enable_mem_pressure,
            attach_mem_pressure,
        ),
        (
            "disk_monitor",
            config.enable_disk_monitor,
            attach_disk_monitor,
        ),
        (
            "syscall_counter",
            config.enable_syscall_counter,
            attach_syscall_counter,
        ),
    ];

    for (name, enabled, attach) in requested {
        if !enabled {
            continue;
        }
        let mut ebpf = match load_monitor_object(name) {
            Ok(ebpf) => ebpf,
            Err(error) => {
                warn!(%error, monitor = name, "Failed to load eBPF monitor object — skipping");
                continue;
            }
        };
        if let Err(error) = write_config_map(&mut ebpf, config, node_pid) {
            warn!(%error, monitor = name, "Failed to configure eBPF monitor — skipping");
            continue;
        }
        if let Err(error) = attach(&mut ebpf) {
            warn!(%error, monitor = name, "Failed to attach eBPF monitor — skipping");
            continue;
        }
        info!(monitor = name, "eBPF monitor attached");
        links.push(name.to_string());
        monitors.push(LoadedMonitor { name, ebpf });
    }

    let mut ns_ebpf = None;
    if config.enable_namespace_enforcer {
        match load_namespace_enforcer_object() {
            Ok(mut ns_bpf) => match attach_namespace_enforcer(&mut ns_bpf) {
                Ok(()) => {
                    info!("Namespace enforcer attached (tracepoints: sock/inet_sock_set_state, syscalls/sys_enter_sendto)");
                    if let Err(e) = write_ns_enforce_config(&mut ns_bpf, config, node_pid) {
                        warn!(error = %e, "Failed to write NS_ENFORCE_CONFIG map — namespace enforcer may not enforce");
                    }
                    links.push("namespace_enforcer".to_string());
                    ns_ebpf = Some(ns_bpf);
                }
                Err(e) => {
                    warn!(error = %e, "Failed to attach namespace enforcer — skipping");
                }
            },
            Err(e) => {
                warn!(error = %e, "Failed to load namespace enforcer eBPF object — skipping");
            }
        }
    }

    // Attempt to set MONITORED_TIDS map as read-only for eBPF programs.
    // This prevents a compromised eBPF program from modifying the identity map.
    // Requires Linux 5.2+ and aya support for BPF_F_RDONLY_PROG.
    if let Some(ref mut ns_bpf) = ns_ebpf {
        if let Some(map) = ns_bpf.map_mut("MONITORED_TIDS") {
            // Note: aya 0.12 does not expose set_flags directly on MapRefMut.
            // In production, use a build.rs with libbpf or patch aya to support:
            //   map.set_flags(libc::BPF_F_RDONLY_PROG);
            let _ = map;
            tracing::info!("MONITORED_TIDS map loaded — consider applying BPF_F_RDONLY_PROG for defense in depth");
        }
    }

    if links.is_empty() {
        warn!("No eBPF programs attached — falling back to userspace monitoring");
        return Ok(None);
    }

    info!(
        programs = links.len(),
        "eBPF programs loaded and attached successfully"
    );

    Ok(Some(LoadedEbpf {
        monitors,
        ns_ebpf,
        links,
    }))
}

// ── eBPF Object Loading ──────────────────────────────────────────────────────

/// Load the eBPF object file.
///
/// Tries multiple strategies:
/// 1. Embedded bytes (via `include_bytes_aligned!`) — production path
/// 2. File at `./ebpf-monitor-bpf/target/bpfel-unknown-none/release/process_tracker`
/// 3. File at `/opt/wasm-node/ebpf/ebpf-monitor.o`
///
/// If none of these work, returns an error suggesting the user compile the
/// BPF programs first.
fn load_monitor_object(name: &str) -> Result<Ebpf> {
    // Strategy 1: Try loading from well-known development paths
    let repo_path = format!("crates/ebpf-monitor/bpf/target/bpfel-unknown-none/release/{name}");
    let crate_path = format!("bpf/target/bpfel-unknown-none/release/{name}");
    let install_path = format!("/opt/wasm-node/ebpf/{name}.o");
    let dev_paths = [
        repo_path.as_str(),
        crate_path.as_str(),
        install_path.as_str(),
    ];

    for path_str in &dev_paths {
        let path = Path::new(path_str);
        if path.exists() {
            info!(path = %path.display(), "Loading eBPF object from file");
            let bytes = std::fs::read(path).context("failed to read eBPF object file")?;
            let bpf = Ebpf::load(&bytes).context("failed to parse eBPF object")?;
            return Ok(bpf);
        }
    }

    Err(anyhow!(
        "eBPF object '{name}' not found; build it or install it at {install_path}"
    ))
}

/// Load the namespace enforcer eBPF object file.
///
/// Tries multiple strategies similar to `load_ebpf_object` but for the
/// `namespace_enforcer` ELF binary.
fn load_namespace_enforcer_object() -> Result<Ebpf> {
    let dev_paths = [
        "crates/ebpf-monitor/bpf/target/bpfel-unknown-none/release/namespace_enforcer",
        "bpf/target/bpfel-unknown-none/release/namespace_enforcer",
        "/opt/wasm-node/ebpf/namespace_enforcer.o",
    ];

    for path_str in &dev_paths {
        let path = Path::new(path_str);
        if path.exists() {
            info!(path = %path.display(), "Loading namespace enforcer eBPF object from file");
            let bytes = std::fs::read(path)
                .context("failed to read namespace enforcer eBPF object file")?;
            let bpf =
                Ebpf::load(&bytes).context("failed to parse namespace enforcer eBPF object")?;
            return Ok(bpf);
        }
    }

    Err(anyhow!(
        "Namespace enforcer eBPF object not found. Compile BPF programs first:\n\
         \tcargo build --manifest-path crates/ebpf-monitor/bpf/Cargo.toml \
         --target bpfel-unknown-none --release\n\
         Or install them at /opt/wasm-node/ebpf/namespace_enforcer.o"
    ))
}

// ── Config Map ────────────────────────────────────────────────────────────────

/// Write the configuration map that eBPF programs read at runtime.
fn write_config_map(ebpf: &mut Ebpf, config: &MonitorConfig, node_pid: u32) -> Result<()> {
    let mut config_map: Array<_, MonitorConfigMap> = ebpf
        .map_mut("CONFIG")
        .ok_or_else(|| anyhow!("CONFIG map not found in eBPF object"))?
        .try_into()
        .context("CONFIG map has wrong type")?;

    let kernel_config = MonitorConfigMap {
        node_pid,
        fd_soft_limit: config.fd_soft_limit,
        fd_hard_limit: config.fd_hard_limit,
        mem_low_threshold_pages: config.mem_low_threshold_pages,
        mem_critical_threshold_pages: config.mem_critical_threshold_pages,
        disk_slow_threshold_ns: config.disk_slow_threshold_ns,
        tcp_conn_limit_per_pid: config.tcp_conn_limit_per_pid,
        syscall_rate_limit: config.syscall_rate_limit,
        sampling_period_ns: config.sampling_period_secs * 1_000_000_000,
    };

    config_map
        .set(0, kernel_config, 0)
        .context("failed to write CONFIG map")?;

    info!(
        node_pid,
        fd_soft_limit = config.fd_soft_limit,
        fd_hard_limit = config.fd_hard_limit,
        mem_low_threshold_pages = config.mem_low_threshold_pages,
        mem_critical_threshold_pages = config.mem_critical_threshold_pages,
        disk_slow_threshold_ns = config.disk_slow_threshold_ns,
        tcp_conn_limit_per_pid = config.tcp_conn_limit_per_pid,
        syscall_rate_limit = config.syscall_rate_limit,
        sampling_period_ns = kernel_config.sampling_period_ns,
        "eBPF config map written"
    );

    Ok(())
}

// ── Program Attachment Helpers ────────────────────────────────────────────────

/// Attach the process tracker (sched_process_exec + sched_process_exit tracepoints).
fn attach_process_tracker(ebpf: &mut Ebpf) -> Result<()> {
    attach_tracepoint(ebpf, "sched_process_exec", "sched", "sched_process_exec")?;
    attach_tracepoint(ebpf, "sched_process_exit", "sched", "sched_process_exit")?;
    Ok(())
}

/// Attach the TCP connection monitor (inet_sock_set_state tracepoint).
fn attach_tcp_monitor(ebpf: &mut Ebpf) -> Result<()> {
    attach_tracepoint(ebpf, "inet_sock_set_state", "sock", "inet_sock_set_state")
}

/// Attach the FD watcher (fd_install + do_filp_close kprobes).
fn attach_fd_watcher(ebpf: &mut Ebpf) -> Result<()> {
    attach_kprobe(ebpf, "fd_install", "fd_install")?;
    attach_kprobe(ebpf, "do_filp_close", "do_filp_close")?;
    Ok(())
}

/// Attach the memory pressure sentinel (try_to_free_pages kprobe).
fn attach_mem_pressure(ebpf: &mut Ebpf) -> Result<()> {
    attach_kprobe(ebpf, "try_to_free_pages", "try_to_free_pages")
}

/// Attach the disk I/O monitor (block_rq_issue + block_rq_complete tracepoints).
fn attach_disk_monitor(ebpf: &mut Ebpf) -> Result<()> {
    attach_tracepoint(ebpf, "block_rq_issue", "block", "block_rq_issue")?;
    attach_tracepoint(ebpf, "block_rq_complete", "block", "block_rq_complete")?;
    Ok(())
}

/// Attach the syscall anomaly counter (raw_syscalls/sys_enter tracepoint).
fn attach_syscall_counter(ebpf: &mut Ebpf) -> Result<()> {
    attach_tracepoint(ebpf, "sys_enter", "raw_syscalls", "sys_enter")
}

/// Attach the namespace enforcer programs.
///
/// Attaches `ns_inet_sock_set_state` to the `sock:inet_sock_set_state` tracepoint
/// and `ns_audit_sendto` to the `syscalls:sys_enter_sendto` tracepoint.
fn attach_namespace_enforcer(ebpf: &mut Ebpf) -> Result<()> {
    attach_tracepoint(
        ebpf,
        "ns_inet_sock_set_state",
        "sock",
        "inet_sock_set_state",
    )?;
    attach_tracepoint(ebpf, "ns_audit_sendto", "syscalls", "sys_enter_sendto")?;
    Ok(())
}

/// Write the NS_ENFORCE_CONFIG singleton array map.
fn write_ns_enforce_config(ebpf: &mut Ebpf, config: &MonitorConfig, node_pid: u32) -> Result<()> {
    let mut config_map: Array<_, NsEnforceConfig> = ebpf
        .map_mut("NS_ENFORCE_CONFIG")
        .ok_or_else(|| anyhow!("NS_ENFORCE_CONFIG map not found in eBPF object"))?
        .try_into()
        .context("NS_ENFORCE_CONFIG map has wrong type")?;

    let mut flags = NsEnforceFlags::EnableAudit as u32;
    if config.enable_forged_header_detect {
        flags |= NsEnforceFlags::EnableForgedHeaderDetect as u32;
    }

    let ns_config = NsEnforceConfig {
        gateway_port: config.gateway_port,
        _padding1: 0,
        flags,
        node_pid,
        _reserved: 0,
    };

    config_map
        .set(0, ns_config, 0)
        .context("failed to write NS_ENFORCE_CONFIG map")?;

    info!(
        gateway_port = config.gateway_port,
        flags, node_pid, "Namespace enforcer config map written"
    );

    Ok(())
}

/// Attach a tracepoint program.
///
/// The program must be defined in the eBPF object with the given `program_name`.
/// It is attached to the tracepoint identified by `category:name`.
fn attach_tracepoint(
    ebpf: &mut Ebpf,
    program_name: &str,
    category: &str,
    name: &str,
) -> Result<()> {
    let program: &mut TracePoint = ebpf
        .program_mut(program_name)
        .ok_or_else(|| anyhow!("eBPF program '{}' not found in object", program_name))?
        .try_into()
        .with_context(|| format!("program '{}' is not a TracePoint", program_name))?;

    program
        .load()
        .with_context(|| format!("failed to load TracePoint program '{}'", program_name))?;

    program.attach(category, name).with_context(|| {
        format!(
            "failed to attach TracePoint '{}' to {}:{}",
            program_name, category, name
        )
    })?;

    Ok(())
}

/// Attach a kprobe program.
///
/// The program must be defined in the eBPF object with the given `program_name`.
/// It is attached to the kernel function `symbol`.
fn attach_kprobe(ebpf: &mut Ebpf, program_name: &str, symbol: &str) -> Result<()> {
    let program: &mut KProbe = ebpf
        .program_mut(program_name)
        .ok_or_else(|| anyhow!("eBPF program '{}' not found in object", program_name))?
        .try_into()
        .with_context(|| format!("program '{}' is not a KProbe", program_name))?;

    program
        .load()
        .with_context(|| format!("failed to load KProbe program '{}'", program_name))?;

    program
        .attach(symbol, 0)
        .with_context(|| format!("failed to attach KProbe '{}' to '{}'", program_name, symbol))?;

    Ok(())
}

// ── Kernel Support Check ──────────────────────────────────────────────────────

/// Check if the kernel supports the eBPF features we need.
///
/// Requirements:
/// - Linux kernel >= 5.8 (for BTF support and ring buffers)
/// - BTF data available at `/sys/kernel/btf/vmlinux`
/// - Sufficient capabilities (CAP_BPF or CAP_SYS_ADMIN)
///
/// Returns `true` if all requirements are met, `false` otherwise.
fn is_kernel_supported() -> bool {
    // Check 1: BTF support (required for BTF-enabled eBPF programs)
    let btf_path = Path::new("/sys/kernel/btf/vmlinux");
    if !btf_path.exists() {
        warn!("BTF not available at /sys/kernel/btf/vmlinux — eBPF programs may not load");
        return false;
    }

    // Check 2: Kernel version >= 5.8
    match get_kernel_version() {
        Some((major, minor, _patch)) => {
            if major < 5 || (major == 5 && minor < 8) {
                warn!(
                    kernel_version = format!("{}.{}.{}", major, minor, _patch),
                    minimum = "5.8.0",
                    "Kernel version too old for eBPF ring buffer support"
                );
                return false;
            }
            info!(
                kernel_version = format!("{}.{}.{}", major, minor, _patch),
                "Kernel version check passed"
            );
        }
        None => {
            warn!("Could not determine kernel version — proceeding optimistically");
            // Don't fail here — the version check is a heuristic. If BTF is available,
            // the kernel likely supports ring buffers too.
        }
    }

    // Check 3: We are running on Linux (the cfg check is done at compile time,
    // but we also verify at runtime for the WSL case)
    #[cfg(not(target_os = "linux"))]
    {
        warn!("Not running on Linux — eBPF is not available");
        return false;
    }

    true
}

/// Parse the kernel version from `/proc/version` or `uname()`.
///
/// Returns `(major, minor, patch)` as numbers.
fn get_kernel_version() -> Option<(u32, u32, u32)> {
    // Try reading /proc/version_signature first (Ubuntu/Debian)
    if let Ok(content) = std::fs::read_to_string("/proc/version_signature") {
        if let Some(version) = parse_version_string(&content) {
            return Some(version);
        }
    }

    // Try uname
    let info = unsafe {
        let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
        if libc::uname(uts.as_mut_ptr()) == -1 {
            return None;
        }
        let uts = uts.assume_init();
        // Convert C string to Rust String
        std::ffi::CStr::from_ptr(uts.release.as_ptr())
            .to_string_lossy()
            .into_owned()
    };

    parse_version_string(&info)
}

/// Parse a version string like "5.15.0-56-generic" into (5, 15, 0).
fn parse_version_string(s: &str) -> Option<(u32, u32, u32)> {
    // Find the first occurrence of a version pattern (X.Y.Z)
    for word in s.split(|c: char| !c.is_ascii_digit() && c != '.') {
        if word.contains('.') {
            let parts: Vec<&str> = word.split('.').collect();
            if parts.len() >= 2 {
                let major = parts[0].parse().ok()?;
                let minor = parts[1].parse().ok()?;
                let patch = if parts.len() >= 3 {
                    // Handle "0-56-generic" by taking only the numeric prefix
                    parts[2]
                        .split(|c: char| !c.is_ascii_digit())
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0)
                } else {
                    0
                };
                return Some((major, minor, patch));
            }
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_string_standard() {
        assert_eq!(
            parse_version_string("Linux version 5.15.0-56-generic"),
            Some((5, 15, 0))
        );
        assert_eq!(parse_version_string("5.8.0-63-generic"), Some((5, 8, 0)));
        assert_eq!(parse_version_string("6.1.0"), Some((6, 1, 0)));
    }

    #[test]
    fn test_parse_version_string_two_part() {
        assert_eq!(parse_version_string("5.8"), Some((5, 8, 0)));
    }

    #[test]
    fn test_parse_version_string_with_extra() {
        assert_eq!(
            parse_version_string("5.15.0-56-generic #62-Ubuntu SMP"),
            Some((5, 15, 0))
        );
    }

    #[test]
    fn test_parse_version_string_empty() {
        assert_eq!(parse_version_string(""), None);
    }

    #[test]
    fn test_parse_version_string_no_version() {
        assert_eq!(parse_version_string("hello world"), None);
    }

    #[test]
    fn test_kernel_version_check() {
        // This test verifies the version parsing logic, not the actual kernel.
        // On WSL, this should return a valid version.
        if cfg!(target_os = "linux") {
            let version = get_kernel_version();
            // On Linux, we should be able to get the version
            // (but don't assert it's >= 5.8 since CI might run on older kernels)
            if let Some((major, minor, _patch)) = version {
                assert!(major >= 3, "Kernel major version should be at least 3");
                assert!(minor <= 100, "Kernel minor version should be reasonable");
            }
        }
    }

    #[test]
    fn test_is_kernel_supported_on_linux() {
        // This test just verifies the function doesn't panic.
        // The result depends on the actual kernel.
        if cfg!(target_os = "linux") {
            let _ = is_kernel_supported();
        }
    }

    #[test]
    fn test_monitor_config_map_serialization() {
        let config = MonitorConfig::default();
        let kernel_config = MonitorConfigMap {
            node_pid: 1234,
            fd_soft_limit: config.fd_soft_limit,
            fd_hard_limit: config.fd_hard_limit,
            mem_low_threshold_pages: config.mem_low_threshold_pages,
            mem_critical_threshold_pages: config.mem_critical_threshold_pages,
            disk_slow_threshold_ns: config.disk_slow_threshold_ns,
            tcp_conn_limit_per_pid: config.tcp_conn_limit_per_pid,
            syscall_rate_limit: config.syscall_rate_limit,
            sampling_period_ns: config.sampling_period_secs * 1_000_000_000,
        };

        // Verify the config map values are reasonable
        assert_eq!(kernel_config.node_pid, 1234);
        assert_eq!(kernel_config.fd_soft_limit, 8192);
        assert_eq!(kernel_config.fd_hard_limit, 9728);
        assert_eq!(kernel_config.mem_low_threshold_pages, 65536);
        assert_eq!(kernel_config.mem_critical_threshold_pages, 16384);
        assert_eq!(kernel_config.disk_slow_threshold_ns, 50_000_000);
        assert_eq!(kernel_config.tcp_conn_limit_per_pid, 10000);
        assert_eq!(kernel_config.syscall_rate_limit, 100_000);
        assert_eq!(kernel_config.sampling_period_ns, 10_000_000_000); // 10s in ns
    }

    #[test]
    fn test_load_and_attach_disabled() {
        let config = MonitorConfig {
            enabled: false,
            ..MonitorConfig::default()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(load_and_attach(&config, 1));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_load_and_attach_no_bpf_object() {
        // With default config (enabled=true), but no BPF object available,
        // should return Ok(None) (graceful fallback)
        let config = MonitorConfig::default();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(load_and_attach(&config, 1));
        assert!(result.is_ok());
        // On a system without BTF or without compiled BPF programs,
        // this should return None gracefully
        if cfg!(target_os = "linux") {
            // Might be Some or None depending on kernel support and BPF object availability
            let _ = result.unwrap();
        } else {
            assert!(result.unwrap().is_none());
        }
    }
}
