#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: provision-testbed.sh [options]
  --preset PRESET         smoke, multi-node, or production-like (default: smoke)
  --profile PROFILE       single-node, multi-node, or chaos-ready (default: single-node)
  --nodes COUNT           topology node count (default is profile-specific)
  --name NAME             topology name (default: local-test)
  --state-file PATH       persistent state path (default: .vm-testbed-state.json)
  --node-memory MIB       memory per platform node (default: 512)
  --node-vcpus COUNT      vCPUs per platform node (default: 2)
  --node-otlp-endpoint URL  OTLP gRPC endpoint configured in every node
  --node-oidc-issuer-url URL  Public issuer used for token validation
  --node-oidc-audience VALUE Expected token audience
  --node-oidc-jwks-url URL    Private JWKS endpoint reachable by every node
  --front-door TYPE       none or haproxy (production-like default: haproxy)
  --front-door-bind ADDR  HAProxy listen address (default: 127.0.0.1:8088)
  --prepare-assets        install/build missing Firecracker assets
EOF
}

preset=
profile=
nodes=
name=local-test
state_file=.vm-testbed-state.json
node_memory=
node_vcpus=
node_otlp_endpoint=
node_oidc_issuer_url=
node_oidc_audience=
node_oidc_jwks_url=
front_door=
front_door_bind=127.0.0.1:8088
prepare_assets=false

while (($#)); do
  case "$1" in
    --preset) preset=${2:?missing preset}; shift 2 ;;
    --profile) profile=${2:?missing profile}; shift 2 ;;
    --nodes) nodes=${2:?missing node count}; shift 2 ;;
    --name) name=${2:?missing name}; shift 2 ;;
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --node-memory) node_memory=${2:?missing memory}; shift 2 ;;
    --node-vcpus) node_vcpus=${2:?missing vCPU count}; shift 2 ;;
    --node-otlp-endpoint) node_otlp_endpoint=${2:?missing OTLP endpoint}; shift 2 ;;
    --node-oidc-issuer-url) node_oidc_issuer_url=${2:?missing OIDC issuer URL}; shift 2 ;;
    --node-oidc-audience) node_oidc_audience=${2:?missing OIDC audience}; shift 2 ;;
    --node-oidc-jwks-url) node_oidc_jwks_url=${2:?missing OIDC JWKS URL}; shift 2 ;;
    --front-door) front_door=${2:?missing front-door type}; shift 2 ;;
    --front-door-bind) front_door_bind=${2:?missing front-door bind address}; shift 2 ;;
    --prepare-assets) prepare_assets=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -n "$preset" && -n "$profile" ]]; then
  echo "Use either --preset or --profile, not both." >&2
  exit 2
fi
oidc_option_count=0
[[ -n "$node_oidc_issuer_url" ]] && ((oidc_option_count += 1))
[[ -n "$node_oidc_audience" ]] && ((oidc_option_count += 1))
[[ -n "$node_oidc_jwks_url" ]] && ((oidc_option_count += 1))
if ((oidc_option_count != 0 && oidc_option_count != 3)); then
  echo "OIDC issuer, audience, and JWKS URL must be supplied together." >&2
  exit 2
fi
if [[ -z "$preset" && -z "$profile" ]]; then
  preset=smoke
fi

case "$preset" in
  smoke)
    profile=${profile:-single-node}
    nodes=${nodes:-1}
    node_memory=${node_memory:-512}
    node_vcpus=${node_vcpus:-2}
    front_door=${front_door:-none}
    ;;
  multi-node)
    profile=${profile:-multi-node}
    nodes=${nodes:-3}
    node_memory=${node_memory:-512}
    node_vcpus=${node_vcpus:-2}
    front_door=${front_door:-none}
    ;;
  production-like)
    profile=${profile:-chaos-ready}
    [[ -n "$nodes" ]] || {
      echo "The production-like preset requires --nodes COUNT (minimum 3)." >&2
      exit 2
    }
    # Two non-trivial WASI services (for example an API and admin UI) can push a
    # 1 GiB guest below the node's default free-memory backpressure threshold.
    node_memory=${node_memory:-2048}
    node_vcpus=${node_vcpus:-2}
    front_door=${front_door:-haproxy}
    ;;
  "")
    node_memory=${node_memory:-512}
    node_vcpus=${node_vcpus:-2}
    front_door=${front_door:-none}
    ;;
  *) echo "Invalid preset: $preset" >&2; exit 2 ;;
esac

case "$profile" in
  single-node|multi-node|chaos-ready) ;;
  *) echo "Invalid profile: $profile" >&2; exit 2 ;;
esac

if [[ -n "$nodes" ]]; then
  [[ "$nodes" =~ ^[1-9][0-9]*$ ]] || { echo "Node count must be a positive integer: $nodes" >&2; exit 2; }
fi
if [[ "$profile" == single-node && -n "$nodes" && "$nodes" -ne 1 ]]; then
  echo "The single-node profile requires --nodes 1; use the multi-node preset for more nodes." >&2
  exit 2
fi
if [[ "$profile" != single-node && -n "$nodes" && "$nodes" -lt 2 ]]; then
  echo "The $profile profile requires at least 2 platform nodes." >&2
  exit 2
fi
if [[ "$preset" == production-like && "$nodes" -lt 3 ]]; then
  echo "The production-like preset requires at least 3 platform nodes." >&2
  exit 2
fi
case "$front_door" in
  none|haproxy) ;;
  *) echo "Invalid front door: $front_door" >&2; exit 2 ;;
esac
[[ "$front_door_bind" =~ ^[A-Za-z0-9._-]+:[0-9]+$ ]] || {
  echo "Invalid front-door bind address: $front_door_bind" >&2
  exit 2
}

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"

[[ -e /dev/kvm ]] || { echo "/dev/kvm is unavailable; enable WSL2 nested virtualization/KVM." >&2; exit 1; }
command -v sudo >/dev/null || { echo "sudo is required for TAP/bridge management." >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required to read and write testbed state." >&2; exit 1; }
if [[ "$front_door" == haproxy ]]; then
  haproxy_bin=$(command -v haproxy || true)
  if [[ -z "$haproxy_bin" && -x /usr/sbin/haproxy ]]; then
    haproxy_bin=/usr/sbin/haproxy
  fi
  [[ -n "$haproxy_bin" ]] || {
    echo "HAProxy is required for --front-door haproxy. Install it in WSL/Linux or use --front-door none." >&2
    exit 1
  }
fi
if [[ -e "$state_file" ]]; then
  echo "State file already exists: $state_file" >&2
  echo "Inspect or destroy the existing testbed before provisioning another one." >&2
  exit 1
fi
services_file="${state_file}.services.json"
haproxy_config="${state_file}.haproxy.cfg"
haproxy_log="${state_file}.haproxy.log"
if [[ -e "$services_file" || -e "$haproxy_config" || -e "$haproxy_log" ]]; then
  echo "Companion state already exists next to $state_file; destroy the old testbed before retrying." >&2
  exit 1
fi

if $prepare_assets; then
  scripts/vm/install-firecracker.sh
  scripts/vm/build-all-images.sh
fi

source scripts/vm/kernel-testbed.env
kernel_asset="assets/vmlinux-$KERNEL_SERIES"
for required in "$kernel_asset" assets/wasm-node-rootfs.ext4 assets/nats-rootfs.ext4; do
  [[ -f "$required" ]] || {
    echo "Missing $required. Re-run with --prepare-assets." >&2
    exit 1
  }
done
kernel_image_schema=
if [[ -f "$kernel_asset.schema" ]]; then
  kernel_image_schema=$(<"$kernel_asset.schema")
fi
if [[ "$kernel_image_schema" != "$KERNEL_SCHEMA" ]]; then
  echo "$kernel_asset is stale or incompatible (expected kernel schema $KERNEL_SCHEMA)." >&2
  echo "Rebuild it with: scripts/vm/build-kernel.sh" >&2
  exit 1
fi
bash scripts/vm/audit-kernel-security.sh --kernel "$kernel_asset" --config "$kernel_asset.config" >/dev/null
command -v debugfs >/dev/null || {
  echo "debugfs is required to validate VM images (Ubuntu/WSL: sudo apt-get install e2fsprogs)." >&2
  exit 1
}
nats_image_schema=$(debugfs -R 'cat /etc/nats/image-schema-version' assets/nats-rootfs.ext4 2>/dev/null || true)
if [[ "$nats_image_schema" != 2 ]]; then
  echo "assets/nats-rootfs.ext4 is stale or incompatible (expected image schema 2)." >&2
  echo "Rebuild it with: scripts/vm/build-nats-rootfs.sh" >&2
  exit 1
fi
node_image_schema=$(debugfs -R 'cat /etc/wasm-node/image-schema-version' assets/wasm-node-rootfs.ext4 2>/dev/null || true)
if [[ "$node_image_schema" != 14 ]]; then
  echo "assets/wasm-node-rootfs.ext4 is stale or incompatible (expected image schema 14)." >&2
  echo "Rebuild it with: scripts/vm/build-node-rootfs.sh" >&2
  exit 1
fi
command -v firecracker >/dev/null || { echo "firecracker is missing; re-run with --prepare-assets." >&2; exit 1; }

if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  run_privileged() { wsl.exe -u root -- "$@"; }
else
  sudo -v
  run_privileged() { sudo -E "$@"; }
fi

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"

args=(up --profile "$profile" --name "$name" --state-file "$state_file" --node-memory "$node_memory" --node-vcpus "$node_vcpus")
[[ -n "$nodes" ]] && args+=(--nodes "$nodes")
[[ -n "$node_otlp_endpoint" ]] && args+=(--node-otlp-endpoint "$node_otlp_endpoint")
if ((oidc_option_count == 3)); then
  args+=(--node-oidc-issuer-url "$node_oidc_issuer_url")
  args+=(--node-oidc-audience "$node_oidc_audience")
  args+=(--node-oidc-jwks-url "$node_oidc_jwks_url")
fi
run_privileged "$cli" "${args[@]}"
run_privileged "$cli" status --state-file "$state_file"

if [[ "$front_door" == haproxy ]]; then
  haproxy_config=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$haproxy_config")
  haproxy_log=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$haproxy_log")

  mapfile -t proxy_addrs < <(run_privileged python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
for node in state.get("nodes", []):
    print(node["proxy_addr"])
PY
  )
  ((${#proxy_addrs[@]} == nodes)) || {
    echo "Expected $nodes proxy endpoints in $state_file, found ${#proxy_addrs[@]}." >&2
    exit 1
  }

  {
    echo "global"
    echo "  log stdout format raw local0"
    echo "defaults"
    echo "  mode http"
    echo "  log global"
    echo "  timeout connect 5s"
    echo "  timeout client 30s"
    echo "  timeout server 30s"
    echo "frontend wasm_frontdoor"
    echo "  bind $front_door_bind"
    echo "  default_backend wasm_nodes"
    echo "backend wasm_nodes"
    echo "  balance roundrobin"
    for index in "${!proxy_addrs[@]}"; do
      echo "  server node$((index + 1)) ${proxy_addrs[$index]} check"
    done
    echo "listen local_prometheus"
    echo "  bind 127.0.0.1:8405"
    echo "  mode http"
    echo "  http-request use-service prometheus-exporter if { path /metrics }"
  } > "$haproxy_config"

  "$haproxy_bin" -c -f "$haproxy_config"
  setsid "$haproxy_bin" -db -f "$haproxy_config" </dev/null > "$haproxy_log" 2>&1 &
  haproxy_pid=$!
  cleanup_failed_front_door() {
    kill "$haproxy_pid" 2>/dev/null || true
  }
  trap cleanup_failed_front_door ERR
  sleep 1
  kill -0 "$haproxy_pid" 2>/dev/null || {
    echo "HAProxy failed to start; inspect $haproxy_log. The microVM state was preserved." >&2
    exit 1
  }

  python3 - "$services_file" "$front_door_bind" "$haproxy_pid" "$haproxy_config" "$haproxy_log" <<'PY'
import json, os, sys
path, bind, pid, config, log = sys.argv[1:]
payload = {
    "schema_version": 1,
    "front_door": {
        "type": "haproxy",
        "bind": bind,
        "pid": int(pid),
        "config": os.path.abspath(config),
        "log": os.path.abspath(log),
        "metrics": "http://127.0.0.1:8405/metrics",
    },
}
temporary = f"{path}.tmp"
with open(temporary, "w", encoding="utf-8") as stream:
    json.dump(payload, stream, indent=2)
    stream.write("\n")
os.replace(temporary, path)
PY
  trap - ERR
  echo "Production-like front door ready at http://$front_door_bind"
  echo "Front-door lifecycle state: $services_file"
fi

if [[ "$preset" == production-like ]]; then
  echo "Production-like rehearsal: $nodes platform nodes, per-node reverse proxies, one NATS VM, front door: $front_door."
  echo "This local topology does not provision production TLS, external secrets, observability, or a highly available NATS cluster."
fi
