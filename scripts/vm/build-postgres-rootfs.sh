#!/usr/bin/env bash
# Build the PostgreSQL service rootfs used by the local microVM testbed.

set -euo pipefail

OUTPUT_DIR=${OUTPUT_DIR:-./assets}
ROOTFS_SIZE_MB=${ROOTFS_SIZE_MB:-1024}
ALPINE_VERSION=${ALPINE_VERSION:-3.21}
POSTGRES_DATABASE=${POSTGRES_DATABASE:-oidc}
POSTGRES_USER=${POSTGRES_USER:-oidc}
POSTGRES_PASSWORD=${POSTGRES_PASSWORD:-oidc-local-test}
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
image="$OUTPUT_DIR/postgres-rootfs.ext4"

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

# The canonical service disk is writable and contains the live database. Do not
# truncate it merely because a schema bump made the cached image look stale.
# Firecracker's block backend is not reported reliably by fuser under every WSL
# kernel, so recorded service PIDs are the authoritative additional guard.
canonical_image=$(realpath -m "$image")
canonical_repo_image=$(realpath -m "$repo_root/assets/postgres-rootfs.ext4")
if [[ -e "$image" && "$canonical_image" == "$canonical_repo_image" ]]; then
  command -v jq >/dev/null || {
    echo "jq is required to prove the canonical PostgreSQL image is not live." >&2
    exit 1
  }
  shopt -s nullglob
  for candidate_state in "$repo_root"/.*state*.json "$repo_root"/*state*.json; do
    [[ -f "$candidate_state" ]] || continue
    while IFS= read -r service_pid; do
      if [[ "$service_pid" =~ ^[0-9]+$ ]] && [[ -d "/proc/$service_pid" ]]; then
        echo "Refusing to replace the canonical PostgreSQL image while recorded service PID $service_pid is alive." >&2
        echo "State file: $candidate_state" >&2
        echo "Stop the exact recorded service or build into a different OUTPUT_DIR." >&2
        exit 1
      fi
    done < <(jq -r '.services[]? | select(.kind == "postgresql") | .pid' "$candidate_state" 2>/dev/null || true)
  done
  shopt -u nullglob
fi

echo "=== Building PostgreSQL rootfs ==="
work_dir=$(mktemp -d)
cleanup() {
  if mountpoint -q "$work_dir/mount" 2>/dev/null; then
    sudo umount "$work_dir/mount"
  fi
  # `postgres` owns files created inside the chroot. Use the same privilege
  # boundary used for chroot and mount operations so the EXIT trap cannot turn
  # an otherwise successful image build into a failure on WSL/Linux.
  sudo rm -rf -- "$work_dir"
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
  alpine-base chrony openrc iproute2 postgresql postgresql-client postgresql17-contrib \
  su-exec ca-certificates
postgres_bin=/usr/libexec/postgresql17
for executable in initdb postgres pg_isready psql createdb; do
  sudo chroot "$rootfs_dir" test -x "$postgres_bin/$executable" || {
    echo "PostgreSQL package is missing $postgres_bin/$executable." >&2
    exit 1
  }
done
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
echo "4" > "$rootfs_dir/etc/postgresql-image-schema-version"

cat > "$rootfs_dir/etc/init.d/postgresql-testbed" <<'EOF'
#!/sbin/openrc-run

description="PostgreSQL for the Wasm Cloud Platform local testbed"
command="/usr/libexec/postgresql17/postgres"
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
    su-exec postgres /usr/libexec/postgresql17/initdb --encoding=UTF8 --locale=C -D /var/lib/postgresql/data
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
    su-exec postgres /usr/libexec/postgresql17/pg_isready -q && break
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
if ! su-exec postgres /usr/libexec/postgresql17/psql -tAc "SELECT 1 FROM pg_roles WHERE rolname = '$POSTGRES_USER'" | grep -q 1; then
  su-exec postgres /usr/libexec/postgresql17/psql -v ON_ERROR_STOP=1 -c "CREATE ROLE $POSTGRES_USER LOGIN PASSWORD '$POSTGRES_PASSWORD'"
fi
if ! su-exec postgres /usr/libexec/postgresql17/psql -tAc "SELECT 1 FROM pg_database WHERE datname = '$POSTGRES_DATABASE'" | grep -q 1; then
  su-exec postgres /usr/libexec/postgresql17/createdb --owner "$POSTGRES_USER" "$POSTGRES_DATABASE"
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

# Use a deterministic PID 1 for the disposable PostgreSQL service VM. It owns
# network setup and first-boot initialization instead of depending on OpenRC
# runlevel ordering.
rm -f "$rootfs_dir/sbin/init"
cat > "$rootfs_dir/sbin/init" <<'EOF'
#!/bin/sh
set -eu
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t tmpfs tmpfs /run
mkdir -p /run/postgresql /var/lib/postgresql/data
chown postgres:postgres /run/postgresql /var/lib/postgresql /var/lib/postgresql/data
ip link set lo up
ip link set eth0 up
ip address add 172.20.0.20/24 dev eth0
ip route replace default via 172.20.0.1 dev eth0

# Database timestamps participate in backup recovery points, token/session
# validity, retention, and audit ordering. Firecracker guest clocks can lag when
# a laptop/WSL host resumes, so synchronize before PostgreSQL accepts traffic and
# keep chronyd running. This fixed public source is only for the disposable local
# image; production must use redundant operator-controlled time sources.
chronyd -q -t 15 'server 162.159.200.1 iburst' || {
  echo "initial clock synchronization failed; refusing to start PostgreSQL" >&2
  poweroff -f
  exit 1
}
chronyd 'server 162.159.200.1 iburst' || {
  echo "failed to start continuous clock synchronization" >&2
  poweroff -f
  exit 1
}

if [ ! -s /var/lib/postgresql/data/PG_VERSION ]; then
  su-exec postgres /usr/libexec/postgresql17/initdb --encoding=UTF8 --locale=C -D /var/lib/postgresql/data
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

su-exec postgres /usr/libexec/postgresql17/postgres -D /var/lib/postgresql/data &
postgres_pid=$!
trap 'kill -TERM "$postgres_pid" 2>/dev/null || true' TERM INT
for attempt in $(seq 1 120); do
  su-exec postgres /usr/libexec/postgresql17/pg_isready -q && break
  if ! kill -0 "$postgres_pid" 2>/dev/null; then
    wait "$postgres_pid"
    exit $?
  fi
  sleep 0.25
done
su-exec postgres /usr/libexec/postgresql17/pg_isready -q || {
  echo "PostgreSQL did not become ready during first-boot initialization." >&2
  kill -TERM "$postgres_pid" 2>/dev/null || true
  wait "$postgres_pid" || true
  exit 1
}
/usr/local/bin/init-oidc-database
wait "$postgres_pid"
EOF
chmod +x "$rootfs_dir/sbin/init"

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
if [[ -e "$image" ]]; then
  command -v fuser >/dev/null || {
    echo "fuser is required before replacing an existing PostgreSQL image." >&2
    exit 1
  }
  if sudo fuser "$image" >/dev/null 2>&1; then
    echo "Refusing to replace PostgreSQL image while a microVM has it open: $image" >&2
    echo "Stop the recorded PostgreSQL service or build into a different OUTPUT_DIR." >&2
    exit 1
  fi
fi
dd if=/dev/zero of="$image" bs=1M count="$ROOTFS_SIZE_MB" status=progress
mkfs.ext4 -F "$image"
mkdir -p "$work_dir/mount"
sudo mount -o loop "$image" "$work_dir/mount"
sudo cp -a "$rootfs_dir"/. "$work_dir/mount"/
sudo umount "$work_dir/mount"

echo "PostgreSQL rootfs ready: $image"
