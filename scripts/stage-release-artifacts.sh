#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <output-directory>" >&2
  exit 1
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  echo "refusing to stage a release from a dirty source tree" >&2
  git status --short >&2
  exit 1
fi

output_dir="$1"
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
bpf_dir="$repo_root/crates/ebpf-monitor/bpf/target/bpfel-unknown-none/release"

rm -rf -- "$output_dir"
mkdir -p "$output_dir/ebpf"
install -m 0755 "$target_dir/release/wasm-node" "$output_dir/wasm-node"
install -m 0755 "$target_dir/release/wasm-ctl" "$output_dir/wasm-ctl"
install -m 0755 "$target_dir/release/wasm-deploy-ingress" "$output_dir/wasm-deploy-ingress"
install -m 0644 "$target_dir/wasm32-wasip2/release/hello-axum.wasm" "$output_dir/hello-axum.wasm"

for object in process_tracker tcp_monitor fd_watcher mem_pressure disk_monitor syscall_counter namespace_enforcer; do
  [[ -f "$bpf_dir/$object" ]] || { echo "missing eBPF release object: $bpf_dir/$object" >&2; exit 1; }
  install -m 0644 "$bpf_dir/$object" "$output_dir/ebpf/$object"
done
