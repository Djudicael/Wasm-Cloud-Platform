#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
rootfs=assets/vault-rootfs.ext4
bootstrap=${VAULT_BOOTSTRAP_FILE:-${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-build-$(id -u)/bootstrap.json}
ca_cert=assets/vault-test-ca.crt
ip=172.20.0.21
memory=512
vcpus=1

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --rootfs) rootfs=${2:?missing rootfs path}; shift 2 ;;
    --bootstrap) bootstrap=${2:?missing bootstrap path}; shift 2 ;;
    --ca-cert) ca_cert=${2:?missing CA path}; shift 2 ;;
    --ip) ip=${2:?missing IP address}; shift 2 ;;
    --memory) memory=${2:?missing memory}; shift 2 ;;
    --vcpus) vcpus=${2:?missing vCPU count}; shift 2 ;;
    -h|--help) echo "Usage: provision-vault-service.sh [--state-file PATH] [--ip IP]"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
for path in "$state_file" "$rootfs" "$bootstrap" "$ca_cert"; do
  [[ -f "$path" ]] || { echo "Missing required file: $path" >&2; exit 1; }
done
[[ $(stat -c '%a' "$bootstrap") =~ ^(400|600)$ ]] || { echo "$bootstrap must have mode 0600 or stricter." >&2; exit 1; }
command -v debugfs >/dev/null || { echo "debugfs is required." >&2; exit 1; }
[[ $(debugfs -R 'cat /etc/vault-image-schema-version' "$rootfs" 2>/dev/null) == 1 ]] || {
  echo "$rootfs is stale; rebuild with scripts/vm/build-vault-rootfs.sh" >&2
  exit 1
}

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  run_privileged() { wsl.exe -u root -- "$@"; }
else
  sudo -v
  run_privileged() { sudo -E "$@"; }
fi

vault_already_recorded=false
if jq -e '.services[]? | select(.id == "vault-secrets")' "$state_file" >/dev/null; then
  vault_already_recorded=true
else
  run_privileged "$cli" add-service --state-file "$state_file" --id vault-secrets \
    --kind vault --ip "$ip" --port 8200 --rootfs "$rootfs" --memory "$memory" --vcpus "$vcpus"
fi

state_absolute=$(realpath "$state_file")
state_key=$(printf '%s' "$state_absolute" | sha256sum | cut -d' ' -f1)
runtime_root="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-$(id -u)"
runtime_dir="$runtime_root/$state_key"
install -d -m 0700 "$runtime_dir"
if ! $vault_already_recorded || [[ ! -f "$runtime_dir/bootstrap.json" ]]; then
  install -m 0600 "$bootstrap" "$runtime_dir/bootstrap.json"
fi
if ! $vault_already_recorded || [[ ! -f "$runtime_dir/ca.crt" ]]; then
  install -m 0644 "$ca_cert" "$runtime_dir/ca.crt"
fi
vault_url="https://${ip}:8200"
root_token=$(jq -er '.root_token' "$runtime_dir/bootstrap.json")
unseal_key=$(jq -er '.unseal_key' "$runtime_dir/bootstrap.json")

curl -fsS --cacert "$runtime_dir/ca.crt" \
  -H 'Content-Type: application/json' --data-binary @- "$vault_url/v1/sys/unseal" \
  <<<"$(jq -cn --arg key "$unseal_key" '{key:$key}')" >/dev/null

root_curl="$runtime_dir/root.curl"
printf 'header = "X-Vault-Token: %s"\n' "$root_token" > "$root_curl"
chmod 0600 "$root_curl"

issue_token() {
  local role=$1 role_id=$2 output=$3 wrap_json wrap_token unwrap_curl secret_json secret_id login_json
  wrap_json=$(curl -fsS --config "$root_curl" --cacert "$runtime_dir/ca.crt" \
    -H 'X-Vault-Wrap-TTL: 60s' -X POST "$vault_url/v1/auth/approle/role/$role/secret-id")
  wrap_token=$(jq -er '.wrap_info.token' <<<"$wrap_json")
  unwrap_curl="$runtime_dir/unwrap-$role.curl"
  printf 'header = "X-Vault-Token: %s"\n' "$wrap_token" > "$unwrap_curl"
  chmod 0600 "$unwrap_curl"
  secret_json=$(curl -fsS --config "$unwrap_curl" --cacert "$runtime_dir/ca.crt" \
    -X POST "$vault_url/v1/sys/wrapping/unwrap")
  rm -f -- "$unwrap_curl"
  secret_id=$(jq -er '.data.secret_id' <<<"$secret_json")
  login_json=$(curl -fsS --cacert "$runtime_dir/ca.crt" -H 'Content-Type: application/json' \
    --data-binary @- "$vault_url/v1/auth/approle/login" \
    <<<"$(jq -cn --arg role_id "$role_id" --arg secret_id "$secret_id" '{role_id:$role_id,secret_id:$secret_id}')")
  jq -er '.auth.client_token' <<<"$login_json" > "$output"
  chmod 0600 "$output"
}

issue_token wasm-node-seal "$(jq -er '.node_role_id' "$runtime_dir/bootstrap.json")" "$runtime_dir/node-token"
issue_token wasm-seal-operator "$(jq -er '.operator_role_id' "$runtime_dir/bootstrap.json")" "$runtime_dir/operator-token"

services_file="${state_file}.services.json"
python3 - "$services_file" "$state_key" "$runtime_dir" "$vault_url" <<'PY'
import json, os, sys, tempfile
path, state_key, runtime_dir, url = sys.argv[1:]
state = {}
if os.path.exists(path):
    with open(path, encoding="utf-8") as stream:
        state = json.load(stream)
state["vault"] = {
    "type": "firecracker-local",
    "state_key": state_key,
    "runtime_dir": runtime_dir,
    "url": url,
    "ca_cert": os.path.join(runtime_dir, "ca.crt"),
    "credentials": {
        "node_token_file": os.path.join(runtime_dir, "node-token"),
        "operator_token_file": os.path.join(runtime_dir, "operator-token"),
    },
}
directory = os.path.dirname(os.path.abspath(path))
fd, temporary = tempfile.mkstemp(prefix=".services-", dir=directory, text=True)
with os.fdopen(fd, "w", encoding="utf-8") as stream:
    json.dump(state, stream, indent=2)
    stream.write("\n")
os.replace(temporary, path)
PY

echo "Vault service ready and unsealed at $vault_url"
echo "AppRole-derived credentials are protected under $runtime_dir"
echo "No token or unseal material was printed."
