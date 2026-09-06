#!/usr/bin/env bash
# Reconfigure the recorded local HAProxy front door for the two-WASI OIDC app.

set -euo pipefail
state_file=.vm-testbed-state.json
requested_bind=127.0.0.1:8088
platform_auth_host=gateway-auth.internal

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --bind) requested_bind=${2:?missing bind address}; shift 2 ;;
    --platform-auth-host) platform_auth_host=${2:?missing platform auth host}; shift 2 ;;
    -h|--help) echo "Usage: configure-oidc-test-gateway.sh [--state-file PATH]"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
haproxy_bin=$(command -v haproxy || true)
if [[ -z "$haproxy_bin" && -x /usr/sbin/haproxy ]]; then
  haproxy_bin=/usr/sbin/haproxy
fi
[[ -n "$haproxy_bin" ]] || { echo "HAProxy is required for the OIDC test gateway." >&2; exit 1; }
services_file="${state_file}.services.json"
[[ -f "$state_file" ]] || { echo "The topology state must exist first." >&2; exit 1; }
if [[ ! -f "$services_file" ]]; then
  gateway_config=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "${state_file}.haproxy.cfg")
  gateway_log=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "${state_file}.haproxy.log")
  python3 - "$services_file" "$requested_bind" "$gateway_config" "$gateway_log" <<'PY'
import json, os, sys
path, bind, config, log = sys.argv[1:]
with open(path, "w", encoding="utf-8") as stream:
    json.dump({
        "schema_version": 1,
        "front_door": {
            "type": "haproxy", "bind": bind, "pid": 0,
            "config": config, "log": log,
        },
    }, stream, indent=2)
    stream.write("\n")
PY
fi

mapfile -t gateway < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    front = (json.load(stream).get("front_door") or {})
for key in ("type", "bind", "pid", "config", "log"):
    print(front.get(key, ""))
PY
)
gateway_type=${gateway[0]:-}
gateway_bind=${gateway[1]:-}
old_pid=${gateway[2]:-}
gateway_config=${gateway[3]:-}
gateway_log=${gateway[4]:-}
[[ "$gateway_type" == haproxy && "$old_pid" =~ ^[0-9]+$ ]] || {
  echo "Invalid HAProxy lifecycle state in $services_file" >&2
  exit 1
}
expected_config=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "${state_file}.haproxy.cfg")
[[ "$gateway_config" == "$expected_config" ]] || {
  echo "Recorded HAProxy config does not belong to $state_file; refusing replacement." >&2
  exit 1
}
if ((old_pid > 0)) && kill -0 "$old_pid" 2>/dev/null; then
  old_args=$(tr '\0' ' ' < "/proc/$old_pid/cmdline")
  [[ "$old_args" == *haproxy* && "$old_args" == *"$gateway_config"* ]] || {
    echo "PID $old_pid is not the recorded HAProxy process; refusing replacement." >&2
    exit 1
  }
fi

mapfile -t proxy_addrs < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
for node in state.get("nodes", []):
    print(node["proxy_addr"])
PY
)
((${#proxy_addrs[@]} >= 3)) || { echo "OIDC rehearsal requires at least three platform nodes." >&2; exit 1; }
bridge_gateway=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["gateway"])' "$state_file")
gateway_port=${gateway_bind##*:}
private_bind="$bridge_gateway:$gateway_port"
private_bind_directive=
[[ "$private_bind" == "$gateway_bind" ]] || private_bind_directive="    bind $private_bind"

temporary_config="${gateway_config}.new"
{
  cat <<EOF
global
    log stdout format raw local0 info
    maxconn 4096

defaults
    log global
    mode http
    option httplog
    option dontlognull
    timeout connect 5s
    timeout client 60s
    timeout server 60s

frontend oidc_front_door
    bind $gateway_bind
$private_bind_directive
    option forwardfor
    # OAuth/OIDC query parameters can contain short-lived credentials and
    # correlation secrets. Keep the operational fields but log only the path.
    log-format "%ci:%cp [%tr] %ft %b/%s %TR/%Tw/%Tc/%Tr/%Ta %ST %B %tsc %ac/%fc/%bc/%sc/%rc %sq/%bq \"%HM %[var(txn.redacted_path)] %HV\""
    http-request set-var(txn.redacted_path) path
    http-request del-header X-Forwarded-For
    http-request del-header X-Forwarded-Host
    http-request del-header X-Forwarded-Proto
    http-request set-header X-Forwarded-Host %[req.hdr(host)]
    http-request set-header X-Forwarded-Proto http
    stick-table type ip size 100k expire 10m store http_req_rate(10s)
    http-request track-sc0 src
    http-request deny deny_status 429 if { sc_http_req_rate(0) gt 100 }
    acl oidc_backend path_beg /api/ /oidc/ /.well-known/ /health
    acl platform_auth_host hdr(host) -i $platform_auth_host
    acl realm_login path_reg ^/realms/[^/]+/login$
    acl realm_protocol path_reg ^/realms/[^/]+/protocol/.*
    acl realm_well_known path_reg ^/realms/[^/]+/\.well-known/.*
    http-request set-header Host oidc-backend.internal if oidc_backend !platform_auth_host
    http-request set-header Host oidc-backend.internal if realm_login !platform_auth_host
    http-request set-header Host oidc-backend.internal if realm_protocol !platform_auth_host
    http-request set-header Host oidc-backend.internal if realm_well_known !platform_auth_host
    http-request set-header Host oidc-frontend.internal if !platform_auth_host !oidc_backend !realm_login !realm_protocol !realm_well_known
    http-response set-header X-Content-Type-Options nosniff
    http-response set-header Referrer-Policy strict-origin-when-cross-origin
    use_backend platform_auth_nodes if platform_auth_host
    use_backend oidc_backend_nodes if { hdr(host) -i oidc-backend.internal }
    default_backend oidc_frontend_nodes

backend oidc_frontend_nodes
    balance roundrobin
    option httpchk
    http-check send meth GET uri / ver HTTP/1.1 hdr Host oidc-frontend.internal
    http-check expect status 200
EOF
  index=0
  for address in "${proxy_addrs[@]}"; do
    printf '    server node%s %s check inter 2s fall 3 rise 2\n' "$index" "$address"
    index=$((index + 1))
  done
  cat <<'EOF'

backend oidc_backend_nodes
    balance roundrobin
    option httpchk
    http-check send meth GET uri /health/ready ver HTTP/1.1 hdr Host oidc-backend.internal
    http-check expect status 200
EOF
  index=0
  for address in "${proxy_addrs[@]}"; do
    printf '    server node%s %s check inter 2s fall 3 rise 2\n' "$index" "$address"
    index=$((index + 1))
  done
  cat <<EOF

backend platform_auth_nodes
    balance roundrobin
    option httpchk
    http-check send meth GET uri /health ver HTTP/1.1 hdr Host $platform_auth_host
    http-check expect status 200
EOF
  index=0
  for address in "${proxy_addrs[@]}"; do
    printf '    server node%s %s check inter 2s fall 3 rise 2\n' "$index" "$address"
    index=$((index + 1))
  done
  cat <<'EOF'

listen local_stats
    bind 127.0.0.1:8404
    stats enable
    stats uri /stats

listen local_prometheus
    bind 127.0.0.1:8405
    mode http
    http-request use-service prometheus-exporter if { path /metrics }
EOF
} > "$temporary_config"

"$haproxy_bin" -c -f "$temporary_config"
if ((old_pid > 0)) && kill -0 "$old_pid" 2>/dev/null; then
  kill "$old_pid"
  for _ in {1..50}; do
    kill -0 "$old_pid" 2>/dev/null || break
    sleep 0.1
  done
  kill -0 "$old_pid" 2>/dev/null && { echo "Old HAProxy did not stop." >&2; exit 1; }
fi
mv -- "$temporary_config" "$gateway_config"
setsid "$haproxy_bin" -db -f "$gateway_config" </dev/null > "$gateway_log" 2>&1 &
new_pid=$!
sleep 0.5
kill -0 "$new_pid" 2>/dev/null || { echo "OIDC HAProxy failed to start; inspect $gateway_log" >&2; exit 1; }

python3 - "$services_file" "$new_pid" <<'PY'
import json, os, sys, tempfile
path, pid = sys.argv[1], int(sys.argv[2])
with open(path, encoding="utf-8") as stream:
    state = json.load(stream)
state["front_door"]["pid"] = pid
state["front_door"]["mode"] = "oidc-two-wasi"
state["front_door"]["metrics"] = "http://127.0.0.1:8405/metrics"
directory = os.path.dirname(os.path.abspath(path))
fd, temporary = tempfile.mkstemp(prefix=".services-", dir=directory, text=True)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        json.dump(state, stream, indent=2)
        stream.write("\n")
    os.replace(temporary, path)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY

echo "OIDC application gateway ready at http://$gateway_bind"
echo "Private JWKS path available to nodes at http://$private_bind/oidc/jwks"
echo "Platform authenticated-app host: $platform_auth_host"
