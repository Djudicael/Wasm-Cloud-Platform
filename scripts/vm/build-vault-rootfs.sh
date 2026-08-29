#!/usr/bin/env bash
# Build the sealed HashiCorp Vault service image used by the local testbed.

set -euo pipefail

OUTPUT_DIR=${OUTPUT_DIR:-./assets}
ROOTFS_SIZE_MB=${ROOTFS_SIZE_MB:-768}
ALPINE_VERSION=${ALPINE_VERSION:-3.21}
VAULT_VERSION=${VAULT_VERSION:-1.21.4}
VAULT_IP=${VAULT_IP:-172.20.0.21}
VAULT_TRANSIT_KEY=${VAULT_TRANSIT_KEY:-wasm-platform-seal}
image="$OUTPUT_DIR/vault-rootfs.ext4"
bootstrap=${VAULT_BOOTSTRAP_FILE:-${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-build-$(id -u)/bootstrap.json}
ca_output="$OUTPUT_DIR/vault-test-ca.crt"
schema=1

case "$(uname -m)" in
  x86_64) vault_arch=amd64; vault_sha=889b681990fe221b884b7932fa9c9dd0ee9811b9349554f1aa287ab63c9f3dae ;;
  aarch64) vault_arch=arm64; vault_sha=1104ef701aad16e104e2e7b4d2a02a6ec993237559343f3097ac63a00b42e85d ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

for command in curl jq openssl unzip mkfs.ext4; do
  command -v "$command" >/dev/null || { echo "$command is required." >&2; exit 1; }
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
mkdir -p "$OUTPUT_DIR"
install -d -m 0700 "$(dirname "$bootstrap")"
work_dir=$(mktemp -d)
vault_pid=
cleanup() {
  if [[ -n "$vault_pid" ]] && kill -0 "$vault_pid" 2>/dev/null; then
    kill "$vault_pid" 2>/dev/null || true
    wait "$vault_pid" 2>/dev/null || true
  fi
  if mountpoint -q "$work_dir/mount" 2>/dev/null; then
    sudo umount "$work_dir/mount"
  fi
  sudo rm -rf -- "$work_dir"
}
trap cleanup EXIT

echo "=== Building Vault $VAULT_VERSION rootfs ==="
vault_zip="$work_dir/vault.zip"
curl -fsSL "https://releases.hashicorp.com/vault/${VAULT_VERSION}/vault_${VAULT_VERSION}_linux_${vault_arch}.zip" -o "$vault_zip"
printf '%s  %s\n' "$vault_sha" "$vault_zip" | sha256sum -c -
unzip -q "$vault_zip" -d "$work_dir/vault-bin"

rootfs="$work_dir/rootfs"
mkdir -p "$rootfs"
curl -fsSL \
  "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/$(uname -m)/alpine-minirootfs-${ALPINE_VERSION}.0-$(uname -m).tar.gz" \
  -o "$work_dir/alpine-rootfs.tar.gz"
tar xzf "$work_dir/alpine-rootfs.tar.gz" -C "$rootfs"
mkdir -p "$rootfs/etc/apk"
printf '%s\n' \
  "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main" \
  "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community" \
  > "$rootfs/etc/apk/repositories"
sudo cp -L /etc/resolv.conf "$rootfs/etc/resolv.conf"
sudo chroot "$rootfs" /sbin/apk add --no-cache alpine-base ca-certificates curl iproute2
sudo chown -R "$(id -u):$(id -g)" "$rootfs"

install -D -m 0755 "$work_dir/vault-bin/vault" "$rootfs/usr/local/bin/vault"
mkdir -p "$rootfs/etc/vault/tls" "$rootfs/var/lib/vault/data" "$rootfs/proc" "$rootfs/sys" "$rootfs/dev" "$rootfs/run"
echo "$schema" > "$rootfs/etc/vault-image-schema-version"

openssl genrsa -out "$work_dir/ca.key" 3072 2>/dev/null
openssl req -x509 -new -sha256 -days 30 -key "$work_dir/ca.key" \
  -subj '/CN=Wasm Cloud Platform local Vault CA' -out "$work_dir/ca.crt"
openssl genrsa -out "$rootfs/etc/vault/tls/server.key" 3072 2>/dev/null
openssl req -new -key "$rootfs/etc/vault/tls/server.key" \
  -subj '/CN=vault.service.internal' -out "$work_dir/server.csr"
cat > "$work_dir/server.ext" <<EOF
subjectAltName=DNS:vault.service.internal,IP:${VAULT_IP},IP:127.0.0.1
extendedKeyUsage=serverAuth
keyUsage=digitalSignature,keyEncipherment
EOF
openssl x509 -req -sha256 -days 30 -in "$work_dir/server.csr" \
  -CA "$work_dir/ca.crt" -CAkey "$work_dir/ca.key" -CAcreateserial \
  -extfile "$work_dir/server.ext" -out "$rootfs/etc/vault/tls/server.crt" 2>/dev/null
install -m 0644 "$work_dir/ca.crt" "$rootfs/etc/vault/tls/ca.crt"
chmod 0600 "$rootfs/etc/vault/tls/server.key"

host_port=$(python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
)
cat > "$work_dir/vault-host.hcl" <<EOF
disable_mlock = true
storage "file" { path = "$rootfs/var/lib/vault/data" }
listener "tcp" {
  address = "127.0.0.1:${host_port}"
  tls_cert_file = "$rootfs/etc/vault/tls/server.crt"
  tls_key_file = "$rootfs/etc/vault/tls/server.key"
}
api_addr = "https://127.0.0.1:${host_port}"
EOF
"$rootfs/usr/local/bin/vault" server -config="$work_dir/vault-host.hcl" >"$work_dir/vault-build.log" 2>&1 &
vault_pid=$!
export VAULT_ADDR="https://127.0.0.1:${host_port}"
export VAULT_CACERT="$work_dir/ca.crt"
for _ in $(seq 1 100); do
  curl -fsS --cacert "$VAULT_CACERT" "$VAULT_ADDR/v1/sys/health?uninitcode=200&sealedcode=200" >/dev/null 2>&1 && break
  kill -0 "$vault_pid" 2>/dev/null || { echo "Vault exited during image initialization." >&2; exit 1; }
  sleep 0.1
done

init_json=$("$rootfs/usr/local/bin/vault" operator init -key-shares=1 -key-threshold=1 -format=json)
unseal_key=$(jq -er '.unseal_keys_b64[0]' <<<"$init_json")
root_token=$(jq -er '.root_token' <<<"$init_json")
curl -fsS --cacert "$VAULT_CACERT" -H 'Content-Type: application/json' \
  --data-binary @- "$VAULT_ADDR/v1/sys/unseal" \
  <<<"$(jq -cn --arg key "$unseal_key" '{key:$key}')" >/dev/null
export VAULT_TOKEN="$root_token"
"$rootfs/usr/local/bin/vault" secrets enable transit >/dev/null
"$rootfs/usr/local/bin/vault" write "transit/keys/$VAULT_TRANSIT_KEY" \
  type=hmac key_size=32 exportable=false allow_plaintext_backup=false >/dev/null
"$rootfs/usr/local/bin/vault" auth enable approle >/dev/null

cat > "$work_dir/node-policy.hcl" <<EOF
path "transit/hmac/${VAULT_TRANSIT_KEY}/sha2-256" {
  capabilities = ["update"]
}
path "transit/keys/${VAULT_TRANSIT_KEY}" {
  capabilities = ["read"]
}
EOF
cat > "$work_dir/operator-policy.hcl" <<EOF
path "transit/keys/${VAULT_TRANSIT_KEY}" {
  capabilities = ["read"]
}
path "transit/keys/${VAULT_TRANSIT_KEY}/rotate" {
  capabilities = ["update"]
}
EOF
"$rootfs/usr/local/bin/vault" policy write wasm-node-seal "$work_dir/node-policy.hcl" >/dev/null
"$rootfs/usr/local/bin/vault" policy write wasm-seal-operator "$work_dir/operator-policy.hcl" >/dev/null
for role in wasm-node-seal wasm-seal-operator; do
  "$rootfs/usr/local/bin/vault" write "auth/approle/role/$role" \
    "token_policies=$role" token_type=batch token_ttl=15m token_max_ttl=30m \
    secret_id_ttl=10m secret_id_num_uses=1 \
    secret_id_bound_cidrs=172.20.0.1/32 token_bound_cidrs=172.20.0.1/32 >/dev/null
done
node_role_id=$("$rootfs/usr/local/bin/vault" read -field=role_id auth/approle/role/wasm-node-seal/role-id)
operator_role_id=$("$rootfs/usr/local/bin/vault" read -field=role_id auth/approle/role/wasm-seal-operator/role-id)
"$rootfs/usr/local/bin/vault" operator seal >/dev/null
kill "$vault_pid"
wait "$vault_pid" || true
vault_pid=

jq -n \
  --arg unseal_key "$unseal_key" \
  --arg root_token "$root_token" \
  --arg node_role_id "$node_role_id" \
  --arg operator_role_id "$operator_role_id" \
  --arg transit_key "$VAULT_TRANSIT_KEY" \
  --arg version "$VAULT_VERSION" \
  '{schema_version:1, unseal_key:$unseal_key, root_token:$root_token, node_role_id:$node_role_id, operator_role_id:$operator_role_id, transit_key:$transit_key, vault_version:$version}' \
  > "$bootstrap"
chmod 0600 "$bootstrap"
install -m 0644 "$work_dir/ca.crt" "$ca_output"

cat > "$rootfs/etc/vault/server.hcl" <<EOF
disable_mlock = true
storage "file" { path = "/var/lib/vault/data" }
listener "tcp" {
  address = "0.0.0.0:8200"
  tls_cert_file = "/etc/vault/tls/server.crt"
  tls_key_file = "/etc/vault/tls/server.key"
}
api_addr = "https://${VAULT_IP}:8200"
EOF
rm -f "$rootfs/sbin/init"
cat > "$rootfs/sbin/init" <<EOF
#!/bin/sh
set -eu
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t tmpfs tmpfs /run
ip link set lo up
ip link set eth0 up
ip address add ${VAULT_IP}/24 dev eth0
ip route replace default via 172.20.0.1 dev eth0
exec /usr/local/bin/vault server -config=/etc/vault/server.hcl
EOF
chmod 0755 "$rootfs/sbin/init"
printf '%s\n' vault-test > "$rootfs/etc/hostname"

if [[ -e "$image" ]] && command -v fuser >/dev/null && sudo fuser "$image" >/dev/null 2>&1; then
  echo "Refusing to replace a Vault image currently opened by a microVM: $image" >&2
  exit 1
fi
dd if=/dev/zero of="$image" bs=1M count="$ROOTFS_SIZE_MB" status=progress
mkfs.ext4 -F "$image"
mkdir -p "$work_dir/mount"
sudo mount -o loop "$image" "$work_dir/mount"
sudo cp -a "$rootfs"/. "$work_dir/mount"/
sudo umount "$work_dir/mount"

echo "Vault rootfs ready: $image"
echo "Local test bootstrap (mode 0600): $bootstrap"
echo "Local test CA: $ca_output"
