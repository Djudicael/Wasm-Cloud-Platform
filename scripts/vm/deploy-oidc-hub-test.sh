#!/usr/bin/env bash
# Build and deploy OpenID-Connect-WASI-Hub into an existing local testbed.

set -euo pipefail

app_dir=/mnt/d/dev/openid_connect_wasi
state_file=.vm-testbed-state.json
public_url=http://localhost:8088
database_url='postgresql://oidc:oidc-local-test@172.20.0.20:5432/oidc?sslmode=disable'
export WASM_CTL_AUTH_TOKEN=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}
admin_email=admin@example.com
admin_password=Admin123

while (($#)); do
  case "$1" in
    --app-dir) app_dir=${2:?missing application directory}; shift 2 ;;
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --public-url) public_url=${2:?missing public URL}; shift 2 ;;
    --database-url) database_url=${2:?missing database URL}; shift 2 ;;
    --admin-email) admin_email=${2:?missing admin email}; shift 2 ;;
    --admin-password) admin_password=${2:?missing admin password}; shift 2 ;;
    -h|--help)
      echo "Usage: deploy-oidc-hub-test.sh [--app-dir PATH] [--state-file PATH] [--public-url URL]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the platform repository." >&2; exit 1; }
cd "$repo_root"
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
[[ -f "$app_dir/Cargo.toml" && -f "$app_dir/front/admin/package-lock.json" ]] || {
  echo "OpenID Connect Hub checkout is incomplete: $app_dir" >&2
  exit 1
}
for command_name in cargo npm openssl curl python3; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done

state_file=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
secret_dir="${state_file}.oidc-secrets"
mkdir -p "$secret_dir"
chmod 700 "$secret_dir"
if [[ ! -s "$secret_dir/rsa.pem" ]]; then
  openssl genrsa -traditional -out "$secret_dir/rsa.pem" 2048 >/dev/null 2>&1
fi
if [[ ! -s "$secret_dir/ed25519.pem" ]]; then
  openssl genpkey -algorithm ED25519 -out "$secret_dir/ed25519.pem" >/dev/null 2>&1
fi
if [[ ! -s "$secret_dir/encryption-key" ]]; then
  openssl rand -hex 32 > "$secret_dir/encryption-key"
fi
if [[ ! -s "$secret_dir/pairwise-salt" ]]; then
  openssl rand -hex 32 > "$secret_dir/pairwise-salt"
fi
chmod 600 "$secret_dir"/*

rsa_key=$(<"$secret_dir/rsa.pem")
ed25519_key=$(<"$secret_dir/ed25519.pem")
encryption_key=$(<"$secret_dir/encryption-key")
pairwise_salt=$(<"$secret_dir/pairwise-salt")

app_target_dir=${OIDC_CARGO_TARGET_DIR:-/tmp/openid-connect-wasi-target}
echo "Building admin frontend in WSL..."
(cd "$app_dir" && npm --prefix front/admin ci && npm --prefix front/admin run build)

echo "Building both WASI components in WSL..."
(cd "$app_dir" && CARGO_TARGET_DIR="$app_target_dir" cargo build \
  -p oidc-admin-wasi -p openid-connect-wasi --target wasm32-wasip2 --release)

echo "Applying migrations and creating repeatable local test data..."
(cd "$app_dir" && \
  OIDC_DATABASE_URL="$database_url" \
  OIDC_PROXY_PORT=8088 \
  DEFAULT_EMAIL="$admin_email" \
  DEFAULT_PASSWORD="$admin_password" \
  CARGO_TARGET_DIR="$app_target_dir" \
  cargo run -p oidc-wasm-dev --release -- seed)

frontend_wasm="$app_target_dir/wasm32-wasip2/release/oidc_admin_wasi.wasm"
backend_wasm="$app_target_dir/wasm32-wasip2/release/openid_connect_wasi.wasm"
frontend_version="v$(sha256sum "$frontend_wasm" | cut -c1-12)"
backend_fuel=10000000000
# Runtime configuration is part of deployment identity. Otherwise a same-artifact
# redeploy can leave an already-running instance on its previous resource limits.
backend_version="v$(printf '%s\nfuel=%s\n' "$(sha256sum "$backend_wasm" | cut -d' ' -f1)" "$backend_fuel" | sha256sum | cut -c1-12)"

scripts/vm/deploy-test-application.sh \
  --state-file "$state_file" \
  --app oidc-admin-wasi \
  --version "$frontend_version" \
  --namespace oidc \
  --wasm "$frontend_wasm" \
  --route-host oidc-frontend.internal \
  --health-path none

scripts/vm/deploy-test-application.sh \
  --state-file "$state_file" \
  --app openid-connect-wasi \
  --version "$backend_version" \
  --namespace oidc \
  --wasm "$backend_wasm" \
  --route-host oidc-backend.internal \
  --fuel "$backend_fuel" \
  --verify-path /health/ready \
  --env "OIDC_DATABASE_URL=$database_url" \
  --env "OIDC_ISSUER=$public_url" \
  --env "OIDC_ENCRYPTION_KEY=$encryption_key" \
  --env "OIDC_PAIRWISE_SALT=$pairwise_salt" \
  --env "OIDC_SIGNING_KEY=$rsa_key" \
  --env "OIDC_SIGNING_KID=local-rsa-1" \
  --env "OIDC_ED25519_KEY=$ed25519_key" \
  --env "OIDC_ED25519_KID=local-ed25519-1" \
  --env "OIDC_RATE_LIMIT_MODE=proxy" \
  --env "OIDC_TRUST_PROXY_HEADERS=true" \
  --env "OIDC_CORS_ORIGINS="

scripts/vm/configure-oidc-test-gateway.sh --state-file "$state_file"

front_door=${public_url#http://}
login_payload=$(mktemp)
trap 'rm -f "$login_payload"' EXIT
chmod 600 "$login_payload"
python3 - "$admin_email" "$admin_password" > "$login_payload" <<'PY'
import json, sys
print(json.dumps({"email": sys.argv[1], "password": sys.argv[2], "client_id": "admin-ui"}))
PY
deadline=$((SECONDS + 120))
while ((SECONDS < deadline)); do
  frontend_status=$(curl -sS -o /tmp/oidc-frontend-response -w '%{http_code}' --max-time 5 "$public_url/" || true)
  ready_status=$(curl -sS -o /tmp/oidc-ready-response -w '%{http_code}' --max-time 5 "$public_url/health/ready" || true)
  discovery_status=$(curl -sS -o /tmp/oidc-discovery-response -w '%{http_code}' --max-time 5 "$public_url/.well-known/openid-configuration" || true)
  spa_status=$(curl -sS -o /tmp/oidc-spa-response -w '%{http_code}' --max-time 5 "$public_url/realms/master" || true)
  login_status=$(curl -sS -o /tmp/oidc-login-response -w '%{http_code}' --max-time 5 "$public_url/realms/master/login" || true)
  login_api_status=$(curl -sS -o /tmp/oidc-login-api-response -w '%{http_code}' --max-time 30 \
    -H 'Content-Type: application/json' --data-binary "@$login_payload" "$public_url/oidc/login" || true)
  ready_ok=false
  if [[ "$ready_status" == 200 ]] && python3 - /tmp/oidc-ready-response <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    readiness = json.load(stream)
if readiness.get("status") != "ready" or readiness.get("checks", {}).get("database") != "ok":
    raise SystemExit(1)
PY
  then
    ready_ok=true
  fi
  login_api_ok=false
  if [[ "$login_api_status" == 200 ]] && python3 - /tmp/oidc-login-api-response <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    response = json.load(stream)
if not response.get("access_token") or not response.get("id_token"):
    raise SystemExit(1)
PY
  then
    login_api_ok=true
  fi
  if [[ "$frontend_status" == 200 && "$ready_ok" == true && "$discovery_status" == 200 && "$spa_status" == 200 && "$login_status" == 200 && "$login_api_ok" == true ]]; then
    python3 - "$public_url" /tmp/oidc-discovery-response <<'PY'
import json, sys
expected, path = sys.argv[1:]
with open(path, encoding="utf-8") as stream:
    discovery = json.load(stream)
if discovery.get("issuer") != expected:
    raise SystemExit(f"issuer mismatch: expected {expected!r}, got {discovery.get('issuer')!r}")
PY
    break
  fi
  sleep 2
done
[[ "${frontend_status:-}" == 200 && "${ready_ok:-false}" == true && "${discovery_status:-}" == 200 && "${spa_status:-}" == 200 && "${login_status:-}" == 200 && "${login_api_ok:-false}" == true ]] || {
  echo "OIDC validation failed: frontend=${frontend_status:-none} ready=${ready_status:-none} discovery=${discovery_status:-none} spa=${spa_status:-none} login-page=${login_status:-none} login-api=${login_api_status:-none}" >&2
  exit 1
}

cat > "$secret_dir/credentials.txt" <<EOF
Application URL: $public_url
Admin email: $admin_email
Admin password: $admin_password
HAProxy stats: http://${front_door%:*}:8404/stats
EOF
chmod 600 "$secret_dir/credentials.txt"

echo "OIDC Hub is ready for browser testing: $public_url"
echo "Admin login: $admin_email / $admin_password"
echo "HAProxy stats: http://${front_door%:*}:8404/stats"
