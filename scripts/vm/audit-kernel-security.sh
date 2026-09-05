#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --kernel <vmlinux> --config <config> [--output <json>]" >&2
}

kernel=
config=
output=/dev/stdout
while [[ $# -gt 0 ]]; do
  case "$1" in
    --kernel) kernel=${2:-}; shift 2 ;;
    --config) config=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done

[[ -f "$kernel" ]] || { echo "kernel not found: $kernel" >&2; exit 1; }
[[ -f "$config" ]] || { echo "kernel config not found: $config" >&2; exit 1; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kernel-testbed.env
source "$script_dir/kernel-testbed.env"

required=(
  CONFIG_64BIT=y CONFIG_SMP=y CONFIG_HYPERVISOR_GUEST=y CONFIG_PARAVIRT=y
  CONFIG_KVM_GUEST=y CONFIG_PARAVIRT_CLOCK=y CONFIG_BLOCK=y
  CONFIG_VIRTIO=y CONFIG_VIRTIO_BLK=y CONFIG_VIRTIO_NET=y CONFIG_VIRTIO_MMIO=y
  CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y CONFIG_EXT4_FS=y
  CONFIG_BINFMT_ELF=y CONFIG_BINFMT_SCRIPT=y CONFIG_UNIX=y
  CONFIG_SYSVIPC=y CONFIG_POSIX_MQUEUE=y CONFIG_SHMEM=y CONFIG_TMPFS=y
  CONFIG_CGROUPS=y CONFIG_CGROUP_BPF=y CONFIG_NAMESPACES=y
  CONFIG_SECCOMP=y CONFIG_SECCOMP_FILTER=y
  CONFIG_BPF=y CONFIG_BPF_SYSCALL=y CONFIG_BPF_JIT=y
  CONFIG_BPF_JIT_ALWAYS_ON=y CONFIG_BPF_UNPRIV_DEFAULT_OFF=y CONFIG_BPF_EVENTS=y
  CONFIG_KALLSYMS=y CONFIG_KPROBES=y CONFIG_KPROBE_EVENTS=y
  CONFIG_UPROBES=y CONFIG_UPROBE_EVENTS=y CONFIG_FTRACE=y CONFIG_TRACING=y
  CONFIG_FTRACE_SYSCALLS=y CONFIG_DEBUG_INFO=y CONFIG_DEBUG_INFO_BTF=y
  CONFIG_CPU_MITIGATIONS=y CONFIG_MITIGATION_PAGE_TABLE_ISOLATION=y
  CONFIG_MITIGATION_RETPOLINE=y
  CONFIG_RANDOMIZE_BASE=y CONFIG_STACKPROTECTOR_STRONG=y CONFIG_FORTIFY_SOURCE=y
  CONFIG_SLAB_FREELIST_HARDENED=y CONFIG_SLAB_FREELIST_RANDOM=y
  CONFIG_INIT_ON_ALLOC_DEFAULT_ON=y CONFIG_INIT_ON_FREE_DEFAULT_ON=y
  CONFIG_STRICT_KERNEL_RWX=y
)
forbidden=(CONFIG_MODULES CONFIG_DEVMEM CONFIG_DEVKMEM CONFIG_KEXEC CONFIG_KEXEC_FILE CONFIG_HIBERNATION)

failures=()
for setting in "${required[@]}"; do
  grep -Fqx "$setting" "$config" || failures+=("required:$setting")
done
for symbol in "${forbidden[@]}"; do
  if grep -Eq "^${symbol}=" "$config"; then
    failures+=("forbidden-enabled:$symbol")
  fi
done
readelf -h "$kernel" >/dev/null 2>&1 || failures+=("kernel:not-elf")
readelf -S "$kernel" | grep -F '.BTF' >/dev/null || failures+=("kernel:missing-btf")
strings "$kernel" | grep -F "Linux version $KERNEL_VERSION" >/dev/null || failures+=("kernel:version-mismatch")

kernel_sha=$(sha256sum "$kernel" | awk '{print $1}')
config_sha=$(sha256sum "$config" | awk '{print $1}')
status=pass
[[ ${#failures[@]} -eq 0 ]] || status=fail

mkdir -p "$(dirname "$output")"
STATUS="$status" KERNEL_PATH="$kernel" CONFIG_PATH="$config" \
KERNEL_SHA="$kernel_sha" CONFIG_SHA="$config_sha" FAILURES="$(printf '%s\n' "${failures[@]-}")" \
KERNEL_VERSION_VALUE="$KERNEL_VERSION" KERNEL_SCHEMA_VALUE="$KERNEL_SCHEMA" \
FC_VERSION_VALUE="$FIRECRACKER_VERSION" FC_COMMIT_VALUE="$FIRECRACKER_CONFIG_COMMIT" \
FC_CONFIG_SHA_VALUE="$FIRECRACKER_CONFIG_SHA256" python3 - "$output" <<'PY'
import json
import os
from pathlib import Path
import sys

failures = [line for line in os.environ["FAILURES"].splitlines() if line]
document = {
    "schema_version": 1,
    "status": os.environ["STATUS"],
    "kernel": {
        "version": os.environ["KERNEL_VERSION_VALUE"],
        "image_schema": int(os.environ["KERNEL_SCHEMA_VALUE"]),
        "path": os.environ["KERNEL_PATH"],
        "sha256": os.environ["KERNEL_SHA"],
        "config_path": os.environ["CONFIG_PATH"],
        "config_sha256": os.environ["CONFIG_SHA"],
    },
    "firecracker_compatibility": {
        "minimum_version": os.environ["FC_VERSION_VALUE"],
        "config_commit": os.environ["FC_COMMIT_VALUE"],
        "base_config_sha256": os.environ["FC_CONFIG_SHA_VALUE"],
    },
    "checks": {
        "static_config": "pass" if not failures else "fail",
        "elf": "pass" if "kernel:not-elf" not in failures else "fail",
        "btf": "pass" if "kernel:missing-btf" not in failures else "fail",
        "embedded_version": "pass" if "kernel:version-mismatch" not in failures else "fail",
    },
    "failures": failures,
    "runtime_mitigation_audit_required_per_host_class": True,
}
payload = json.dumps(document, indent=2, sort_keys=True) + "\n"
if sys.argv[1] == "/dev/stdout":
    sys.stdout.write(payload)
else:
    Path(sys.argv[1]).write_text(payload, encoding="utf-8")
PY

if [[ "$status" != pass ]]; then
  printf 'kernel security audit failed: %s\n' "${failures[*]}" >&2
  exit 1
fi
