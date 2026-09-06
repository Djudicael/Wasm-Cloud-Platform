#!/usr/bin/env bash
# Build the pinned, audited guest kernel used only by the local VM testbed.
# This is not a Wasm Cloud Platform release artifact. Run on Linux/WSL2 x86_64.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd "$script_dir/../.." && pwd)"
# shellcheck source=kernel-testbed.env
source "$script_dir/kernel-testbed.env"

if [[ $# -gt 1 ]]; then
  echo "usage: $0 [exact-pinned-version]" >&2
  exit 2
fi
if [[ $# -eq 1 && "$1" != "$KERNEL_VERSION" ]]; then
  echo "refusing unpinned kernel version $1; update kernel-testbed.env as one reviewed testbed compatibility unit" >&2
  exit 1
fi
[[ $(uname -m) == x86_64 ]] || {
  echo "the VM-testbed guest-kernel profile currently supports x86_64 only" >&2
  exit 1
}

output_dir="${OUTPUT_DIR:-$project_root/assets}"
jobs="${JOBS:-$(nproc)}"
cache_dir="${KERNEL_BUILD_CACHE_DIR:-/tmp/wasm-cloud-platform-kernel}"
source_archive="$cache_dir/linux-$KERNEL_VERSION.tar.xz"
kernel_dir="$cache_dir/linux-$KERNEL_VERSION"
base_config="$cache_dir/firecracker-$FIRECRACKER_CONFIG_COMMIT-x86_64-6.18.config"
output="$output_dir/vmlinux-$KERNEL_SERIES"
source_url="https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz"
config_url="https://raw.githubusercontent.com/firecracker-microvm/firecracker/$FIRECRACKER_CONFIG_COMMIT/resources/guest_configs/microvm-kernel-ci-x86_64-6.18.config"

echo "=== Building audited Linux $KERNEL_VERSION guest kernel ==="
missing=()
for command in bc bison flex gcc make openssl pahole readelf sha256sum; do
  command -v "$command" >/dev/null 2>&1 || missing+=("$command")
done
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "missing kernel build commands: ${missing[*]}" >&2
  echo "Ubuntu/WSL: sudo apt-get install build-essential bc bison flex libssl-dev libelf-dev dwarves" >&2
  exit 1
fi

mkdir -p "$cache_dir" "$output_dir"
if [[ ! -f "$source_archive" ]] || ! echo "$KERNEL_SOURCE_SHA256  $source_archive" | sha256sum --check --status; then
  rm -f -- "$source_archive"
  curl --fail --location --silent --show-error "$source_url" -o "$source_archive"
fi
echo "$KERNEL_SOURCE_SHA256  $source_archive" | sha256sum --check --status || {
  echo "kernel.org source checksum mismatch for $source_archive" >&2
  exit 1
}

if [[ ! -f "$base_config" ]] || ! echo "$FIRECRACKER_CONFIG_SHA256  $base_config" | sha256sum --check --status; then
  rm -f -- "$base_config"
  curl --fail --location --silent --show-error "$config_url" -o "$base_config"
fi
echo "$FIRECRACKER_CONFIG_SHA256  $base_config" | sha256sum --check --status || {
  echo "Firecracker base-config checksum mismatch for $base_config" >&2
  exit 1
}

if [[ ${KERNEL_REUSE_BUILD_DIR:-0} != 1 || ! -f "$kernel_dir/Makefile" ]]; then
  rm -rf -- "$kernel_dir"
  tar -xf "$source_archive" -C "$cache_dir"
else
  echo "Reusing local kernel build tree (developer-only optimization)."
fi
cd "$kernel_dir"

# Firecracker documents that its checked-in config targets the Amazon Linux
# microVM kernel. Applying it to upstream LTS is a checked compatibility
# operation: olddefconfig resolves it and the independent audit below fails if
# any boot, eBPF, or hardening contract is lost.
cp "$base_config" .config
cat "$script_dir/kernel/testbed-x86_64.fragment" >> .config
make olddefconfig

echo "Building with $jobs parallel jobs..."
make -j"$jobs" vmlinux
cp vmlinux "$output"
# Retain BTF for CO-RE/eBPF while dropping bulky ordinary DWARF.
objcopy --strip-debug --keep-section=.BTF --keep-section=.BTF_ids "$output"
cp .config "$output.config"
printf '%s\n' "$KERNEL_SCHEMA" > "$output.schema"

SOURCE_URL="$source_url" SOURCE_SHA="$KERNEL_SOURCE_SHA256" \
CONFIG_URL="$config_url" CONFIG_SHA="$FIRECRACKER_CONFIG_SHA256" \
FC_COMMIT="$FIRECRACKER_CONFIG_COMMIT" FC_VERSION="$FIRECRACKER_VERSION" \
KERNEL_VERSION_VALUE="$KERNEL_VERSION" python3 - "$output.provenance.json" <<'PY'
import json
import os
from pathlib import Path
import sys

document = {
    "schema_version": 1,
    "kernel_version": os.environ["KERNEL_VERSION_VALUE"],
    "source": {"url": os.environ["SOURCE_URL"], "sha256": os.environ["SOURCE_SHA"]},
    "firecracker": {
        "minimum_version": os.environ["FC_VERSION"],
        "config_commit": os.environ["FC_COMMIT"],
        "config_url": os.environ["CONFIG_URL"],
        "config_sha256": os.environ["CONFIG_SHA"],
    },
    "testbed_overlay": "scripts/vm/kernel/testbed-x86_64.fragment",
}
Path(sys.argv[1]).write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY

bash "$script_dir/audit-kernel-security.sh" \
  --kernel "$output" --config "$output.config" --output "$output.security-audit.json"

echo
echo "=== VM-testbed guest kernel build complete ==="
echo "Kernel: $output"
echo "Config: $output.config"
echo "Static audit: $output.security-audit.json"
echo "Provenance: $output.provenance.json"
sha256sum "$output" "$output.config" "$output.security-audit.json" "$output.provenance.json"
