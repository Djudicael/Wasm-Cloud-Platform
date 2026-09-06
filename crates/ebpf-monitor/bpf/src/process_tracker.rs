#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{map, tracepoint},
    maps::{Array, HashMap, PerCpuArray, RingBuf},
    programs::TracePointContext,
    EbpfContext,
};
use aya_log_ebpf::info;

use ebpf_monitor_bpf::*;

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(1024 * 1024, 0);

#[map]
static DROPPED_EVENTS: PerCpuArray<u64> = PerCpuArray::with_max_entries(1, 0);

#[inline(always)]
fn record_drop() {
    if let Some(value) = DROPPED_EVENTS.get_ptr_mut(0) {
        unsafe { *value = (*value).saturating_add(1) };
    }
}

/// Wasm execution threads registered by Supervisor. Wasm instances are
/// in-process, so the node TGID alone cannot identify instance activity.
#[map]
static MONITORED_TIDS: HashMap<u32, TidIdentity> = HashMap::with_max_entries(4096, 0);

/// Registration generation already announced for each TID. Comparing the
/// supervisor-provided timestamp makes TID reuse generate a fresh start event.
#[map]
static STARTED_TIDS: HashMap<u32, u64> = HashMap::with_max_entries(4096, 0);

/// The first syscall after supervisor registration is the observable workload
/// start boundary for an in-process Wasm instance.
#[tracepoint]
pub fn monitored_tid_start(ctx: TracePointContext) -> u32 {
    match try_monitored_tid_start(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_monitored_tid_start(ctx: TracePointContext) -> Result<u32, u32> {
    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let pid = (pid_tgid >> 32) as u32;
    let tid = pid_tgid as u32;
    let identity = match unsafe { MONITORED_TIDS.get(&tid) } {
        Some(identity) => identity,
        None => return Ok(0),
    };
    let generation = identity.registered_at_ns;
    if unsafe { STARTED_TIDS.get(&tid).copied() } == Some(generation) {
        return Ok(0);
    }
    let _ = STARTED_TIDS.insert(&tid, &generation, 0);

    let comm = ctx.command().unwrap_or([0; TASK_COMM_LEN]);
    let event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExec as u32,
            _padding: 0,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid,
        },
        comm,
        exit_code: 0,
        signal: 0,
        ppid: 0,
        _padding: 0,
        cgroup_id: unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() },
    };
    if let Some(mut entry) = EVENTS.reserve::<ProcessEvent>(0) {
        entry.write(event);
        entry.submit(0);
    } else {
        record_drop();
    }
    Ok(0)
}

#[tracepoint]
pub fn sched_process_exec(ctx: TracePointContext) -> u32 {
    match try_sched_process_exec(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

#[tracepoint]
pub fn sched_process_exit(ctx: TracePointContext) -> u32 {
    match try_sched_process_exit(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_sched_process_exec(ctx: TracePointContext) -> Result<u32, u32> {
    let config = CONFIG.get(0).ok_or(0u32)?;
    let node_pid = config.node_pid;

    // Read the tracepoint arguments. The format is:
    // - pid_t pid
    // - pid_t old_pid
    // We are interested in the new process's PID and its parent.
    // However, the tracepoint for exec does not directly give the parent.
    // We can get the parent PID from the current task's parent.
    // But note: the tracepoint is for the new process, and we can get its PID.
    // We'll use bpf_get_current_pid_tgid to get the current PID (the one being executed).
    // Then we can get the parent PID from the current task's parent.

    let pid = ctx.tgid();
    let tid = ctx.pid();

    // Only monitor children of the wasm-node process.
    if pid != node_pid || unsafe { MONITORED_TIDS.get(&tid) }.is_none() {
        return Ok(0);
    }

    let comm = ctx.command().unwrap_or([0; TASK_COMM_LEN]);
    let mut event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExec as u32,
            _padding: 0,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid,
        },
        comm: [0; TASK_COMM_LEN],
        exit_code: 0,
        signal: 0,
        ppid: 0,
        _padding: 0,
        cgroup_id: unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() },
    };

    // Copy the comm string
    let comm_len = comm.len().min(TASK_COMM_LEN);
    event.comm[..comm_len].copy_from_slice(&comm[..comm_len]);

    // Write to ring buffer
    if let Some(mut entry) = EVENTS.reserve::<ProcessEvent>(0) {
        entry.write(event);
        entry.submit(0);
    } else {
        record_drop();
    }

    info!(&ctx, "Process exec: pid={}", pid);

    Ok(0)
}

fn try_sched_process_exit(ctx: TracePointContext) -> Result<u32, u32> {
    let config = CONFIG.get(0).ok_or(0u32)?;
    let node_pid = config.node_pid;

    // The tracepoint arguments for sched_process_exit include:
    // - pid_t pid
    // - int prio
    // - int sig (signal number)
    // - int exit_code
    // We are interested in the PID and its parent (to see if it's a child of wasm-node).
    // However, the tracepoint does not give the parent. We can get the parent from the task.
    // But note: the process might have already been reaped. We can try to get the parent from the current task.
    // Alternatively, we can track the parent in a map when we see the exec.
    // For simplicity, we'll check if the current task's parent is the node_pid.

    let pid = ctx.tgid();
    let tid = ctx.pid();

    // Only monitor children of the wasm-node process.
    if pid != node_pid || unsafe { MONITORED_TIDS.get(&tid) }.is_none() {
        return Ok(0);
    }

    // Read the signal and exit_code from the tracepoint arguments.
    // The tracepoint context provides the arguments as an array of u64.
    // We need to know the layout. The tracepoint sched_process_exit has:
    // arg1: pid_t pid
    // arg2: int prio
    // arg3: int exit_code
    // arg4: int signal
    // However, the aya_ebpf::TracePointContext does not expose these directly.
    // We can use `ctx.read_at` to read the arguments at specific offsets.
    // The offsets are defined in the kernel source: include/trace/events/sched.h
    // We'll use the following offsets (in bytes) for the arguments:
    // - pid: offset 0 (type: pid_t)
    // - prio: offset 4 (type: int)
    // - exit_code: offset 8 (type: int)
    // - signal: offset 12 (type: int)
    // But note: the tracepoint context in aya_ebpf uses the raw tracepoint format.
    // We can use the `TracePointContext::args` method to get the arguments as a slice of u64.
    // However, the arguments are passed as u64 values, so we can read them by index.

    // sched_process_exit does not expose exit status or signal. Those fields
    // remain zero; userspace can correlate detailed process status separately.
    let exit_code = 0i32;
    let signal = 0i32;

    let comm = ctx.command().unwrap_or([0; TASK_COMM_LEN]);
    let mut event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExit as u32,
            _padding: 0,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid,
        },
        comm: [0; TASK_COMM_LEN],
        exit_code: exit_code as u32,
        signal: signal as u32,
        ppid: 0,
        _padding: 0,
        cgroup_id: unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() },
    };

    // Copy the comm string
    let comm_len = comm.len().min(TASK_COMM_LEN);
    event.comm[..comm_len].copy_from_slice(&comm[..comm_len]);

    // Write to ring buffer
    if let Some(mut entry) = EVENTS.reserve::<ProcessEvent>(0) {
        entry.write(event);
        entry.submit(0);
    } else {
        record_drop();
    }

    let _ = STARTED_TIDS.remove(&tid);

    // Log OOM kills
    if signal == 9 {
        info!(&ctx, "OOM kill detected: pid={}", pid);
    } else {
        info!(
            &ctx,
            "Process exit: pid={}, signal={}, exit_code={}", pid, signal, exit_code
        );
    }

    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
