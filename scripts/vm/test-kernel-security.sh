#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
source scripts/vm/kernel-testbed.env
kernel="assets/vmlinux-$KERNEL_SERIES"
work_dir=$(mktemp -d)
cleanup() { rm -rf -- "$work_dir"; }
trap cleanup EXIT

bash scripts/vm/audit-kernel-security.sh \
  --kernel "$kernel" --config "$kernel.config" --output "$work_dir/static.json"

cp "$kernel.config" "$work_dir/unsafe.config"
sed -i 's/^CONFIG_CPU_MITIGATIONS=y$/# CONFIG_CPU_MITIGATIONS is not set/' "$work_dir/unsafe.config"
if bash scripts/vm/audit-kernel-security.sh \
  --kernel "$kernel" --config "$work_dir/unsafe.config" >/dev/null 2>&1; then
  echo "static audit accepted a kernel config without CPU mitigations" >&2
  exit 1
fi

cat > "$work_dir/safe-serial.log" <<EOF
WCP_KERNEL_AUDIT_BEGIN
WCP_KERNEL_RELEASE=$KERNEL_VERSION
WCP_KERNEL_VULNERABILITY=meltdown|Not affected
WCP_KERNEL_VULNERABILITY=spec_store_bypass|Mitigation: Speculative Store Bypass disabled
WCP_KERNEL_VULNERABILITY=spectre_v1|Mitigation: usercopy/swapgs barriers and __user pointer sanitization
WCP_KERNEL_VULNERABILITY=spectre_v2|Mitigation: Retpolines; IBPB conditional
WCP_KERNEL_AUDIT_END
EOF
bash scripts/vm/audit-kernel-runtime.sh \
  --serial-log "$work_dir/safe-serial.log" --output "$work_dir/runtime.json"

sed 's/Mitigation: Retpolines; IBPB conditional/Vulnerable/' \
  "$work_dir/safe-serial.log" > "$work_dir/unsafe-serial.log"
if bash scripts/vm/audit-kernel-runtime.sh \
  --serial-log "$work_dir/unsafe-serial.log" >/dev/null 2>&1; then
  echo "runtime audit accepted a vulnerable CPU status" >&2
  exit 1
fi

echo "kernel security audit tests passed"
