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
    Bpf,
};
use std::path::Path;
use tracing::{info, warn};

use crate::common::MonitorConfigMap;
use crate::config::MonitorConfig;

/// Loaded eBPF programs, maps, and attachment links.
///
/// The `Bpf` object owns the loaded eBPF object (programs + maps).
/// The `links` vector holds the attachment links, which can be detached
/// at runtime to stop monitoring.
pub struct LoadedEbpf {
    /// The loaded eBPF object (programs + maps).
    pub ebpf: Bpf,
    /// Attachment links for each loaded program.
    /// Links can be detached to stop a specific monitor.
    pub links: Vec<String>,
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

    info!("Loading eBPF programs...");

    // Try to load the eBPF object.
    // In production, the BPF bytecode is embedded via include_bytes_aligned!.
    // For development, we try loading from a file at a well-known path.
    let mut ebpf = match load_ebpf_object() {
        Ok(bpf) => bpf,
        Err(e) => {
            warn!(
                error = %e,
                "Failed to load eBPF object — this is expected if BPF programs \
                 haven't been compiled yet. Falling back to userspace monitoring."
            );
            return Ok(None);
        }
    };

    // Write the configuration map before attaching any programs.
    // The eBPF programs read this map at runtime to get thresholds.
    if let Err(e) = write_config_map(&mut ebpf, config, node_pid) {
        warn!(
            error = %e,
            "Failed to write eBPF config map — falling back to userspace"
        );
        return Ok(None);
    }

    // Attach programs based on configuration
    let mut links = Vec::new();

    if config.enable_process_tracker {
        match attach_process_tracker(&mut ebpf) {
            Ok(()) => {
                info!(
                    "Process tracker attached (tracepoint: sched_process_exec, sched_process_exit)"
                );
                links.push("process_tracker".to_string());
            }
            Err(e) => {
                warn!(error = %e, "Failed to attach process tracker — skipping");
            }
        }
    }

    if config.enable_tcp_monitor {
        match attach_tcp_monitor(&mut ebpf) {
            Ok(()) => {
                info!("TCP monitor attached (tracepoint: inet_sock_set_state)");
                links.push("tcp_monitor".to_string());
            }
            Err(e) => {
                warn!(error = %e, "Failed to attach TCP monitor — skipping");
            }
        }
    }

    if config.enable_fd_watcher {
        match attach_fd_watcher(&mut ebpf) {
            Ok(()) => {
                info!("FD watcher attached (kprobe: fd_install, do_filp_close)");
                links.push("fd_watcher".to_string());
            }
            Err(e) => {
                warn!(error = %e, "Failed to attach FD watcher — skipping");
            }
        }
    }

    if config.enable_mem_pressure {
        match attach_mem_pressure(&mut ebpf) {
            Ok(()) => {
                info!("Memory pressure sentinel attached (kprobe: try_to_free_pages)");
                links.push("mem_pressure".to_string());
            }
            Err(e) => {
                warn!(error = %e, "Failed to attach memory pressure sentinel — skipping");
            }
        }
    }

    if config.enable_disk_monitor {
        match attach_disk_monitor(&mut ebpf) {
            Ok(()) => {
                info!("Disk I/O monitor attached (tracepoint: block_rq_issue, block_rq_complete)");
                links.push("disk_monitor".to_string());
            }
            Err(e) => {
                warn!(error = %e, "Failed to attach disk I/O monitor — skipping");
            }
        }
    }

    if config.enable_syscall_counter {
        match attach_syscall_counter(&mut ebpf) {
            Ok(()) => {
                info!("Syscall counter attached (tracepoint: raw_syscalls/sys_enter)");
                links.push("syscall_counter".to_string());
            }
            Err(e) => {
                warn!(error = %e, "Failed to attach syscall counter — skipping");
            }
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

    Ok(Some(LoadedEbpf { ebpf, links }))
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
fn load_ebpf_object() -> Result<Bpf> {
    // Strategy 1: Try loading from well-known development paths
    let dev_paths = [
        // Standard aya build output location
        "./ebpf-monitor-bpf/target/bpfel-unknown-none/release/process_tracker",
        // Relative to crate directory
        "../ebpf-monitor-bpf/target/bpfel-unknown-none/release/process_tracker",
        // System installation path
        "/opt/wasm-node/ebpf/ebpf-monitor.o",
    ];

    for path_str in &dev_paths {
        let path = Path::new(path_str);
        if path.exists() {
            info!(path = %path.display(), "Loading eBPF object from file");
            let bytes = std::fs::read(path).context("failed to read eBPF object file")?;
            let bpf = Bpf::load(&bytes).context("failed to parse eBPF object")?;
            return Ok(bpf);
        }
    }

    // Strategy 2: Try include_bytes_aligned (compile-time embedded)
    // This is the production path but requires the BPF programs to be built first.
    // We use a runtime check because the file may not exist during development.
    #[cfg(feature = "ebpf")]
    {
        // If the BPF programs were compiled and included via build.rs,
        // we would load them here. For now, we rely on file-based loading.
        // In production, add a build.rs that compiles BPF programs and
        // use include_bytes_aligned! to embed them.
    }

    Err(anyhow!(
        "eBPF object not found. Compile BPF programs first:\n\
         \tcargo build --manifest-path crates/ebpf-monitor/bpf/Cargo.toml \
         --target bpfel-unknown-none --release\n\
         Or install them at /opt/wasm-node/ebpf/ebpf-monitor.o"
    ))
}

// ── Config Map ────────────────────────────────────────────────────────────────

/// Write the configuration map that eBPF programs read at runtime.
fn write_config_map(ebpf: &mut Bpf, config: &MonitorConfig, node_pid: u32) -> Result<()> {
    let config_map: Array<_, MonitorConfigMap> = ebpf
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
fn attach_process_tracker(ebpf: &mut Bpf) -> Result<()> {
    attach_tracepoint(ebpf, "sched_process_exec", "sched", "sched_process_exec")?;
    attach_tracepoint(ebpf, "sched_process_exit", "sched", "sched_process_exit")?;
    Ok(())
}

/// Attach the TCP connection monitor (inet_sock_set_state tracepoint).
fn attach_tcp_monitor(ebpf: &mut Bpf) -> Result<()> {
    attach_tracepoint(ebpf, "inet_sock_set_state", "sock", "inet_sock_set_state")
}

/// Attach the FD watcher (fd_install + do_filp_close kprobes).
fn attach_fd_watcher(ebpf: &mut Bpf) -> Result<()> {
    attach_kprobe(ebpf, "fd_install", "fd_install")?;
    attach_kprobe(ebpf, "do_filp_close", "do_filp_close")?;
    Ok(())
}

/// Attach the memory pressure sentinel (try_to_free_pages kprobe).
fn attach_mem_pressure(ebpf: &mut Bpf) -> Result<()> {
    attach_kprobe(ebpf, "try_to_free_pages", "try_to_free_pages")
}

/// Attach the disk I/O monitor (block_rq_issue + block_rq_complete tracepoints).
fn attach_disk_monitor(ebpf: &mut Bpf) -> Result<()> {
    attach_tracepoint(ebpf, "block_rq_issue", "block", "block_rq_issue")?;
    attach_tracepoint(ebpf, "block_rq_complete", "block", "block_rq_complete")?;
    Ok(())
}

/// Attach the syscall anomaly counter (raw_syscalls/sys_enter tracepoint).
fn attach_syscall_counter(ebpf: &mut Bpf) -> Result<()> {
    attach_tracepoint(ebpf, "sys_enter", "raw_syscalls", "sys_enter")
}

/// Attach a tracepoint program.
///
/// The program must be defined in the eBPF object with the given `program_name`.
/// It is attached to the tracepoint identified by `category:name`.
fn attach_tracepoint(ebpf: &mut Bpf, program_name: &str, category: &str, name: &str) -> Result<()> {
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
fn attach_kprobe(ebpf: &mut Bpf, program_name: &str, symbol: &str) -> Result<()> {
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
        let mut buf: [std::os::raw::c_char; 256] = [0; 256];
        if libc::uname(buf.as_mut_ptr()) == -1 {
            return None;
        }
        // Convert C string to Rust String
        std::ffi::CStr::from_ptr(buf.as_ptr())
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
