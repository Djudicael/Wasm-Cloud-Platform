#!/usr/bin/env bash
# Private, disposable RustFS service image from an explicitly pinned OCI image.
set -euo pipefail
umask 077
output_dir=${OUTPUT_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/wasm-cloud-platform-rustfs}
source_image=${RUSTFS_IMAGE:?Set RUSTFS_IMAGE to an immutable image digest or sha256 image ID}
size_mb=${ROOTFS_SIZE_MB:-2048}
[[ $(uname -s) == Linux ]] || { echo 'Run in Linux/WSL2.' >&2; exit 1; }
[[ "$source_image" =~ @sha256:[a-f0-9]{64}$ || "$source_image" =~ ^sha256:[a-f0-9]{64}$ ]] || { echo 'An immutable OCI image reference is required.' >&2; exit 2; }
[[ "$size_mb" =~ ^[0-9]+$ && "$size_mb" -ge 1024 ]] || exit 2
for cmd in podman mkfs.ext4 openssl realpath; do command -v "$cmd" >/dev/null; done
install -d -m 0700 "$output_dir"
output_dir=$(realpath "$output_dir")
[[ "$output_dir" != /mnt/* ]] || { echo 'Keep credential-bearing images on the private Linux filesystem.' >&2; exit 2; }
image="$output_dir/rustfs-rootfs.ext4"
[[ ! -e "$image" ]] || { echo "Refusing to replace existing image: $image" >&2; exit 1; }
privileged() {
  if (( EUID == 0 )); then "$@";
  elif [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then wsl.exe -u root -- "$@";
  elif [[ -n ${WSL_DISTRO_NAME:-} && -x /mnt/c/Windows/System32/wsl.exe ]]; then /mnt/c/Windows/System32/wsl.exe -u root -- "$@";
  else sudo -n "$@"; fi
}
work=$(mktemp -d /tmp/wcp-rustfs-image.XXXXXX)
container=
cleanup() {
  if [[ -n "$container" ]]; then podman rm "$container" >/dev/null || true; fi
  [[ "$work" == /tmp/wcp-rustfs-image.* ]] && privileged rm -rf -- "$work"
}
trap cleanup EXIT
container=$(podman create "$source_image")
mkdir "$work/rootfs"
podman export "$container" | tar -xf - -C "$work/rootfs"
rootfs="$work/rootfs"
for dir in etc sbin usr usr/bin etc/rustfs data proc sys dev run; do
  [[ ! -L "$rootfs/$dir" ]] || { echo "Unsupported symlinked image directory: $dir" >&2; exit 1; }
done
[[ -x "$rootfs/usr/bin/rustfs" && -x "$rootfs/bin/busybox" && -e "$rootfs/sbin/ip" ]] || { echo 'Expected RustFS Alpine image with BusyBox and iproute support.' >&2; exit 1; }
applets=$(privileged chroot "$rootfs" /bin/busybox --list)
for applet in ntpd timeout; do
  grep -qx "$applet" <<< "$applets" || { echo "BusyBox must provide $applet." >&2; exit 1; }
done
if [[ ! -s "$output_dir/root-access-key.value" ]]; then openssl rand -hex 12 > "$output_dir/root-access-key.value"; fi
if [[ ! -s "$output_dir/root-secret-key.value" ]]; then openssl rand -hex 32 > "$output_dir/root-secret-key.value"; fi
mkdir -p "$rootfs/etc/rustfs" "$rootfs/data" "$rootfs/proc" "$rootfs/sys" "$rootfs/dev" "$rootfs/run"
{
  printf 'RUSTFS_ACCESS_KEY=%s\n' "$(tr -d '\r\n' < "$output_dir/root-access-key.value")"
  printf 'RUSTFS_SECRET_KEY=%s\n' "$(tr -d '\r\n' < "$output_dir/root-secret-key.value")"
} > "$rootfs/etc/rustfs/credentials.env"
chmod 0600 "$rootfs/etc/rustfs/credentials.env"
rm -f "$rootfs/sbin/init"
cat > "$rootfs/sbin/init" <<'INIT'
#!/bin/sh
set -eu
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t tmpfs tmpfs /run
ip_address= gateway=
for arg in $(cat /proc/cmdline); do
  case "$arg" in wcp.ip=*) ip_address=${arg#wcp.ip=} ;; wcp.gateway=*) gateway=${arg#wcp.gateway=} ;; esac
done
test -n "$ip_address" && test -n "$gateway"
ip link set lo up
ip link set eth0 up
ip address add "$ip_address/24" dev eth0
ip route replace default via "$gateway" dev eth0
# SigV4 rejects clock skew. Rehearsal guests can drift after WSL suspension;
# synchronize before accepting S3 requests and keep the clock disciplined.
# Public peers are disposable-test defaults, not production time policy.
/bin/busybox timeout 30 /bin/busybox ntpd -n -q -p 162.159.200.1 -p 162.159.200.123 || {
  echo 'Initial clock synchronization failed; refusing to start RustFS.' >&2
  exit 1
}
/bin/busybox ntpd -p 162.159.200.1 -p 162.159.200.123
set -a
. /etc/rustfs/credentials.env
set +a
export RUSTFS_ADDRESS=0.0.0.0:9000 RUSTFS_CONSOLE_ENABLE=false RUSTFS_VOLUMES=/data RUST_LOG=warn
exec /usr/bin/rustfs /data
INIT
chmod 0755 "$rootfs/sbin/init"
printf '1\n' > "$rootfs/etc/rustfs-image-schema-version"
printf '%s\n' "$source_image" > "$output_dir/source-image.txt"
privileged chown -R root:root "$rootfs"
truncate -s "${size_mb}M" "$image"
privileged mkfs.ext4 -q -F -d "$rootfs" "$image"
chmod 0600 "$image"
echo "Private RustFS image ready: $image"
