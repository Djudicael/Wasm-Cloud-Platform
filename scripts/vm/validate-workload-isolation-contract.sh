#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
node_id=
restart_node=false
evidence_dir=
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}

usage() {
  cat <<'EOF'
Usage: validate-workload-isolation-contract.sh [OPTIONS]

Validate the current single-trust-domain production contract and the node-level
cgroup boundary used by the system-wide eBPF probes.

Options:
  --state-file PATH     Existing microVM testbed state
  --node-id ID          Node to inspect (defaults to the first recorded node)
  --restart-node        Restart that node from schema-15 rootfs with mandatory eBPF
  --evidence-dir PATH   Write machine-readable and captured evidence
  --auth-token TOKEN    Node admin bearer token
  -h, --help            Show this help
EOF
}

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --node-id) node_id=${2:?missing node ID}; shift 2 ;;
    --restart-node) restart_node=true; shift ;;
    --evidence-dir) evidence_dir=${2:?missing evidence directory}; shift 2 ;;
    --auth-token) auth_token=${2:?missing auth token}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
cd "$repo_root"

for command_name in cargo curl debugfs jq sha256sum; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
[[ -f assets/wasm-node-rootfs.ext4 ]] || { echo "Missing node rootfs." >&2; exit 1; }

schema=$(debugfs -R 'cat /etc/wasm-node/image-schema-version' assets/wasm-node-rootfs.ext4 2>/dev/null || true)
[[ "$schema" == 15 ]] || {
  echo "Node rootfs schema is $schema, expected 15; rebuild with scripts/vm/build-node-rootfs.sh." >&2
  exit 1
}

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
export CARGO_TARGET_DIR="$target_dir"

echo "[1/5] Validate the production admission contract"
cargo test -p config tests::production_requires_explicit_single_trust_domain_and_ebpf -- --exact
cargo test -p config tests::unsupported_hostile_multi_tenant_claim_is_rejected -- --exact

echo "[2/5] Validate cgroup-v2 identity resolution"
cargo test -p ebpf-monitor --features ebpf loader::tests::test_current_cgroup_id_is_nonzero -- --exact

echo "[3/5] Build every eBPF object from a clean BPF target"
bash scripts/ebpf/build-ebpf.sh

cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
if [[ -z "$node_id" ]]; then
  node_id=$(jq -er '.nodes[0].id' "$state_file")
fi
node_ip=$(jq -er --arg id "$node_id" '.nodes[] | select(.id == $id) | .ip' "$state_file")

if [[ "$restart_node" == true ]]; then
  echo "[4/5] Restart $node_id from the validated rootfs with mandatory eBPF"
  if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
    wsl.exe -u root -- "$cli" restart-node --state-file "$state_file" --id "$node_id" --ebpf-required
  else
    sudo -E "$cli" restart-node --state-file "$state_file" --id "$node_id" --ebpf-required
  fi
else
  echo "[4/5] Inspect the already-running $node_id (no restart requested)"
fi

echo "[5/5] Correlate guest cgroup identity with the configured BPF maps"
serial_log="/tmp/vm-testbed-${node_id}/serial.log"
[[ -r "$serial_log" ]] || { echo "Missing serial log: $serial_log" >&2; exit 1; }
guest_cgroup_id=$(tr -d '\r' < "$serial_log" | sed -n 's/^WCP_NODE_CGROUP_ID=\([0-9][0-9]*\)$/\1/p' | tail -n 1)
configured_cgroup_id=$(sed -n 's/.*"node_cgroup_id":\([0-9][0-9]*\).*/\1/p' "$serial_log" | tail -n 1)
[[ -n "$guest_cgroup_id" ]] || { echo "Guest did not report its dedicated node cgroup ID." >&2; exit 1; }
[[ "$configured_cgroup_id" == "$guest_cgroup_id" ]] || {
  echo "BPF CONFIG cgroup $configured_cgroup_id does not match guest cgroup $guest_cgroup_id." >&2
  exit 1
}

status=$(curl -fsS --max-time 5 \
  -H "Authorization: Bearer $auth_token" \
  "http://${node_ip}:9090/admin/ebpf/status")
jq -e '.ebpf_active == true and .monitoring_required == true and .monitoring_degraded == false and .attached_programs == 7' \
  <<<"$status" >/dev/null
curl -fsS --max-time 5 "http://${node_ip}:9090/readyz" | jq -e '.status == "healthy"' >/dev/null

while IFS= read -r ip; do
  curl -fsS --max-time 5 "http://${ip}:9090/readyz" | jq -e '.status == "healthy" or .status == "degraded"' >/dev/null
done < <(jq -r '.nodes[].ip' "$state_file")

if [[ -n "$evidence_dir" ]]; then
  mkdir -p "$evidence_dir"
  printf '%s\n' "$status" | jq . > "$evidence_dir/ebpf-status.json"
  grep -E 'WCP_NODE_CGROUP_ID=|node_cgroup_id|kernel monitoring active' "$serial_log" \
    > "$evidence_dir/cgroup-activation.log"
  jq -n \
    --arg node_id "$node_id" \
    --arg node_ip "$node_ip" \
    --argjson node_cgroup_id "$guest_cgroup_id" \
    --arg rootfs_sha256 "$(sha256sum assets/wasm-node-rootfs.ext4 | awk '{print $1}')" \
    --arg kernel_sha256 "$(sha256sum "$(jq -r '.kernel_path' "$state_file")" | awk '{print $1}')" \
    '{
      gate: "P10-10",
      result: "pass",
      production_isolation_mode: "single-trust-domain",
      hostile_multi_tenant_supported: false,
      node_id: $node_id,
      node_ip: $node_ip,
      node_cgroup_id: $node_cgroup_id,
      system_wide_probe_scope: "wasm-node cgroup",
      per_application_identity: "registered dedicated runtime TID",
      buffered_block_io_per_application_attribution: false,
      ebpf_mandatory: true,
      attached_programs: 7,
      node_rootfs_schema: 15,
      node_rootfs_sha256: $rootfs_sha256,
      kernel_sha256: $kernel_sha256
    }' > "$evidence_dir/RESULT_SUMMARY.json"
  (
    cd "$evidence_dir"
    sha256sum RESULT_SUMMARY.json ebpf-status.json cgroup-activation.log > SHA256SUMS
  )
fi

echo "P10-10 isolation contract PASS: node=$node_id cgroup=$guest_cgroup_id eBPF=mandatory/7-of-7"
echo "Scope: one mutually trusted application domain per node; hostile multi-tenancy is rejected."
