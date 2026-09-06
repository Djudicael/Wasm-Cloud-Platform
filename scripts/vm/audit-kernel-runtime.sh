#!/usr/bin/env bash
set -euo pipefail

usage() { echo "usage: $0 --serial-log <guest-serial.log> [--output <json>]" >&2; }
serial_log=
output=/dev/stdout
while [[ $# -gt 0 ]]; do
  case "$1" in
    --serial-log) serial_log=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 2 ;;
  esac
done
[[ -f "$serial_log" ]] || { echo "serial log not found: $serial_log" >&2; exit 1; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kernel-testbed.env
source "$script_dir/kernel-testbed.env"

SERIAL_LOG="$serial_log" OUTPUT="$output" EXPECTED_VERSION="$KERNEL_VERSION" python3 <<'PY'
import json
import os
from pathlib import Path
import re

text = Path(os.environ["SERIAL_LOG"]).read_text(encoding="utf-8", errors="replace")
versions = re.findall(r"^WCP_KERNEL_RELEASE=(.+)$", text, re.MULTILINE)
records = re.findall(r"^WCP_KERNEL_VULNERABILITY=([^|]+)\|(.*)$", text, re.MULTILINE)
statuses = {name.strip(): status.strip().replace("\r", "") for name, status in records}
required = {"spectre_v1", "spectre_v2", "spec_store_bypass", "meltdown"}
failures = []
if not versions or not versions[-1].startswith(os.environ["EXPECTED_VERSION"]):
    failures.append("kernel-version-mismatch")
for name in sorted(required - statuses.keys()):
    failures.append(f"missing-vulnerability-status:{name}")
for name, status in sorted(statuses.items()):
    lowered = status.lower()
    if "vulnerable" in lowered or lowered.startswith("unknown") or not status:
        failures.append(f"unsafe-vulnerability-status:{name}:{status}")
document = {
    "schema_version": 1,
    "status": "pass" if not failures else "fail",
    "kernel_release": versions[-1].strip().replace("\r", "") if versions else None,
    "vulnerabilities": statuses,
    "failures": failures,
    "scope": "guest runtime on the CPU/host class represented by this serial log",
}
payload = json.dumps(document, indent=2, sort_keys=True) + "\n"
if os.environ["OUTPUT"] == "/dev/stdout":
    print(payload, end="")
else:
    Path(os.environ["OUTPUT"]).write_text(payload, encoding="utf-8")
if failures:
    raise SystemExit("guest runtime kernel audit failed: " + ", ".join(failures))
PY
