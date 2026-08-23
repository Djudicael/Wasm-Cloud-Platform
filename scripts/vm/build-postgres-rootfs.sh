#!/usr/bin/env bash
# Build the PostgreSQL service rootfs used by the local microVM testbed.

set -euo pipefail

OUTPUT_DIR=${OUTPUT_DIR:-./assets}
ROOTFS_SIZE_MB=${ROOTFS_SIZE_MB:-1024}
ALPINE_VERSION=${ALPINE_VERSION:-3.21}
POSTGRES_DATABASE=${POSTGRES_DATABASE:-oidc}
POSTGRES_USER=${POSTGRES_USER:-oidc}
POSTGRES_PASSWORD=${POSTGRES_PASSWORD:-oidc-local-test}

[[ "$POSTGRES_DATABASE" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || {
  echo "POSTGRES_DATABASE must be a simple SQL identifier." >&2
  exit 2
}
[[ "$POSTGRES_USER" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || {
  echo "POSTGRES_USER must be a simple SQL identifier." >&2
  exit 2
}
[[ "$POSTGRES_PASSWORD" =~ ^[A-Za-z0-9._-]+$ ]] || {
  echo "POSTGRES_PASSWORD may contain only letters, digits, dot, underscore, and dash." >&2
  exit 2
}

echo "=== Building PostgreSQL rootfs ==="
work_dir=$(mktemp -d)
cleanup() {
  if mountpoint -q "$work_dir/mount" 2>/dev/null; then
    sudo umount "$work_dir/mount"
  fi
  rm -rf -- "$work_dir"
}
trap cleanup EXIT

rootfs_dir="$work_dir/rootfs"
mkdir -p "$rootfs_dir"
arch=$(uname -m)
curl -fsSL \
  "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${arch}/alpine-minirootfs-${ALPINE_VERSION}.0-${arch}.tar.gz" \
  -o "$work_dir/alpine-rootfs.tar.gz"
tar xzf "$work_dir/alpine-rootfs.tar.gz" -C "$rootfs_dir"

mkdir -p "$rootfs_dir/etc/apk"
printf '%s\n' \
  "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main" \
  "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community" \
  > "$rootfs_dir/etc/apk/repositories"

sudo cp -L /etc/resolv.conf "$rootfs_dir/etc/resolv.conf"
sudo chroot "$rootfs_dir" /sbin/apk add --no-cache \
  alpine-base openrc iproute2 postgresql postgresql-client postgresql17-contrib \
  su-exec ca-certificates
sudo chown -R "$(id -u):$(id -g)" "$rootfs_dir"

mkdir -p \
  "$rootfs_dir/etc/init.d" \
  "$rootfs_dir/etc/runlevels/default" \
  "$rootfs_dir/var/lib/postgresql/data" \
  "$rootfs_dir/run/postgresql" \
  "$rootfs_dir/proc" \
  "$rootfs_dir/sys" \
  "$rootfs_dir/dev" \
  "$rootfs_dir/tmp"
sudo chroot "$rootfs_dir" chown -R postgres:postgres /var/lib/postgresql /run/postgresql

cat > "$rootfs_dir/etc/init.d/postgresql-testbed" <<'EOF'
#!/sbin/openrc-run

description="PostgreSQL for the Wasm Cloud Platform local testbed"
command="/usr/bin/postgres"
command_args="-D /var/lib/postgresql/data"
command_user="postgres:postgres"
command_background=true
pidfile="/run/postgresql/postmaster.pid"

depend() {
  need net
}

start_pre() {
  checkpath -d -m 0700 -o postgres:postgres /var/lib/postgresql/data
  checkpath -d -m 0755 -o postgres:postgres /run/postgresql
  if [ ! -s /var/lib/postgresql/data/PG_VERSION ]; then
    su-exec postgres initdb --encoding=UTF8 --locale=C -D /var/lib/postgresql/data
    cat >> /var/lib/postgresql/data/postgresql.conf <<'CONFIG'
listen_addresses = '*'
port = 5432
password_encryption = 'scram-sha-256'
max_connections = 200
shared_buffers = '128MB'
CONFIG
    cat > /var/lib/postgresql/data/pg_hba.conf <<'HBA'
local all all trust
host all all 172.20.0.0/24 scram-sha-256
HBA
    chown postgres:postgres /var/lib/postgresql/data/postgresql.conf /var/lib/postgresql/data/pg_hba.conf
  fi
}

start_post() {
  for _ in $(seq 1 60); do
    su-exec postgres pg_isready -q && break
    sleep 0.25
  done
  /usr/local/bin/init-oidc-database
}
EOF
chmod +x "$rootfs_dir/etc/init.d/postgresql-testbed"
ln -s /etc/init.d/postgresql-testbed "$rootfs_dir/etc/runlevels/default/postgresql-testbed"

cat > "$rootfs_dir/usr/local/bin/init-oidc-database" <<EOF
#!/bin/sh
set -eu
if ! su-exec postgres psql -tAc "SELECT 1 FROM pg_roles WHERE rolname = '$POSTGRES_USER'" | grep -q 1; then
  su-exec postgres psql -v ON_ERROR_STOP=1 -c "CREATE ROLE $POSTGRES_USER LOGIN PASSWORD '$POSTGRES_PASSWORD'"
fi
if ! su-exec postgres psql -tAc "SELECT 1 FROM pg_database WHERE datname = '$POSTGRES_DATABASE'" | grep -q 1; then
  su-exec postgres createdb --owner "$POSTGRES_USER" "$POSTGRES_DATABASE"
fi
EOF
chmod +x "$rootfs_dir/usr/local/bin/init-oidc-database"

cat > "$rootfs_dir/etc/inittab" <<'EOF'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
EOF

printf '%s\n' postgres-vm > "$rootfs_dir/etc/hostname"
cat > "$rootfs_dir/etc/network/interfaces" <<'EOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 172.20.0.20
    netmask 255.255.255.0
    gateway 172.20.0.1
EOF

mkdir -p "$OUTPUT_DIR"
image="$OUTPUT_DIR/postgres-rootfs.ext4"
dd if=/dev/zero of="$image" bs=1M count="$ROOTFS_SIZE_MB" status=progress
mkfs.ext4 -F "$image"
mkdir -p "$work_dir/mount"
sudo mount -o loop "$image" "$work_dir/mount"
sudo cp -a "$rootfs_dir"/. "$work_dir/mount"/
sudo umount "$work_dir/mount"

echo "PostgreSQL rootfs ready: $image"
