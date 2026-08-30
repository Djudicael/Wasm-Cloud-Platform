#!/usr/bin/env bash
# Exercise sensitive headers through the recorded microVM topology and prove
# that the sentinel is absent from platform logs, audit exports, and traces.

set -euo pipefail

state_file=.vm-testbed-state.json
output_dir=

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --output-dir) output_dir=${2:?missing output directory}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-platform-redaction.sh --state-file PATH --output-dir PATH"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
for command in curl jq python3 sha256sum; do
  command -v "$command" >/dev/null || { echo "$command is required." >&2; exit 1; }
done

state_file=$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
services_file="${state_file}.services.json"
[[ -f "$state_file" && -f "$services_file" ]] || { echo "Missing topology/service state." >&2; exit 1; }
[[ -n "$output_dir" ]] || { echo "--output-dir is required." >&2; exit 2; }
output_dir=$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$output_dir")
evidence_root="$repo_root/INFRA_IMPL/process/prod_validation/evidence"
case "$output_dir/" in
  "$evidence_root"/*/) ;;
  *) echo "--output-dir must be below $evidence_root" >&2; exit 2 ;;
esac
mkdir -p "$output_dir"
chmod 700 "$output_dir"
# A repeated run replaces its generated trace selection. Preserve operator notes
# such as README.md, but do not mix traces from earlier sentinels into this run.
find "$output_dir" -maxdepth 1 -type f -name 'trace-*.json' -delete

mapfile -t lifecycle < <(python3 - "$state_file" "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
with open(sys.argv[2], encoding="utf-8") as stream:
    services = json.load(stream)
obs = services.get("observability") or {}
front = services.get("front_door") or {}
print(front.get("bind", ""))
print(front.get("log", ""))
print(obs.get("tempo", ""))
print(obs.get("operational_log", ""))
print(obs.get("audit_log", ""))
for node in state.get("nodes", []):
    print("NODE\t%s\t%s" % (node.get("id", ""), node.get("admin_addr", "")))
PY
)

front_bind=${lifecycle[0]:-}
front_log=${lifecycle[1]:-}
tempo_url=${lifecycle[2]:-}
operational_log=${lifecycle[3]:-}
audit_log=${lifecycle[4]:-}
[[ "$front_bind" == 127.0.0.1:* && "$tempo_url" == http://127.0.0.1:* ]] || {
  echo "Unexpected recorded front door or Tempo endpoint." >&2
  exit 1
}
for path in "$front_log" "$operational_log" "$audit_log"; do
  [[ "$path" == /* && -f "$path" ]] || { echo "Missing recorded artifact: $path" >&2; exit 1; }
done

sentinel="P10_05_$(openssl rand -hex 24 2>/dev/null || python3 -c 'import secrets; print(secrets.token_hex(24))')"
sentinel_sha256=$(printf '%s' "$sentinel" | sha256sum | cut -d' ' -f1)
backend_trace=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
frontend_trace=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
span_id=$(python3 -c 'import secrets; print(secrets.token_hex(8))')

request() {
  local name=$1 host=$2 path=$3 trace_id=$4
  curl -sS --max-time 20 -D "$output_dir/${name}.headers" -o "$output_dir/${name}.body" \
    -H "Host: $host" \
    -H "Authorization: Bearer $sentinel" \
    -H "Cookie: p10_session=$sentinel" \
    -H "X-App-Id: $sentinel" \
    -H "X-Trace-Id: $sentinel" \
    -H "traceparent: 00-${trace_id}-${span_id}-01" \
    "http://${front_bind}${path}"
}

request backend oidc-backend.internal /health/ready "$backend_trace"
request frontend oidc-frontend.internal / "$frontend_trace"

# A malformed trace header containing the sentinel must be ignored rather than
# copied into application correlation fields.
curl -sS --max-time 20 -D "$output_dir/malformed-trace.headers" \
  -o "$output_dir/malformed-trace.body" \
  -H 'Host: oidc-backend.internal' \
  -H "traceparent: $sentinel" \
  "http://${front_bind}/health/ready"

# OIDC responses legitimately use query parameters for authorization codes and
# state. Neither the platform front door nor application middleware may log
# their values.
curl -sS --max-time 20 -D "$output_dir/query-redaction.headers" \
  -o "$output_dir/query-redaction.body" \
  -H 'Host: oidc-backend.internal' \
  "http://${front_bind}/health/ready?code=${sentinel}&state=${sentinel}"

# This is an application-protocol assertion exercised through the platform:
# an unregistered redirect URI must never become the response Location.
curl -sS --max-time 20 -D "$output_dir/invalid-redirect.headers" \
  -o "$output_dir/invalid-redirect.body" \
  -H 'Host: oidc-backend.internal' \
  "http://${front_bind}/oidc/authorize?client_id=admin-ui&redirect_uri=https%3A%2F%2Fevil.invalid%2Fcallback&response_type=code&scope=openid&state=p10-invalid-redirect"

node_count=0
for entry in "${lifecycle[@]:5}"; do
  IFS=$'\t' read -r kind node_id admin_addr <<<"$entry"
  [[ "$kind" == NODE && -n "$node_id" && "$admin_addr" == *:* ]] || continue
  node_count=$((node_count + 1))
  curl -sS --max-time 10 -D "$output_dir/${node_id}-admin.headers" \
    -o "$output_dir/${node_id}-admin.body" \
    -H "Authorization: Bearer $sentinel" \
    "http://${admin_addr}/api/v1/apps" || true
  tail -c 8388608 "/tmp/vm-testbed-${node_id}/serial.log" \
    > "$output_dir/${node_id}-serial.log"
done
[[ $node_count -gt 0 ]] || { echo "No recorded platform nodes." >&2; exit 1; }

# Allow the Collector and Tempo batch exporters to flush, then preserve the
# exact traces created above.
for trace_id in "$backend_trace" "$frontend_trace"; do
  trace_file="$output_dir/trace-${trace_id}.json"
  for _ in {1..20}; do
    if curl -fsS --max-time 10 "$tempo_url/api/traces/$trace_id" -o "$trace_file" 2>/dev/null \
      && [[ -s "$trace_file" ]]; then
      break
    fi
    sleep 1
  done
  [[ -s "$trace_file" ]] || { echo "Tempo trace not found: $trace_id" >&2; exit 1; }
done

tail -c 4194304 "$front_log" > "$output_dir/haproxy.log"
tail -c 16777216 "$operational_log" > "$output_dir/operational.json"
tail -c 16777216 "$audit_log" > "$output_dir/audit.json"

mapfile -t scanned < <(find "$output_dir" -maxdepth 1 -type f ! -name RESULT_SUMMARY.json ! -name SHA256SUMS -print | sort)
WASM_SECRET_REDACTION_SENTINEL=$sentinel scripts/validate-secret-redaction.sh "${scanned[@]}"

python3 - "$output_dir" "$backend_trace" "$frontend_trace" "$sentinel_sha256" "$node_count" <<'PY'
import json, pathlib, sys
out = pathlib.Path(sys.argv[1])
backend_trace, frontend_trace, sentinel_hash, node_count = sys.argv[2:]
backend = (out / f"trace-{backend_trace}.json").read_text(encoding="utf-8")
frontend = (out / f"trace-{frontend_trace}.json").read_text(encoding="utf-8")
admin_headers = sorted(out.glob("*-admin.headers"))
if not admin_headers or any(" 401 " not in path.read_text(encoding="utf-8").splitlines()[0] for path in admin_headers):
    raise SystemExit("one or more node admin APIs accepted the invalid bearer credential")
if "oidc/openid-connect-wasi:" not in backend:
    raise SystemExit("backend trace lacks the backend deployment identity")
if "oidc/oidc-admin-wasi:" not in frontend:
    raise SystemExit("frontend trace lacks the frontend deployment identity")
if "oidc/oidc-admin-wasi:" in backend or "oidc/openid-connect-wasi:" in frontend:
    raise SystemExit("frontend/backend deployment attribution crossed traces")
redirect_headers = (out / "invalid-redirect.headers").read_text(encoding="utf-8").lower()
if "location: https://evil.invalid" in redirect_headers:
    raise SystemExit("OIDC application redirected to an unregistered redirect_uri")
if "location: /oidc/error" not in redirect_headers:
    raise SystemExit("OIDC invalid redirect_uri did not use the local error endpoint")
summary = {
    "status": "pass",
    "platform_nodes": int(node_count),
    "sentinel_sha256": sentinel_hash,
    "sentinel_value_preserved": False,
    "checks": {
        "authorization_cookie_and_identity_headers_absent": True,
        "malformed_trace_context_absent": True,
        "invalid_admin_bearer_rejected_on_every_node": True,
        "backend_trace_attributed_to_backend_deployment": True,
        "frontend_trace_attributed_to_frontend_deployment": True,
        "cross_application_trace_attribution_absent": True,
        "invalid_oidc_redirect_uri_uses_local_error": True,
    },
    "scope": "local Firecracker topology; repeat with production telemetry and retention systems",
}
(out / "RESULT_SUMMARY.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
PY

(
  cd "$output_dir"
  mapfile -t checksum_files < <(
    find . -maxdepth 1 -type f ! -name SHA256SUMS -printf '%P\n' | sort
  )
  sha256sum -- "${checksum_files[@]}" > SHA256SUMS
)
echo "Platform redaction and trace-attribution validation passed: $output_dir"
