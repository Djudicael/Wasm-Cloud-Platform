#!/usr/bin/env bash
# Real-Vault acceptance drill for the node's external seal root and rotation path.

set -euo pipefail

state_file=.prod-validation-single-host-state.json
evidence_dir=INFRA_IMPL/process/prod_validation/evidence/2026-08-30-single-host/P10-02-vault-microvm
while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --evidence-dir) evidence_dir=${2:?missing evidence directory}; shift 2 ;;
    -h|--help) echo "Usage: validate-vault-transit-microvm.sh [--state-file PATH] [--evidence-dir PATH]"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
services_file="${state_file}.services.json"
[[ -f "$services_file" ]] || { echo "Missing companion service state: $services_file" >&2; exit 1; }

# Refresh short-lived AppRole credentials without replacing the recorded VM.
bash scripts/vm/provision-vault-service.sh --state-file "$state_file"
runtime_dir=$(jq -er '.vault.runtime_dir' "$services_file")
vault_url=$(jq -er '.vault.url' "$services_file")
ca_cert=$(jq -er '.vault.ca_cert' "$services_file")
node_token_file=$(jq -er '.vault.credentials.node_token_file' "$services_file")
operator_token_file=$(jq -er '.vault.credentials.operator_token_file' "$services_file")
for path in "$ca_cert" "$node_token_file" "$operator_token_file" "$runtime_dir/bootstrap.json"; do
  [[ -f "$path" ]] || { echo "Missing Vault runtime material: $path" >&2; exit 1; }
done

mkdir -p "$evidence_dir"
chmod 0700 "$evidence_dir"
work_dir=$(mktemp -d)
node_pid=
audit_pid=
audit_enabled=false
root_curl="$runtime_dir/root.curl"
node_curl="$work_dir/node.curl"
operator_curl="$work_dir/operator.curl"
printf 'header = "X-Vault-Token: %s"\n' "$(<"$node_token_file")" > "$node_curl"
printf 'header = "X-Vault-Token: %s"\n' "$(<"$operator_token_file")" > "$operator_curl"
chmod 0600 "$node_curl" "$operator_curl"

root_api() {
  curl -fsS --config "$root_curl" --cacert "$ca_cert" "$@"
}
cleanup() {
  if [[ -n "$node_pid" ]] && kill -0 "$node_pid" 2>/dev/null; then
    kill -TERM "$node_pid" 2>/dev/null || true
    wait "$node_pid" 2>/dev/null || true
  fi
  if $audit_enabled; then
    root_api -X DELETE "$vault_url/v1/sys/audit/vault-drill" >/dev/null 2>&1 || true
  fi
  if [[ -n "$audit_pid" ]] && kill -0 "$audit_pid" 2>/dev/null; then
    kill "$audit_pid" 2>/dev/null || true
    wait "$audit_pid" 2>/dev/null || true
  fi
  rm -rf -- "$work_dir"
}
trap cleanup EXIT

audit_port=$(python3 - <<'PY'
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind(("0.0.0.0", 0))
print(sock.getsockname()[1])
sock.close()
PY
)
audit_log="$evidence_dir/vault-audit.jsonl"
python3 -c 'import socket,sys; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind(("0.0.0.0",int(sys.argv[1]))); f=open(sys.argv[2],"ab",buffering=0); [(f.write(s.recv(1048576)+b"\n")) for _ in iter(int,1)]' "$audit_port" "$audit_log" &
audit_pid=$!
root_api -H 'Content-Type: application/json' -X PUT --data-binary @- \
  "$vault_url/v1/sys/audit/vault-drill" \
  <<<"$(jq -cn --arg address "172.20.0.1:$audit_port" '{type:"socket",options:{address:$address,socket_type:"udp"}}')" >/dev/null
audit_enabled=true

transit_key=$(jq -er '.transit_key' "$runtime_dir/bootstrap.json")
sentinel="vault-drill-$(openssl rand -hex 16)"
encoded_input=$(printf '%s' "$sentinel" | base64 -w0)

unauthorized_status=$(curl -sS -o "$work_dir/unauthorized.json" -w '%{http_code}' --cacert "$ca_cert" \
  -H 'Content-Type: application/json' --data-binary @- \
  "$vault_url/v1/transit/hmac/$transit_key/sha2-256" <<<"$(jq -cn --arg input "$encoded_input" '{input:$input}')")
[[ "$unauthorized_status" == 403 ]] || { echo "Unauthenticated HMAC returned $unauthorized_status, expected 403." >&2; exit 1; }

key_metadata=$(curl -fsS --config "$operator_curl" --cacert "$ca_cert" \
  "$vault_url/v1/transit/keys/$transit_key")
previous_version=$(jq -er '.data.latest_version' <<<"$key_metadata")
previous_hmac=$(curl -fsS --config "$node_curl" --cacert "$ca_cert" \
  -H 'Content-Type: application/json' --data-binary @- \
  "$vault_url/v1/transit/hmac/$transit_key/sha2-256" \
  <<<"$(jq -cn --arg input "$encoded_input" --argjson version "$previous_version" '{input:$input,key_version:$version}')")
previous_request_id=$(jq -er '.request_id' <<<"$previous_hmac")

denied_rotate_status=$(curl -sS -o "$work_dir/denied-rotate.json" -w '%{http_code}' \
  --config "$node_curl" --cacert "$ca_cert" -X POST \
  "$vault_url/v1/transit/keys/$transit_key/rotate")
[[ "$denied_rotate_status" == 403 ]] || { echo "Node role rotated the key ($denied_rotate_status); least privilege failed." >&2; exit 1; }

rotate_json=$(curl -fsS --config "$operator_curl" --cacert "$ca_cert" -X POST \
  "$vault_url/v1/transit/keys/$transit_key/rotate")
rotate_request_id=$(jq -er '.request_id' <<<"$rotate_json")
active_version=$(curl -fsS --config "$operator_curl" --cacert "$ca_cert" \
  "$vault_url/v1/transit/keys/$transit_key" | jq -er '.data.latest_version')
[[ "$active_version" -eq $((previous_version + 1)) ]] || { echo "Vault key version did not increment." >&2; exit 1; }
active_hmac=$(curl -fsS --config "$node_curl" --cacert "$ca_cert" \
  -H 'Content-Type: application/json' --data-binary @- \
  "$vault_url/v1/transit/hmac/$transit_key/sha2-256" \
  <<<"$(jq -cn --arg input "$encoded_input" --argjson version "$active_version" '{input:$input,key_version:$version}')")
[[ $(jq -er '.data.hmac' <<<"$previous_hmac") != $(jq -er '.data.hmac' <<<"$active_hmac") ]] || {
  echo "Pinned Vault versions unexpectedly returned the same HMAC." >&2
  exit 1
}
active_request_id=$(jq -er '.request_id' <<<"$active_hmac")

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
cargo build -p node --bin wasm-node
node_bin="$CARGO_TARGET_DIR/debug/wasm-node"
db_path="$work_dir/node-state.redb"
nats_url=$(jq -er '.nats_url' "$state_file")

reserve_port() {
  python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}
start_node() {
  local version=$1 previous=${2:-} log=$3
  local proxy_port admin_port artifact_port deploy_port
  proxy_port=$(reserve_port); admin_port=$(reserve_port); artifact_port=$(reserve_port); deploy_port=$(reserve_port)
  local -a env_args=(
    "WASM_VAULT_DRILL_TOKEN=$(<"$node_token_file")"
    "WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_KEY_VERSION=$version"
    "RUST_LOG=info"
  )
  if [[ -n "$previous" ]]; then
    env_args+=("WASM_NODE_RUNTIME_KEY_VAULT_TRANSIT_PREVIOUS_KEY_VERSION=$previous")
  fi
  env "${env_args[@]}" "$node_bin" \
    --node-id vault-drill-node --db-path "$db_path" --nats-url "$nats_url" \
    --proxy-port "$proxy_port" --admin-port "$admin_port" --artifact-port "$artifact_port" \
    --deploy-ingress-port "$deploy_port" --admin-bind-address 127.0.0.1 \
    --artifact-bind-address 127.0.0.1 --deploy-ingress-bind-address 127.0.0.1 \
    --key-source vault-transit --key-vault-url "$vault_url" \
    --key-vault-token-env WASM_VAULT_DRILL_TOKEN --key-vault-ca-cert "$ca_cert" \
    --key-vault-transit-mount transit --key-vault-transit-key "$transit_key" \
    --key-vault-transit-context "$sentinel" --auth-enabled false --auth-require-tls false \
    >"$log" 2>&1 &
  node_pid=$!
  for _ in $(seq 1 120); do
    if curl -fsS "http://127.0.0.1:$admin_port/healthz" >/dev/null 2>&1; then return 0; fi
    if [[ -f "$log" ]] && rg -q \
      'initialized sealed node secret transport key|rewrapped node secret transport key|loaded sealed node secret transport key' \
      "$log"; then
      kill -0 "$node_pid" 2>/dev/null && return 0
    fi
    kill -0 "$node_pid" 2>/dev/null || { wait "$node_pid" || true; return 1; }
    sleep 0.25
  done
  return 1
}
stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || true
  node_pid=
}

start_node "$previous_version" '' "$evidence_dir/node-initial.log" || { echo "Node failed its initial real-Vault start." >&2; exit 1; }
stop_node
start_node "$active_version" "$previous_version" "$evidence_dir/node-rewrap.log" || { echo "Node failed the controlled rewrap restart." >&2; exit 1; }
stop_node
rg -q 'rewrapped persisted KEK with the active external seal-key version' "$evidence_dir/node-rewrap.log"
rg -q 'rewrapped node secret transport key with the active external seal-key version' "$evidence_dir/node-rewrap.log"
start_node "$active_version" '' "$evidence_dir/node-current-only.log" || { echo "Node failed to restart without the previous Vault key version." >&2; exit 1; }
stop_node

root_api -X POST "$vault_url/v1/sys/seal" >/dev/null
if start_node "$active_version" '' "$evidence_dir/node-vault-sealed.log"; then
  echo "Node unexpectedly started while Vault was sealed." >&2
  exit 1
fi
node_pid=
unseal_key=$(jq -er '.unseal_key' "$runtime_dir/bootstrap.json")
curl -fsS --cacert "$ca_cert" -H 'Content-Type: application/json' --data-binary @- \
  "$vault_url/v1/sys/unseal" <<<"$(jq -cn --arg key "$unseal_key" '{key:$key}')" >/dev/null
start_node "$active_version" '' "$evidence_dir/node-after-recovery.log" || { echo "Node did not recover after Vault unseal." >&2; exit 1; }
stop_node

sleep 1
root_api -X DELETE "$vault_url/v1/sys/audit/vault-drill" >/dev/null
audit_enabled=false
kill "$audit_pid"
wait "$audit_pid" 2>/dev/null || true
audit_pid=
[[ -s "$audit_log" ]] || { echo "Vault audit receiver captured no events." >&2; exit 1; }

WASM_SECRET_REDACTION_SENTINEL="$sentinel" scripts/validate-secret-redaction.sh \
  "$evidence_dir"/*.log "$audit_log" > "$evidence_dir/redaction-scan.txt"
jq -n \
  --arg vault_url "$vault_url" --arg previous_version "$previous_version" --arg active_version "$active_version" \
  --arg previous_hmac_request_id "$previous_request_id" --arg rotation_request_id "$rotate_request_id" \
  --arg active_hmac_request_id "$active_request_id" \
  '{status:"pass",vault_url:$vault_url,tests:{tls_private_ca:true,approle_auth:true,unauthenticated_hmac_denied:true,node_role_rotation_denied:true,pinned_version_hmac:true,seal_root_rewrap:true,current_only_restart:true,sealed_outage_fail_closed:true,recovery:true,audit_capture:true,redaction_scan:true},versions:{previous:($previous_version|tonumber),active:($active_version|tonumber)},request_ids:{previous_hmac:$previous_hmac_request_id,rotation:$rotation_request_id,active_hmac:$active_hmac_request_id}}' \
  > "$evidence_dir/result.json"
find "$evidence_dir" -maxdepth 1 -type f ! -name SHA256SUMS -print0 \
  | sort -z \
  | xargs -0 sha256sum > "$evidence_dir/SHA256SUMS"

echo "Real Vault Transit microVM drill passed."
echo "Evidence: $evidence_dir"
echo "The platform testbed and Vault service remain running."
