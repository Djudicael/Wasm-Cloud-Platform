# P10-06 VM-testbed guest-kernel evidence

Date: 2026-09-01  
Status: **TESTBED PASS / HOST SECURITY OBSERVATION, NOT A PLATFORM BLOCKER**

## Validated compatibility unit

- kernel.org Linux LTS: `6.18.48`
- source SHA-256: `5ebdadb10a4b5708fc6b1c457764a110bc49f8150cc3502c59b921ead8c6fc8c`
- Firecracker: checksum-verified `v1.16.1`
- Firecracker x86_64 6.18 base config commit:
  `a6146c8bb2139ead4ae2470fd15085629d02236e`
- Firecracker base config SHA-256:
  `ba22401a0c7292a4c024ebcd10a562d4a1f1bfd2faed671406d3b159c0cf5215`
- testbed kernel schema: `8`
- built `assets/vmlinux-6.18` SHA-256:
  `b6486c8e3fc5f9c251ddbc38289cc3154891e564dc7c04709df5e1c07fedf0b9`
- resolved config SHA-256:
  `a66273484e73776823b0a7eb0c8fa1d7755872876d184a007c9370308f888a17`

The source archive checksum matches kernel.org's signed checksum manifest. The
Firecracker guest config is pinned by both commit and checksum. The build does
not accept an arbitrary version argument.

## Passed gates

- clean compilation of upstream Linux 6.18.48 using Firecracker's maintained
  6.18 x86_64 guest config plus the testbed overlay;
- resolved boot, runtime, eBPF, tracepoint/kprobe, BTF, and hardening contract;
- CPU mitigations, page-table isolation, retpoline, KASLR, strong stack
  protector, fortify, freelist hardening/randomization, initialization on
  allocation/free, and strict kernel W^X;
- modules, `/dev/mem`, kexec, and hibernation disabled;
- static audit positive path and negative missing-mitigation rejection;
- runtime-audit parser positive path and vulnerable-status rejection;
- deterministic testbed-kernel build and tamper-rejection tests;
- current 6.18 LTS patch check as a testbed-maintenance signal;
- `cargo fmt --all -- --check`, the required workspace all-target native check,
  workspace all-target Clippy with warnings denied, and 9/9 `vm-testbed`
  library tests in WSL.

## Fail-closed observation

The first audit correctly rejected the compiled kernel because Linux 6.18 uses
the resolved symbols `CONFIG_MITIGATION_PAGE_TABLE_ISOLATION` and
`CONFIG_MITIGATION_RETPOLINE`, an absent `CONFIG_DEVKMEM` is safer than an
explicit value, and `pipefail` made early-exit `grep -q` unsuitable for ELF
section checks. The policy was corrected to audit the resolved 6.18 symbols,
reject only actually enabled forbidden symbols, and consume complete ELF tool
output. The same binary then passed; the failure was not waived.

## Runtime canary result

The user selected one smoke platform node plus the separate NATS microVM, with
direct node access and no HAProxy. The state-scoped canary booted the exact
artifacts above under Firecracker 1.16.1:

- `local-test-node-0` returned HTTP 200 from `/healthz`; its disk check was
  independently degraded because the disposable 2-GiB rootfs is below the
  configured warning envelope, while redb, memory, and eBPF were healthy;
- NATS was alive and its TCP endpoint connected;
- Linux `6.18.48` passed the loader version check;
- all seven eBPF programs loaded and attached: `process_tracker`,
  `tcp_monitor`, `fd_watcher`, `mem_pressure`, `disk_monitor`,
  `syscall_counter`, and `namespace_enforcer`;
- metrics reported `wasm_ebpf_active 1` and
  `wasm_ebpf_monitoring_degraded 0`;
- 18 of 19 emitted vulnerability records were `Not affected` or an explicit
  mitigation. `spec_rstack_overflow` failed with
  `Vulnerable: Safe RET, no microcode`.

The Linux SRSO documentation says this state applies Safe-RET to protect the
kernel but lacks the IBPB-extending microcode, so user-space tasks may remain
vulnerable. The audit therefore remains fail-closed. Supplying
`spec_rstack_overflow=safe-ret` would not fix it because that software path is
already selected; the host/virtualization layer must expose updated microcode
capability if that specific test host needs to pass the diagnostic.

The WSL layer itself reports the same SRSO state and additionally reports
`tsa: Vulnerable: No microcode`; this reinforces that WSL is useful functional
evidence but cannot qualify another VPS host class. It is not a platform source
defect or a platform production blocker.

This WSL result validates only the represented CPU/virtualization class. The
platform does not ship this kernel. A production operator validates the actual
VPS/host kernel, patching policy, cgroups, and optional eBPF prerequisites. See
`INFRA_IMPL/process/VM_TESTBED_KERNEL_VALIDATION.md`.

The redacted/runtime artifacts in `runtime/` include the audit JSON, serial
log, eBPF attach evidence, authenticated metrics, health response, state-driven
status, host/CPU identity, and artifact hashes. The disposable topology was
then removed using only `.p10-06-kernel-state.json`.
