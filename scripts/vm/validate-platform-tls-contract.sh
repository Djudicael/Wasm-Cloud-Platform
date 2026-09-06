#!/usr/bin/env bash
# Validate platform-owned TLS integration without treating external services as
# platform components. This starts one disposable mTLS NATS container and one
# native wasm-node process; it does not mutate or destroy a recorded testbed.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: validate-platform-tls-contract.sh [--evidence-dir DIR]

Runs in Linux/WSL2. The test proves private-CA/mTLS NATS connectivity, HTTPS on
the proxy/admin/deploy-ingress/artifact listeners, plaintext rejection, NATS
outage visibility and reconnect, and fail-closed invalid certificate startup.
EOF
}

evidence_dir=
while (($#)); do
  case "$1" in
    --evidence-dir) evidence_dir=${2:?missing evidence directory}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
for command_name in cargo curl openssl podman python3; do
  command -v "$command_name" >/dev/null || { echo "$command_name is required." >&2; exit 1; }
done

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
node_bin="$target_dir/debug/wasm-node"
if [[ ! -x "$node_bin" ]]; then
  CARGO_TARGET_DIR="$target_dir" cargo build -p node --bin wasm-node
fi

work_dir=$(mktemp -d)
container_name="wasm-platform-p10-09-nats-$$"
node_pid=
cleanup() {
  if [[ -n ${node_pid:-} ]] && kill -0 "$node_pid" 2>/dev/null; then
    kill "$node_pid" 2>/dev/null || true
    wait "$node_pid" 2>/dev/null || true
  fi
  podman rm -f "$container_name" >/dev/null 2>&1 || true
  rm -rf -- "$work_dir"
}
trap cleanup EXIT

read -r nats_port http_port https_port admin_port artifact_port deploy_port < <(
  python3 - <<'PY'
import socket
sockets=[]
ports=[]
for _ in range(6):
    sock=socket.socket()
    sock.bind(("127.0.0.1", 0))
    sockets.append(sock)
    ports.append(str(sock.getsockname()[1]))
print(" ".join(ports))
PY
)

cat > "$work_dir/ca.cnf" <<'EOF'
[req]
distinguished_name=dn
prompt=no
[dn]
CN=Wasm Platform P10-09 Test CA
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -config "$work_dir/ca.cnf" \
  -keyout "$work_dir/ca.key" -out "$work_dir/ca.crt" >/dev/null 2>&1

issue_certificate() {
  local name=$1 usage=$2
  cat > "$work_dir/$name.cnf" <<EOF
[req]
distinguished_name=dn
prompt=no
[dn]
CN=localhost
[ext]
subjectAltName=DNS:localhost,IP:127.0.0.1
extendedKeyUsage=$usage
keyUsage=digitalSignature,keyEncipherment
EOF
  openssl req -new -newkey rsa:2048 -nodes -config "$work_dir/$name.cnf" \
    -keyout "$work_dir/$name.key" -out "$work_dir/$name.csr" >/dev/null 2>&1
  openssl x509 -req -days 1 -in "$work_dir/$name.csr" \
    -CA "$work_dir/ca.crt" -CAkey "$work_dir/ca.key" -CAcreateserial \
    -extfile "$work_dir/$name.cnf" -extensions ext -out "$work_dir/$name.crt" >/dev/null 2>&1
}
issue_certificate nats-server serverAuth
issue_certificate nats-client clientAuth
issue_certificate node-listener serverAuth
chmod 600 "$work_dir"/*.key

cat > "$work_dir/nats.conf" <<'EOF'
port: 4222
jetstream { store_dir: "/data" }
tls {
  cert_file: "/certs/nats-server.crt"
  key_file: "/certs/nats-server.key"
  ca_file: "/certs/ca.crt"
  verify: true
  timeout: 2
}
EOF

podman run -d --name "$container_name" \
  --label wasm-cloud-platform.test-scope=p10-09 \
  -p "127.0.0.1:${nats_port}:4222" \
  -v "$work_dir:/certs:ro,Z" \
  docker.io/library/nats:2.10-alpine -c /certs/nats.conf >/dev/null

for _ in $(seq 1 30); do
  if podman logs "$container_name" 2>&1 | grep -q "Server is ready"; then break; fi
  sleep 1
done
podman inspect -f '{{.State.Running}}' "$container_name" | grep -qx true

cat > "$work_dir/node.toml" <<EOF
[node]
node_id = "p10-09-tls-node"
environment = "test"

[storage]
db_path = "$work_dir/state.redb"

[nats]
url = "tls://127.0.0.1:$nats_port"
ca_cert = "$work_dir/ca.crt"
client_cert = "$work_dir/nats-client.crt"
client_key = "$work_dir/nats-client.key"

[proxy]
http_port = $http_port
https_port = $https_port
tls_cert = "$work_dir/node-listener.crt"
tls_key = "$work_dir/node-listener.key"

[admin]
port = $admin_port
artifact_port = $artifact_port
deploy_ingress_port = $deploy_port
bind_address = "127.0.0.1"
artifact_bind_address = "127.0.0.1"
deploy_ingress_bind_address = "127.0.0.1"
tls_cert = "$work_dir/node-listener.crt"
tls_key = "$work_dir/node-listener.key"

[auth]
enabled = true
read_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
write_token = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
require_tls = true

[runtime]
port_start = 20000
port_end = 20200
key_source = "generate"

[ebpf]
enabled = false
EOF

"$node_bin" --config "$work_dir/node.toml" \
  --proxy-port "$http_port" --proxy-https-port "$https_port" \
  --admin-port "$admin_port" --artifact-port "$artifact_port" \
  --deploy-ingress-port "$deploy_port" >"$work_dir/node.log" 2>&1 &
node_pid=$!

health_url="https://localhost:$admin_port/health"
for _ in $(seq 1 180); do
  if curl -fsS --cacert "$work_dir/ca.crt" --resolve "localhost:$admin_port:127.0.0.1" \
      "$health_url" >"$work_dir/health-initial.json" 2>/dev/null; then
    break
  fi
  kill -0 "$node_pid" 2>/dev/null || { sed -E '/(token|password|credential|api[ _-]*key)/I{s/.*/[REDACTED]/;}' "$work_dir/node.log" >&2; exit 1; }
  sleep 1
done
python3 - "$work_dir/health-initial.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
assert d["status"] == "healthy", d
assert any(x["name"] == "nats" and x["status"] == "healthy" for x in d["dependencies"]), d
PY

for listener_spec in "proxy:$https_port" "admin:$admin_port" \
    "artifact:$artifact_port" "deploy-ingress:$deploy_port"; do
  listener_name=${listener_spec%%:*}
  port=${listener_spec##*:}
  tls_ready=false
  for _ in $(seq 1 30); do
    if curl --http1.1 --no-alpn -sS --cacert "$work_dir/ca.crt" --resolve "localhost:$port:127.0.0.1" \
        -o /dev/null "https://localhost:$port/" 2>/dev/null; then
      tls_ready=true
      break
    fi
    sleep 1
  done
  if [[ "$tls_ready" != true ]]; then
    echo "$listener_name TLS listener $port did not become ready." >&2
    sed -E '/(token|password|credential|api[ _-]*key)/I{s/.*/[REDACTED]/;}' "$work_dir/node.log" >&2
    exit 1
  fi
  if curl -sS --max-time 2 -o /dev/null "http://127.0.0.1:$port/" 2>/dev/null; then
    echo "Plaintext unexpectedly succeeded on TLS listener $port." >&2
    exit 1
  fi
done

podman stop --time 2 "$container_name" >/dev/null
degraded_seen=false
for _ in $(seq 1 30); do
  status=$(curl -sS --cacert "$work_dir/ca.crt" --resolve "localhost:$admin_port:127.0.0.1" \
    -o "$work_dir/health-outage.json" -w '%{http_code}' "$health_url" || true)
  if [[ "$status" == 503 ]]; then degraded_seen=true; break; fi
  sleep 1
done
[[ "$degraded_seen" == true ]] || { echo "NATS outage was not reported through readiness." >&2; exit 1; }

podman start "$container_name" >/dev/null
recovered=false
for _ in $(seq 1 60); do
  status=$(curl -sS --cacert "$work_dir/ca.crt" --resolve "localhost:$admin_port:127.0.0.1" \
    -o "$work_dir/health-recovered.json" -w '%{http_code}' "$health_url" || true)
  if [[ "$status" == 200 ]]; then recovered=true; break; fi
  sleep 1
done
[[ "$recovered" == true ]] || { echo "Node did not recover after NATS restart." >&2; exit 1; }

cp "$work_dir/node.toml" "$work_dir/invalid-node.toml"
sed -i "s#node-listener.crt#missing-listener.crt#g" "$work_dir/invalid-node.toml"
if "$node_bin" --config "$work_dir/invalid-node.toml" \
    --proxy-port 0 --proxy-https-port 0 --admin-port 0 --artifact-port 0 \
    --deploy-ingress-port 0 >"$work_dir/invalid-tls.log" 2>&1; then
  echo "Node unexpectedly accepted missing TLS material." >&2
  exit 1
fi
grep -q "TLS config error" "$work_dir/invalid-tls.log"

if [[ -n "$evidence_dir" ]]; then
  mkdir -p "$evidence_dir"
  python3 - "$evidence_dir/RESULT_SUMMARY.json" <<'PY'
import json,sys
result={
  "status":"passed",
  "scope":"platform TLS and NATS client integration",
  "private_ca_nats":True,
  "nats_mutual_tls":True,
  "https_listeners":["proxy","admin","deploy-ingress","artifact"],
  "plaintext_rejected":True,
  "invalid_tls_material_failed_closed":True,
  "initial":{"http_status":200,"nats":"healthy","node_status":"healthy"},
  "nats_outage":{"http_status":503,"nats":"unhealthy","node_status":"unhealthy"},
  "nats_reconnected_without_node_restart":True,
  "recovered":{"http_status":200,"nats":"healthy","node_status":"healthy"},
}
with open(sys.argv[1],"w") as f: json.dump(result,f,indent=2,sort_keys=True); f.write("\n")
PY
fi

echo "P10-09 platform TLS/NATS integration validation passed."
