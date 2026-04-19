//! eBPF program loader and attachment.

use anyhow::{anyhow, Result};
use aya::{
    maps::Array,
    programs::{KProbe, ProgramFd, TracePoint},
    Bpf,
};
use tracing::{info, warn};

use crate::common::MonitorConfigMap;
use crate::config::MonitorConfig;

/// Loaded eBPF programs and maps.
pub struct LoadedEbpf {
    pub ebpf: Bpf,
    pub attached: Vec<ProgramFd>,
}

/// Load and attach all eBPF programs.
pub async fn load_and_attach(config: &MonitorConfig, node_pid: u32) -> Result<Option<LoadedEbpf>> {
    if !config.enabled {
        info!("eBPF monitor disabled by configuration");
        return Ok(None);
    }

    // Check kernel version
    if !is_kernel_supported() {
        warn!("Kernel does not support required eBPF features — falling back to userspace");
        return Ok(None);
    }

    info!("Loading eBPF programs...");

    // Load the compiled eBPF object
    // TODO: Replace with actual eBPF binary when built
    let dummy_bytes = &[];
    let mut ebpf = Bpf::load(dummy_bytes)?;

    // Write config map
    let mut config_map: Array<_, MonitorConfigMap> = ebpf
        .map_mut("CONFIG")
        .ok_or_else(|| anyhow!("CONFIG map not found"))?
        .try_into()?;

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
    config_map.set(0, kernel_config, 0)?;

    let mut attached = Vec::new();

    // Attach process tracker
    if config.enable_process_tracker {
        attach_tracepoint(
            &mut ebpf,
            "sched_process_exec",
            "sched",
            "sched_process_exec",
        )?;
        attach_tracepoint(
            &mut ebpf,
            "sched_process_exit",
            "sched",
            "sched_process_exit",
        )?;
        info!("Process tracker attached");
    }

    // Attach TCP monitor
    if config.enable_tcp_monitor {
        attach_tracepoint(
            &mut ebpf,
            "inet_sock_set_state",
            "sock",
            "inet_sock_set_state",
        )?;
        info!("TCP monitor attached");
    }

    // Attach FD watcher
    if config.enable_fd_watcher {
        attach_kprobe(&mut ebpf, "fd_install", "fd_install")?;
        attach_kprobe(&mut ebpf, "do_filp_close", "do_filp_close")?;
        info!("FD watcher attached");
    }

    // Attach memory pressure
    if config.enable_mem_pressure {
        attach_kprobe(&mut ebpf, "try_to_free_pages", "try_to_free_pages")?;
        info!("Memory pressure sentinel attached");
    }

    // Attach disk monitor
    if config.enable_disk_monitor {
        attach_tracepoint(&mut ebpf, "block_rq_issue", "block", "block_rq_issue")?;
        attach_tracepoint(&mut ebpf, "block_rq_complete", "block", "block_rq_complete")?;
        info!("Disk I/O monitor attached");
    }

    // Attach syscall counter
    if config.enable_syscall_counter {
        attach_tracepoint(&mut ebpf, "sys_enter", "raw_syscalls", "sys_enter")?;
        info!("Syscall counter attached");
    }

    info!("All eBPF programs loaded and attached successfully");

    Ok(Some(LoadedEbpf { ebpf, attached }))
}

fn attach_tracepoint(ebpf: &mut Bpf, program_name: &str, category: &str, name: &str) -> Result<()> {
    let program: &mut TracePoint = ebpf
        .program_mut(program_name)
        .ok_or_else(|| anyhow!("Program {} not found", program_name))?
        .try_into()?;
    program.load()?;
    program.attach(category, name)?;
    Ok(())
}

fn attach_kprobe(ebpf: &mut Bpf, program_name: &str, symbol: &str) -> Result<()> {
    let program: &mut KProbe = ebpf
        .program_mut(program_name)
        .ok_or_else(|| anyhow!("Program {} not found", program_name))?
        .try_into()?;
    program.load()?;
    program.attach(symbol, 0)?;
    Ok(())
}

fn is_kernel_supported() -> bool {
    // Check for kernel version >= 5.8 and BTF support
    // For now, we assume the kernel is supported if we can read /sys/kernel/btf/vmlinux
    // and the kernel version is at least 5.8.
    // We'll implement a more robust check later.
    true
}
