#!/usr/bin/env bash
# Attach an application object store to a recorded local testbed.
set -euo pipefail
state_file=.vm-testbed-state.json
rootfs= ip=172.20.0.30 memory=1024
while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?}; shift 2 ;;
    --rootfs) rootfs=${2:?}; shift 2 ;;
    --ip) ip=${2:?}; shift 2 ;;
    --memory) memory=${2:?}; shift 2 ;;
    -h|--help) echo 'Usage: provision-rustfs-service.sh --state-file PATH --rootfs PRIVATE_IMAGE [--ip IP] [--memory MIB]'; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ $(uname -s) == Linux && -f "$state_file" && -f "$rootfs" ]] || { echo 'Run in WSL/Linux with existing topology and private image.' >&2; exit 1; }
export PATH="$PATH:/usr/sbin:/sbin"
[[ $(debugfs -R 'cat /etc/rustfs-image-schema-version' "$rootfs" 2>/dev/null) == 1 ]] || { echo 'Invalid RustFS image schema.' >&2; exit 1; }
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
cargo build --locked -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
args=(add-service --state-file "$(realpath "$state_file")" --id rustfs-documents --kind rustfs --ip "$ip" --port 9000 --rootfs "$(realpath "$rootfs")" --memory "$memory" --vcpus 1)
if (( EUID == 0 )); then "$cli" "${args[@]}";
elif [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then wsl.exe -u root -- "$cli" "${args[@]}";
elif [[ -n ${WSL_DISTRO_NAME:-} && -x /mnt/c/Windows/System32/wsl.exe ]]; then /mnt/c/Windows/System32/wsl.exe -u root -- "$cli" "${args[@]}";
else sudo -n "$cli" "${args[@]}"; fi
