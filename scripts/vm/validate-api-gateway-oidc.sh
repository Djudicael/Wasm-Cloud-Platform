#!/usr/bin/env bash
# Deploy a small WASI application and validate the platform API gateway with
# real tokens issued by the local OpenID Connect WASI Hub.

set -euo pipefail

state_file=.prod-validation-single-host-state.json
public_url=http://127.0.0.1:8088
route_host=gateway-auth.internal
evidence_dir=

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --public-url) public_url=${2:?missing public URL}; shift 2 ;;
    --route-host) route_host=${2:?missing route host}; shift 2 ;;
    --evidence-dir) evidence_dir=${2:?missing evidence directory}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-api-gateway-oidc.sh [--state-file PATH] [--public-url URL] [--route-host HOST] [--evidence-dir PATH]"
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

expected_issuer=$public_url
expected_audience=admin-ui
expected_jwks=$(python3 - "$state_file" "$public_url" <<'PY'
import json, sys, urllib.parse
with open(sys.argv[1], encoding="utf-8") as stream:
    gateway = json.load(stream)["gateway"]
parsed = urllib.parse.urlsplit(sys.argv[2])
port = parsed.port or (443 if parsed.scheme == "https" else 80)
print(f"http://{gateway}:{port}/oidc/jwks")
PY
)
python3 - "$state_file" "$expected_issuer" "$expected_audience" "$expected_jwks" <<'PY'
import json, sys
path, issuer, audience, jwks = sys.argv[1:]
with open(path, encoding="utf-8") as stream:
    state = json.load(stream)
expected = {
    "node_oidc_issuer_url": issuer,
    "node_oidc_audience": audience,
    "node_oidc_jwks_url": jwks,
}
for key, value in expected.items():
    if state.get(key) != value:
        raise SystemExit(f"state {key} mismatch: expected {value!r}, got {state.get(key)!r}")
if len(state.get("nodes", [])) < 3:
    raise SystemExit("the production-like gateway validation requires at least three nodes")
PY

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT
chmod 700 "$work_dir"
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}

echo "Building hello-axum for wasm32-wasip2..."
CARGO_TARGET_DIR="$target_dir" cargo build -p hello-axum --target wasm32-wasip2 --release
artifact="$target_dir/wasm32-wasip2/release/hello-axum.wasm"
[[ -s "$artifact" ]] || { echo "Missing WASI artifact: $artifact" >&2; exit 1; }
artifact_sha=$(sha256sum "$artifact" | cut -d' ' -f1)
policy_sha=$(printf '%s' 'public-health;authenticated-root;admin-scope-health;missing-gateway-admin' | sha256sum | cut -d' ' -f1)
version="oidc-${artifact_sha:0:8}-${policy_sha:0:4}"
app_id="validation/gateway-auth-hello:$version"

manifest="$work_dir/deploy.toml"
cat > "$manifest" <<EOF
[app]
name = "gateway-auth-hello"
version = "$version"
namespace = "validation"
wasm_artifact = "$artifact"

[fuel]
quota = 500000000
memory_pages = 2048
max_instances = 10
idle_timeout_secs = 300

[gateway]
host = "$route_host"

[gateway.auth]
policy = "authenticated"

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/app-health"
methods = ["GET"]
auth = "authenticated"
required_scopes = ["admin"]

[[gateway.endpoints]]
path = "/requires-gateway-admin"
methods = ["GET"]
auth = "authenticated"
required_scopes = ["gateway:admin"]
EOF

mapfile -t topology < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
print(state["nats_url"])
for node in state["nodes"]:
    print(node["admin_addr"])
    print(node["artifact_addr"])
    print(node["proxy_addr"])
PY
)
nats_url=${topology[0]}
node_api="http://${topology[1]}"
artifact_api="http://${topology[2]}"

echo "Deploying $app_id with endpoint-level gateway policy..."
WASM_CTL_NATS_URL="$nats_url" \
WASM_CTL_NODE_API="$node_api" \
WASM_CTL_AUTH_TOKEN=local-test-write-token-change-me \
  "$target_dir/release/wasm-ctl" deploy --manifest "$manifest" --artifact-api "$artifact_api"

state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
secret_dir="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-oidc-secrets-$(id -u)/$state_key"
credentials="$secret_dir/credentials.txt"
private_key="$secret_dir/rsa.pem"
[[ -r "$credentials" && -r "$private_key" ]] || {
  echo "OIDC deployment credentials or signing key are unavailable for the local test." >&2
  exit 1
}
admin_email=$(sed -n 's/^Admin email: //p' "$credentials")
admin_password=$(sed -n 's/^Admin password: //p' "$credentials")
[[ -n "$admin_email" && -n "$admin_password" ]] || { echo "Invalid local credential file." >&2; exit 1; }

login_payload="$work_dir/login.json"
login_response="$work_dir/login-response.json"
python3 - "$admin_email" "$admin_password" > "$login_payload" <<'PY'
import json, sys
print(json.dumps({"email": sys.argv[1], "password": sys.argv[2], "client_id": "admin-ui"}))
PY
chmod 600 "$login_payload" "$login_response" 2>/dev/null || true
curl -fsS --max-time 30 -H 'Content-Type: application/json' \
  --data-binary "@$login_payload" "$public_url/oidc/login" > "$login_response"
token_file="$work_dir/token"
claims_file="$work_dir/claims.json"
python3 - "$login_response" "$token_file" "$claims_file" "$expected_issuer" "$expected_audience" <<'PY'
import base64, json, os, sys
response_path, token_path, claims_path, expected_issuer, expected_audience = sys.argv[1:]
with open(response_path, encoding="utf-8") as stream:
    token = json.load(stream)["access_token"]
parts = token.split(".")
if len(parts) != 3:
    raise SystemExit("OIDC access token is not a JWT")
claims = json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4)))
if claims.get("iss") != expected_issuer:
    raise SystemExit(f"unexpected token issuer: {claims.get('iss')!r}")
aud = claims.get("aud")
if expected_audience not in ([aud] if isinstance(aud, str) else aud or []):
    raise SystemExit(f"unexpected token audience: {aud!r}")
with open(token_path, "w", encoding="utf-8") as stream:
    stream.write(token)
os.chmod(token_path, 0o600)
safe = {key: claims.get(key) for key in ("iss", "aud", "scope", "roles", "iat", "exp")}
with open(claims_path, "w", encoding="utf-8") as stream:
    json.dump(safe, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
access_token=$(<"$token_file")

make_signed_variant() {
  local mode=$1 output=$2 signing_input signature
  signing_input="$work_dir/signing-$mode"
  python3 - "$token_file" "$mode" > "$signing_input" <<'PY'
import base64, json, sys, time
token_path, mode = sys.argv[1:]
token = open(token_path, encoding="utf-8").read()
header_part, claims_part, _ = token.split(".")
decode = lambda value: json.loads(base64.urlsafe_b64decode(value + "=" * (-len(value) % 4)))
encode = lambda value: base64.urlsafe_b64encode(json.dumps(value, separators=(",", ":")).encode()).rstrip(b"=").decode()
header, claims = decode(header_part), decode(claims_part)
if mode == "wrong-audience":
    claims["aud"] = "not-the-platform-audience"
elif mode == "expired":
    claims["iat"] = int(time.time()) - 600
    claims["exp"] = int(time.time()) - 300
else:
    raise SystemExit("unsupported JWT variant")
print(f"{encode(header)}.{encode(claims)}", end="")
PY
  signature=$(openssl dgst -sha256 -sign "$private_key" -binary "$signing_input" | openssl base64 -A | tr '+/' '-_' | tr -d '=')
  printf '%s.%s' "$(<"$signing_input")" "$signature" > "$output"
  chmod 600 "$output"
}
make_signed_variant wrong-audience "$work_dir/wrong-audience-token"
make_signed_variant expired "$work_dir/expired-token"
wrong_audience_token=$(<"$work_dir/wrong-audience-token")
expired_token=$(<"$work_dir/expired-token")

result_tsv="$work_dir/results.tsv"
: > "$result_tsv"
request() {
  local name=$1 url=$2 expected=$3 token=${4:-} body status
  body="$work_dir/body-$name"
  local -a args=(-sS --max-time 15 -o "$body" -w '%{http_code}' -H "Host: $route_host")
  [[ -n "$token" ]] && args+=(-H "Authorization: Bearer $token")
  status=$(curl "${args[@]}" "$url" || true)
  printf '%s\t%s\t%s\n' "$name" "$expected" "$status" >> "$result_tsv"
  [[ "$status" == "$expected" ]] || {
    echo "Validation $name failed: expected HTTP $expected, got $status" >&2
    return 1
  }
}

deadline=$((SECONDS + 180))
while :; do
  ready=true
  for index in 3 6 9; do
    status=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' -H "Host: $route_host" "http://${topology[$index]}/health" || true)
    [[ "$status" == 200 ]] || ready=false
  done
  $ready && break
  ((SECONDS < deadline)) || { echo "Application did not become ready on all nodes." >&2; exit 1; }
  sleep 2
done

for index in 3 6 9; do
  base="http://${topology[$index]}"
  suffix="node$(((index - 3) / 3))"
  request "$suffix-public" "$base/health" 200
  request "$suffix-missing-token" "$base/" 401
  request "$suffix-valid-token" "$base/" 200 "$access_token"
done
request front-public "$public_url/health" 200
request front-missing-token "$public_url/" 401
request front-malformed-token "$public_url/" 401 'not-a-jwt'
request front-wrong-audience "$public_url/" 401 "$wrong_audience_token"
request front-expired-token "$public_url/" 401 "$expired_token"
request front-valid-token "$public_url/" 200 "$access_token"
request front-valid-scope "$public_url/app-health" 200 "$access_token"
request front-missing-scope "$public_url/requires-gateway-admin" 403 "$access_token"

# Keep one active immutable validation identity. Cleanup is deliberately scoped
# to older versions of this exact application in the validation namespace.
deployed_apps=$(curl -fsS --max-time 10 \
  -H 'Authorization: Bearer local-test-write-token-change-me' \
  "$node_api/admin/apps?namespace=validation")
mapfile -t superseded_app_ids < <(python3 - "$app_id" "$deployed_apps" <<'PY'
import json, sys
current, raw = sys.argv[1:]
for app in json.loads(raw):
    app_id = app.get("id", "")
    if app_id.startswith("validation/gateway-auth-hello:") and app_id != current:
        print(app_id)
PY
)
for superseded_app_id in "${superseded_app_ids[@]}"; do
  WASM_CTL_NATS_URL="$nats_url" "$target_dir/release/wasm-ctl" remove "$superseded_app_id"
done

if [[ -n "$evidence_dir" ]]; then
  mkdir -p "$evidence_dir"
  python3 - "$result_tsv" "$claims_file" "$evidence_dir/RESULT_SUMMARY.json" "$app_id" "$artifact_sha" <<'PY'
import json, sys
results_path, claims_path, output_path, app_id, artifact_sha = sys.argv[1:]
checks = []
with open(results_path, encoding="utf-8") as stream:
    for line in stream:
        name, expected, actual = line.rstrip("\n").split("\t")
        checks.append({"name": name, "expected_status": int(expected), "actual_status": int(actual), "passed": expected == actual})
with open(claims_path, encoding="utf-8") as stream:
    claims = json.load(stream)
summary = {
    "schema_version": 1,
    "result": "pass" if all(item["passed"] for item in checks) else "fail",
    "application": app_id,
    "artifact_sha256": artifact_sha,
    "token_claim_metadata": claims,
    "checks": checks,
    "secret_material_recorded": False,
}
with open(output_path, "w", encoding="utf-8") as stream:
    json.dump(summary, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
  (cd "$evidence_dir" && sha256sum RESULT_SUMMARY.json > SHA256SUMS)
fi

echo "PASS: platform API gateway OIDC validation completed for $app_id"
echo "Public /health=200; protected /=401 without token and 200 with token; scope checks returned 200 when present and 403 when absent."
