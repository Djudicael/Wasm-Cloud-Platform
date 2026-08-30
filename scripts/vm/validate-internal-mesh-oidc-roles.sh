#!/usr/bin/env bash
# Validate east-west internal-gateway routing, workload attribution, namespace
# isolation, and OIDC realm/client role authorization in the local microVM testbed.

set -euo pipefail

state_file=.prod-validation-single-host-state.json
public_url=http://127.0.0.1:8088
evidence_dir=

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --public-url) public_url=${2:?missing public URL}; shift 2 ;;
    --evidence-dir) evidence_dir=${2:?missing evidence directory}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-internal-mesh-oidc-roles.sh [--state-file PATH] [--public-url URL] [--evidence-dir PATH]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
state_file=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
[[ -f "$state_file" ]] || { echo "Missing state file: $state_file" >&2; exit 1; }
for command_name in cargo curl openssl python3 sha256sum; do
  command -v "$command_name" >/dev/null || { echo "Missing command: $command_name" >&2; exit 1; }
done

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT
chmod 700 "$work_dir"
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
auth_token=local-test-write-token-change-me

mapfile -t topology < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
print(state["nats_url"])
for node in state.get("nodes", []):
    print(node["admin_addr"])
    print(node["artifact_addr"])
    print(node["proxy_addr"])
PY
)
[[ ${#topology[@]} -eq 10 ]] || { echo "Expected exactly three platform nodes." >&2; exit 1; }
nats_url=${topology[0]}
node_api="http://${topology[1]}"
artifact_api="http://${topology[2]}"
mapfile -t node_ids < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    for node in json.load(stream).get("nodes", []):
        print(node["id"])
PY
)
[[ ${#node_ids[@]} -eq 3 ]] || { echo "Expected exactly three node IDs." >&2; exit 1; }

for index in 1 4 7; do
  status=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $auth_token" "http://${topology[$index]}/healthz" || true)
  [[ "$status" == 200 ]] || { echo "Node health gate failed at ${topology[$index]}: HTTP $status" >&2; exit 1; }
done

deadline=$((SECONDS + 180))
while :; do
  oidc_status=$(curl -sS --max-time 10 -o /dev/null -w '%{http_code}' \
    -H 'Host: oidc-backend.internal' "$public_url/health/ready" || true)
  [[ "$oidc_status" == 200 ]] && break
  ((SECONDS < deadline)) || { echo "OIDC token issuer did not become ready: HTTP $oidc_status" >&2; exit 1; }
  sleep 2
done

echo "Building east-west WASI fixtures..."
CARGO_TARGET_DIR="$target_dir" cargo build -p hello-axum -p echo-service --target wasm32-wasip2 --release
caller_artifact="$target_dir/wasm32-wasip2/release/hello-axum.wasm"
target_artifact="$target_dir/wasm32-wasip2/release/echo-service.wasm"
[[ -s "$caller_artifact" && -s "$target_artifact" ]] || { echo "Missing WASI fixture artifact." >&2; exit 1; }
caller_sha=$(sha256sum "$caller_artifact" | cut -d' ' -f1)
target_sha=$(sha256sum "$target_artifact" | cut -d' ' -f1)
policy_sha=$(printf '%s' 'realm:mesh-admin;client:mesh-api:operator;namespace-deny' | sha256sum | cut -d' ' -f1)
target_name="mesh-role-target-${target_sha:0:8}-${policy_sha:0:4}"
caller_version="mesh-${caller_sha:0:8}-${policy_sha:0:4}"
target_app_id="validation/$target_name:v1"
caller_app_id="validation/mesh-role-caller:$caller_version"
cross_caller_app_id="mesh-attacker/mesh-role-caller:$caller_version"
target_internal_host="$target_name.validation.internal"
target_internal_url="http://$target_internal_host:9080"

target_manifest="$work_dir/target.toml"
cat > "$target_manifest" <<EOF
[app]
name = "$target_name"
version = "v1"
namespace = "validation"
wasm_artifact = "$target_artifact"

[fuel]
quota = 500000000
memory_pages = 2048
max_instances = 10
idle_timeout_secs = 300

[gateway.auth]
policy = "none"

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/echo"
methods = ["GET"]
auth = "roles"
allowed_roles = ["mesh-admin"]

[[gateway.endpoints]]
path = "/info"
methods = ["GET"]
auth = "roles"
allowed_roles = ["operator"]
client_id = "mesh-api"
EOF

write_caller_manifest() {
  local path=$1 namespace=$2 route_host=$3
  local placement_section=
  if [[ "$namespace" == validation ]]; then
    placement_section=$(printf '\n[placement]\npolicy = "every_node"\nlocal_dependencies = ["%s"]\n' "$target_app_id")
  fi
  cat > "$path" <<EOF
[app]
name = "mesh-role-caller"
version = "$caller_version"
namespace = "$namespace"
wasm_artifact = "$caller_artifact"

[fuel]
quota = 500000000
memory_pages = 2048
max_instances = 10
idle_timeout_secs = 300

[env]
ECHO_SERVICE_SERVICE_URL = "$target_internal_url"
ECHO_SERVICE_HOST = "$target_internal_host"
$placement_section

[gateway]
host = "$route_host"

[gateway.auth]
policy = "none"
EOF
}

caller_manifest="$work_dir/caller.toml"
cross_caller_manifest="$work_dir/cross-caller.toml"
caller_host=mesh-role-caller.internal
cross_caller_host=mesh-cross-caller.internal
write_caller_manifest "$caller_manifest" validation "$caller_host"
write_caller_manifest "$cross_caller_manifest" mesh-attacker "$cross_caller_host"

deploy_manifest() {
  WASM_CTL_NATS_URL="$nats_url" \
  WASM_CTL_NODE_API="$node_api" \
  WASM_CTL_AUTH_TOKEN="$auth_token" \
    "$target_dir/release/wasm-ctl" deploy --manifest "$1" --artifact-api "$artifact_api"
}

echo "Deploying role-protected target and same/cross-namespace callers..."
deploy_manifest "$target_manifest"
deploy_manifest "$caller_manifest"
deploy_manifest "$cross_caller_manifest"

# The runtime historically imposed a 30-second epoch lifetime on CLI-style
# servers. Keep the fixtures idle beyond that boundary before obtaining the
# short-lived OIDC token, then prove that they still serve traffic.
service_lifetime_gate_secs=${MESH_SERVICE_LIFETIME_GATE_SECS:-35}
echo "Waiting ${service_lifetime_gate_secs}s to cross the former WASI service lifetime boundary..."
sleep "$service_lifetime_gate_secs"

state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
secret_dir="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-oidc-secrets-$(id -u)/$state_key"
credentials="$secret_dir/credentials.txt"
private_key="$secret_dir/rsa.pem"
[[ -r "$credentials" && -r "$private_key" ]] || { echo "Missing local OIDC test credentials/key." >&2; exit 1; }
admin_email=$(sed -n 's/^Admin email: //p' "$credentials")
admin_password=$(sed -n 's/^Admin password: //p' "$credentials")
login_payload="$work_dir/login.json"
login_response="$work_dir/login-response.json"
python3 - "$admin_email" "$admin_password" > "$login_payload" <<'PY'
import json, sys
print(json.dumps({"email": sys.argv[1], "password": sys.argv[2], "client_id": "admin-ui"}))
PY
curl -fsS --max-time 30 -H 'Content-Type: application/json' \
  --data-binary "@$login_payload" "$public_url/oidc/login" > "$login_response"
base_token_file="$work_dir/base-token"
python3 - "$login_response" "$base_token_file" "$public_url" <<'PY'
import base64, json, os, sys
response_path, token_path, issuer = sys.argv[1:]
token = json.load(open(response_path, encoding="utf-8"))["access_token"]
parts = token.split(".")
claims = json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4)))
if claims.get("iss") != issuer:
    raise SystemExit("unexpected test-token issuer")
aud = claims.get("aud")
if "admin-ui" not in ([aud] if isinstance(aud, str) else aud or []):
    raise SystemExit("unexpected test-token audience")
open(token_path, "w", encoding="utf-8").write(token)
os.chmod(token_path, 0o600)
PY
base_token=$(<"$base_token_file")

make_role_token() {
  local mode=$1 output=$2 signing_input signature
  signing_input="$work_dir/signing-$mode"
  python3 - "$base_token_file" "$mode" > "$signing_input" <<'PY'
import base64, json, sys
token_path, mode = sys.argv[1:]
token = open(token_path, encoding="utf-8").read()
header_part, claims_part, _ = token.split(".")
decode = lambda value: json.loads(base64.urlsafe_b64decode(value + "=" * (-len(value) % 4)))
encode = lambda value: base64.urlsafe_b64encode(json.dumps(value, separators=(",", ":")).encode()).rstrip(b"=").decode()
header, claims = decode(header_part), decode(claims_part)
claims.pop("realm_access", None)
claims.pop("resource_access", None)
if mode == "realm-allowed":
    claims["realm_access"] = {"roles": ["mesh-admin"]}
elif mode == "realm-denied":
    claims["realm_access"] = {"roles": ["viewer"]}
elif mode == "client-allowed":
    claims["resource_access"] = {"mesh-api": {"roles": ["operator"]}}
elif mode == "client-denied":
    claims["resource_access"] = {"mesh-api": {"roles": ["viewer"]}}
else:
    raise SystemExit("unsupported role-token mode")
print(f"{encode(header)}.{encode(claims)}", end="")
PY
  signature=$(openssl dgst -sha256 -sign "$private_key" -binary "$signing_input" | openssl base64 -A | tr '+/' '-_' | tr -d '=')
  printf '%s.%s' "$(<"$signing_input")" "$signature" > "$output"
  chmod 600 "$output"
}
make_role_token realm-allowed "$work_dir/realm-allowed"
make_role_token realm-denied "$work_dir/realm-denied"
make_role_token client-allowed "$work_dir/client-allowed"
make_role_token client-denied "$work_dir/client-denied"
realm_allowed=$(<"$work_dir/realm-allowed")
realm_denied=$(<"$work_dir/realm-denied")
client_allowed=$(<"$work_dir/client-allowed")
client_denied=$(<"$work_dir/client-denied")

result_tsv="$work_dir/results.tsv"
: > "$result_tsv"
request() {
  local name=$1 base=$2 host=$3 path=$4 expected=$5 token=${6:-} forged_role=${7:-} body status
  body="$work_dir/body-$name"
  local -a args=(-sS --max-time 20 -o "$body" -w '%{http_code}' -H "Host: $host")
  [[ -n "$token" ]] && args+=(-H "Authorization: Bearer $token")
  [[ -n "$forged_role" ]] && args+=(-H "X-User-Roles: $forged_role")
  status=$(curl "${args[@]}" "$base$path" || true)
  printf '%s\t%s\t%s\n' "$name" "$expected" "$status" >> "$result_tsv"
  [[ "$status" == "$expected" ]] || {
    echo "Validation $name failed: expected HTTP $expected, got $status" >&2
    [[ -s "$body" ]] && sed -n '1,5p' "$body" >&2
    return 1
  }
}

deadline=$((SECONDS + 180))
while :; do
  ready=true
  for index in 3 6 9; do
    status=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' \
      -H "Host: $caller_host" "http://${topology[$index]}/health" || true)
    [[ "$status" == 200 ]] || ready=false
  done
  $ready && break
  ((SECONDS < deadline)) || { echo "Mesh caller did not become ready on all nodes." >&2; exit 1; }
  sleep 2
done

declare -a serial_logs serial_offsets
for node_id in "${node_ids[@]}"; do
  serial_log="/tmp/vm-testbed-$node_id/serial.log"
  [[ -r "$serial_log" ]] || { echo "Missing readable node serial log: $serial_log" >&2; exit 1; }
  serial_logs+=("$serial_log")
  serial_offsets+=("$(wc -c < "$serial_log")")
done

for index in 3 6 9; do
  base="http://${topology[$index]}"
  suffix="node$(((index - 3) / 3))"
  request "$suffix-realm-missing-token" "$base" "$caller_host" /call-echo 401
  request "$suffix-realm-forged-header" "$base" "$caller_host" /call-echo 401 "" mesh-admin
  request "$suffix-realm-no-role" "$base" "$caller_host" /call-echo 403 "$base_token"
  request "$suffix-realm-wrong-role" "$base" "$caller_host" /call-echo 403 "$realm_denied"
  request "$suffix-realm-allowed" "$base" "$caller_host" /call-echo 200 "$realm_allowed"
  grep -Fq 'Echo from echo-service' "$work_dir/body-$suffix-realm-allowed" || {
    echo "$suffix realm-role response did not prove target identity" >&2
    exit 1
  }
  request "$suffix-client-wrong-role" "$base" "$caller_host" /call-echo-info 403 "$client_denied"
  request "$suffix-client-allowed" "$base" "$caller_host" /call-echo-info 200 "$client_allowed"
  grep -Fq '"port"' "$work_dir/body-$suffix-client-allowed" || {
    echo "$suffix client-role response did not prove target identity" >&2
    exit 1
  }
  request "$suffix-cross-namespace-denied" "$base" "$cross_caller_host" /call-echo 403 "$realm_allowed"
done

# Sustain concurrent WASI HTTP calls on every node. Every successful response
# proves both standard `.internal` resolution and fail-closed eBPF caller
# attribution under shared Tokio worker concurrency.
# Let the preceding authorization matrix leave the configured per-IP limiter's
# one-second window; rate limiting is validated separately from this workload.
sleep 2
concurrency_tsv="$work_dir/concurrency-results.tsv"
: > "$concurrency_tsv"
concurrency_parallelism=8
concurrency_rounds=12
concurrency_requests=$((concurrency_parallelism * concurrency_rounds))
run_node_concurrency() {
  local index="$1"
  local base suffix statuses completed succeeded
  base="http://${topology[$index]}"
  suffix="node$(((index - 3) / 3))"
  statuses="$work_dir/concurrency-$suffix.statuses"
  seq "$concurrency_parallelism" | xargs -P "$concurrency_parallelism" -I '{}' \
    bash -c 'for _round in $(seq 1 "$1"); do
      curl -sS --max-time 20 -o /dev/null -w "%{http_code}\\n" \
        -H "Host: $2" -H "Authorization: Bearer $3" "$4/call-echo"
      sleep 1
    done' _ "$concurrency_rounds" "$caller_host" "$realm_allowed" "$base" > "$statuses"
  completed=$(wc -l < "$statuses")
  succeeded=$(grep -c '^200$' "$statuses" || true)
  [[ "$completed" -eq "$concurrency_requests" && "$succeeded" -eq "$concurrency_requests" ]] || {
    echo "$suffix concurrency validation failed: $succeeded/$completed successful requests" >&2
    exit 1
  }
  printf '%s\t%s\t%s\t%s\n' "$suffix" "$concurrency_requests" "$concurrency_parallelism" "$succeeded" >> "$concurrency_tsv"
}

# Run every node at the same time so the same short-lived OIDC token remains
# valid for the whole sustained-concurrency window.
concurrency_pids=()
for index in 3 6 9; do
  run_node_concurrency "$index" &
  concurrency_pids+=("$!")
done
concurrency_failed=0
for concurrency_pid in "${concurrency_pids[@]}"; do
  wait "$concurrency_pid" || concurrency_failed=1
done
[[ "$concurrency_failed" -eq 0 ]] || exit 1

identity_tsv="$work_dir/identity-results.tsv"
: > "$identity_tsv"
for index in 0 1 2; do
  new_log="$work_dir/new-serial-$index.log"
  tail -c "+$((serial_offsets[$index] + 1))" "${serial_logs[$index]}" > "$new_log"
  same_resolved=false
  cross_resolved=false
  cross_denied=false
  grep -F '"message":"[INTERNAL-GW] caller identity resolved"' "$new_log" \
    | grep -F "\"app_id\":\"$caller_app_id\"" >/dev/null && same_resolved=true
  grep -F '"message":"[INTERNAL-GW] caller identity resolved"' "$new_log" \
    | grep -F "\"app_id\":\"$cross_caller_app_id\"" >/dev/null && cross_resolved=true
  grep -F '"message":"[INTERNAL-GW] cross-namespace call DENIED"' "$new_log" \
    | grep -F "\"caller_app\":\"$cross_caller_app_id\"" >/dev/null && cross_denied=true
  [[ "$same_resolved" == true && "$cross_resolved" == true && "$cross_denied" == true ]] || {
    echo "Node ${node_ids[$index]} did not record complete workload attribution/namespace denial evidence." >&2
    exit 1
  }
  printf '%s\t%s\t%s\t%s\n' "${node_ids[$index]}" "$same_resolved" "$cross_resolved" "$cross_denied" >> "$identity_tsv"
done

# A removed dependency intentionally leaves its dependent workload deployed,
# but local calls must return a bounded 502 and retained grace-period artifacts
# must not permit an accidental cold start. Redeploying the dependency restores
# the same node-local dependency closure without any remote-node fallback.
dependency_tsv="$work_dir/dependency-results.tsv"
: > "$dependency_tsv"
WASM_CTL_NATS_URL="$nats_url" "$target_dir/release/wasm-ctl" remove "$target_app_id"
deadline=$((SECONDS + 120))
while :; do
  unavailable=0
  for index in 3 6 9; do
    status=$(curl -sS --max-time 10 -o /dev/null -w '%{http_code}' \
      -H "Host: $caller_host" -H "Authorization: Bearer $realm_allowed" \
      "http://${topology[$index]}/call-echo" || true)
    [[ "$status" == 502 ]] && ((unavailable += 1))
  done
  [[ "$unavailable" -eq 3 ]] && break
  ((SECONDS < deadline)) || { echo "Required local dependency did not fail as HTTP 502 on every node." >&2; exit 1; }
  sleep 2
done
printf 'dependency_removed\t502\t502\t3\n' >> "$dependency_tsv"

deploy_manifest "$target_manifest"
deadline=$((SECONDS + 180))
while :; do
  recovered=0
  for index in 3 6 9; do
    status=$(curl -sS --max-time 10 -o /dev/null -w '%{http_code}' \
      -H "Host: $caller_host" -H "Authorization: Bearer $realm_allowed" \
      "http://${topology[$index]}/call-echo" || true)
    [[ "$status" == 200 ]] && ((recovered += 1))
  done
  [[ "$recovered" -eq 3 ]] && break
  ((SECONDS < deadline)) || { echo "Required local dependency did not recover on every node." >&2; exit 1; }
  sleep 2
done
printf 'dependency_redeployed\t200\t200\t3\n' >> "$dependency_tsv"

if [[ -n "$evidence_dir" ]]; then
  mkdir -p "$evidence_dir"
  python3 - "$result_tsv" "$identity_tsv" "$concurrency_tsv" "$dependency_tsv" "$evidence_dir/RESULT_SUMMARY.json" \
    "$target_app_id" "$caller_app_id" "$cross_caller_app_id" "$target_sha" "$caller_sha" "$service_lifetime_gate_secs" <<'PY'
import json, sys
results_path, identity_path, concurrency_path, dependency_path, output_path, target, caller, cross_caller, target_sha, caller_sha, lifetime_gate = sys.argv[1:]
checks = []
for line in open(results_path, encoding="utf-8"):
    name, expected, actual = line.rstrip("\n").split("\t")
    checks.append({"name": name, "expected_status": int(expected), "actual_status": int(actual), "passed": expected == actual})
identity_checks = []
for line in open(identity_path, encoding="utf-8"):
    node, same_resolved, cross_resolved, cross_denied = line.rstrip("\n").split("\t")
    identity_checks.append({
        "node": node,
        "same_namespace_identity_resolved": same_resolved == "true",
        "cross_namespace_identity_resolved": cross_resolved == "true",
        "cross_namespace_identity_denied": cross_denied == "true",
    })
concurrency_checks = []
for line in open(concurrency_path, encoding="utf-8"):
    node, requests, parallelism, succeeded = line.rstrip("\n").split("\t")
    concurrency_checks.append({
        "node": node,
        "requests": int(requests),
        "parallelism": int(parallelism),
        "successful_requests": int(succeeded),
        "passed": requests == succeeded,
    })
dependency_checks = []
for line in open(dependency_path, encoding="utf-8"):
    name, expected, actual, nodes = line.rstrip("\n").split("\t")
    dependency_checks.append({
        "name": name,
        "expected_status": int(expected),
        "actual_status": int(actual),
        "nodes": int(nodes),
        "passed": expected == actual and int(nodes) == 3,
    })
all_passed = (
    all(item["passed"] for item in checks)
    and all(item["passed"] for item in concurrency_checks)
    and all(item["passed"] for item in dependency_checks)
    and all(
        item["same_namespace_identity_resolved"]
        and item["cross_namespace_identity_resolved"]
        and item["cross_namespace_identity_denied"]
        for item in identity_checks
    )
)
summary = {
    "schema_version": 1,
    "result": "pass" if all_passed else "fail",
    "target_application": target,
    "same_namespace_caller": caller,
    "cross_namespace_caller": cross_caller,
    "target_artifact_sha256": target_sha,
    "caller_artifact_sha256": caller_sha,
    "validated_role_forms": ["realm_access.roles", "resource_access.<client_id>.roles"],
    "workload_identity_checks": identity_checks,
    "sustained_concurrency_checks": concurrency_checks,
    "local_dependency_failure_checks": dependency_checks,
    "placement_policy": "every_node",
    "service_lifetime_gate_seconds": int(lifetime_gate),
    "remote_node_fallback": False,
    "internal_dns_name": target.split('/', 1)[1].split(':', 1)[0] + ".validation.internal",
    "cross_host_mesh_identity": "out_of_scope_by_design",
    "checks": checks,
    "secret_material_recorded": False,
}
with open(output_path, "w", encoding="utf-8") as stream:
    json.dump(summary, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
  (cd "$evidence_dir" && sha256sum RESULT_SUMMARY.json > SHA256SUMS)
fi

echo "PASS: east-west internal gateway OIDC role validation completed."
echo "Realm and client roles passed on all nodes; missing/wrong roles, forged headers, and cross-namespace calls were denied."
