#![no_std]
#![no_main]

use aya_ebpf::{
    cty::c_int,
    macros::{map, tracepoint},
    maps::{Array, PerCpuArray, RingBuf},
    programs::TracePointContext,
};
use aya_log_ebpf::info;

use ebpf_monitor_bpf_common::*;

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(1024 * 1024, 0);

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
    let config = CONFIG.get(0).ok_or(0)?;
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

    let pid = ctx.pid();
    let ppid = ctx.ppid();

    // Only monitor children of the wasm-node process.
    if ppid != node_pid {
        return Ok(0);
    }

    let comm = ctx.comm();
    let mut event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExec as u32,
            timestamp_ns: aya_ebpf::helpers::bpf_ktime_get_ns(),
            pid,
            tid: ctx.tid(),
        },
        comm: [0; TASK_COMM_LEN],
        exit_code: 0,
        signal: 0,
        ppid,
        cgroup_id: 0, // TODO: get cgroup_id if available
    };

    // Copy the comm string
    let comm_len = comm.len().min(TASK_COMM_LEN);
    event.comm[..comm_len].copy_from_slice(&comm[..comm_len]);

    // Write to ring buffer
    if EVENTS.reserve::<ProcessEvent>(0).is_err() {
        return Ok(0);
    }

    unsafe {
        let event_ptr = EVENTS.data_ptr_mut() as *mut ProcessEvent;
        core::ptr::write_volatile(event_ptr, event);
        EVENTS.submit(event_ptr as *mut u8, 0);
    }

    info!(&ctx, "Process exec: pid={}, comm={:?}", pid, comm);

    Ok(0)
}

fn try_sched_process_exit(ctx: TracePointContext) -> Result<u32, u32> {
    let config = CONFIG.get(0).ok_or(0)?;
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

    let pid = ctx.pid();
    let ppid = ctx.ppid();

    // Only monitor children of the wasm-node process.
    if ppid != node_pid {
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

    let args = ctx.args();
    if args.len() < 4 {
        return Ok(0);
    }

    let _prio = args[1] as i32;
    let exit_code = args[2] as i32;
    let signal = args[3] as i32;

    let comm = ctx.comm();
    let mut event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExit as u32,
            timestamp_ns: aya_ebpf::helpers::bpf_ktime_get_ns(),
            pid,
            tid: ctx.tid(),
        },
        comm: [0; TASK_COMM_LEN],
        exit_code: exit_code as u32,
        signal: signal as u32,
        ppid,
        cgroup_id: 0, // TODO: get cgroup_id if available
    };

    // Copy the comm string
    let comm_len = comm.len().min(TASK_COMM_LEN);
    event.comm[..comm_len].copy_from_slice(&comm[..comm_len]);

    // Write to ring buffer
    if EVENTS.reserve::<ProcessEvent>(0).is_err() {
        return Ok(0);
    }

    unsafe {
        let event_ptr = EVENTS.data_ptr_mut() as *mut ProcessEvent;
        core::ptr::write_volatile(event_ptr, event);
        EVENTS.submit(event_ptr as *mut u8, 0);
    }

    // Log OOM kills
    if signal == 9 {
        info!(&ctx, "OOM kill detected: pid={}, comm={:?}", pid, comm);
    } else {
        info!(
            &ctx,
            "Process exit: pid={}, comm={:?}, signal={}, exit_code={}",
            pid,
            comm,
            signal,
            exit_code
        );
    }

    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
