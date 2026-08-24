#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NIGHTLY="nightly-2026-08-20"
MANIFEST="$REPO_ROOT/crates/ebpf-monitor/bpf/Cargo.toml"

command -v bpf-linker >/dev/null || {
    echo "bpf-linker is missing. Run scripts/ebpf/install-toolchain.sh first." >&2
    exit 1
}
rustup run "$NIGHTLY" rustc --version >/dev/null 2>&1 || {
    echo "$NIGHTLY is missing. Run scripts/ebpf/install-toolchain.sh first." >&2
    exit 1
}

cargo "+$NIGHTLY" build \
    -Z build-std=core \
    --manifest-path "$MANIFEST" \
    --target bpfel-unknown-none \
    --release

output="$REPO_ROOT/crates/ebpf-monitor/bpf/target/bpfel-unknown-none/release"
for object in process_tracker tcp_monitor fd_watcher mem_pressure disk_monitor syscall_counter namespace_enforcer; do
    [[ -x "$output/$object" ]] || {
        echo "Expected eBPF object is missing: $output/$object" >&2
        exit 1
    }
done

echo "eBPF objects ready in $output"
