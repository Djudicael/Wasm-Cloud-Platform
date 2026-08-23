#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
rootfs=assets/postgres-rootfs.ext4
ip=172.20.0.20
memory=512
vcpus=1

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --rootfs) rootfs=${2:?missing rootfs path}; shift 2 ;;
    --ip) ip=${2:?missing IP address}; shift 2 ;;
    --memory) memory=${2:?missing memory}; shift 2 ;;
    --vcpus) vcpus=${2:?missing vCPU count}; shift 2 ;;
    -h|--help)
      echo "Usage: provision-postgres-service.sh [--state-file PATH] [--rootfs PATH] [--ip IP]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
[[ -f "$rootfs" ]] || { echo "Missing PostgreSQL rootfs: $rootfs" >&2; exit 1; }

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
sudo -E "$cli" add-service \
  --state-file "$state_file" \
  --id oidc-postgres \
  --kind postgresql \
  --ip "$ip" \
  --port 5432 \
  --rootfs "$rootfs" \
  --memory "$memory" \
  --vcpus "$vcpus"
