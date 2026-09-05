#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 4 ]]; then
  echo "usage: $0 <artifact-directory|release.tar.gz> [expected-git-sha] [expected-release-ref] [candidate|promotion]" >&2
  exit 1
fi

input="$1"
expected_sha="${2:-}"
expected_ref="${3:-}"
expected_mode="${4:-}"
temp_dir=""
cleanup() { [[ -z "$temp_dir" ]] || rm -rf -- "$temp_dir"; }
trap cleanup EXIT

if [[ -d "$input" ]]; then
  artifact_dir="$input"
else
  [[ -f "$input" ]] || { echo "release bundle not found: $input" >&2; exit 1; }
  temp_dir="$(mktemp -d)"
  python3 - "$input" <<'PY'
import sys
import tarfile
from pathlib import PurePosixPath
with tarfile.open(sys.argv[1], "r:gz") as archive:
    seen = set()
    total_size = 0
    for member in archive.getmembers():
        path = PurePosixPath(member.name)
        if path.is_absolute() or ".." in path.parts or member.issym() or member.islnk() or not (member.isfile() or member.isdir()):
            raise SystemExit(f"unsafe release archive member: {member.name}")
        if member.name in seen:
            raise SystemExit(f"duplicate release archive member: {member.name}")
        seen.add(member.name)
        total_size += member.size
    if len(seen) > 1024 or total_size > 4 * 1024 * 1024 * 1024:
        raise SystemExit("release archive exceeds admission limits")
PY
  tar -xzf "$input" -C "$temp_dir"
  artifact_dir="$temp_dir"
fi

EXPECTED_SHA="$expected_sha" EXPECTED_REF="$expected_ref" EXPECTED_MODE="$expected_mode" \
python3 - "$artifact_dir" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import re
import sys

root = Path(sys.argv[1])
subjects = {
    "wasm-node", "wasm-ctl", "wasm-deploy-ingress", "hello-axum.wasm",
    "ebpf/process_tracker", "ebpf/tcp_monitor", "ebpf/fd_watcher",
    "ebpf/mem_pressure", "ebpf/disk_monitor", "ebpf/syscall_counter",
    "ebpf/namespace_enforcer",
    "sbom.spdx.json", "security-audit.json",
}
required = subjects | {"SHA256SUMS", "RELEASE-MANIFEST.json"}
actual = set()
for path in root.rglob("*"):
    if path.is_symlink():
        raise SystemExit(f"symlink is forbidden in release bundle: {path}")
    if path.is_file():
        actual.add(path.relative_to(root).as_posix())
if actual != required:
    raise SystemExit(f"release allowlist mismatch; missing={sorted(required-actual)} unexpected={sorted(actual-required)}")

checksums = {}
for line in (root / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
    match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
    if not match or match.group(2).startswith(("/", "../")) or "/../" in match.group(2):
        raise SystemExit(f"invalid checksum line: {line!r}")
    if match.group(2) in checksums:
        raise SystemExit(f"duplicate checksum subject: {match.group(2)}")
    checksums[match.group(2)] = match.group(1)
if set(checksums) != subjects:
    raise SystemExit("SHA256SUMS subject allowlist mismatch")
for name, digest in checksums.items():
    if hashlib.sha256((root / name).read_bytes()).hexdigest() != digest:
        raise SystemExit(f"checksum mismatch: {name}")

manifest = json.loads((root / "RELEASE-MANIFEST.json").read_text(encoding="utf-8"))
if manifest.get("schema_version") != 3 or manifest.get("source_tree_clean") is not True:
    raise SystemExit("manifest schema or clean-source assertion is invalid")
if not re.fullmatch(r"[0-9a-f]{40}", manifest.get("git_sha", "")):
    raise SystemExit("manifest git SHA is invalid")
if not isinstance(manifest.get("source_date_epoch"), int) or manifest["source_date_epoch"] < 0:
    raise SystemExit("manifest SOURCE_DATE_EPOCH is invalid")
for key in ("cargo_lock_sha256", "rust_toolchain_sha256"):
    if not re.fullmatch(r"[0-9a-f]{64}", manifest.get(key, "")):
        raise SystemExit(f"manifest {key} is invalid")
if manifest.get("promotion_mode") not in {"candidate", "promotion"}:
    raise SystemExit("manifest promotion mode is invalid")
if manifest.get("promotable") is not (manifest["promotion_mode"] == "promotion"):
    raise SystemExit("manifest promotable flag is inconsistent")
if manifest["promotion_mode"] == "promotion" and not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[+-][0-9A-Za-z.-]+)?", manifest.get("release_ref", "")):
    raise SystemExit("promotable bundle does not use a semantic-version tag")
for env_name, key in (("EXPECTED_SHA", "git_sha"), ("EXPECTED_REF", "release_ref"), ("EXPECTED_MODE", "promotion_mode")):
    expected = os.environ.get(env_name, "")
    if expected and manifest.get(key) != expected:
        raise SystemExit(f"manifest {key} mismatch: expected {expected}, got {manifest.get(key)}")

entries = {entry["path"]: entry for entry in manifest.get("artifacts", [])}
if len(entries) != len(manifest.get("artifacts", [])):
    raise SystemExit("manifest contains duplicate artifact paths")
if set(entries) != required - {"RELEASE-MANIFEST.json"}:
    raise SystemExit("manifest artifact allowlist mismatch")
for name, entry in entries.items():
    data = (root / name).read_bytes()
    if hashlib.sha256(data).hexdigest() != entry.get("sha256") or len(data) != entry.get("size_bytes"):
        raise SystemExit(f"manifest digest or size mismatch: {name}")

sbom = json.loads((root / "sbom.spdx.json").read_text(encoding="utf-8"))
if sbom.get("spdxVersion") != "SPDX-2.3" or not sbom.get("packages"):
    raise SystemExit("SBOM must be a non-empty SPDX 2.3 JSON document")
audit = json.loads((root / "security-audit.json").read_text(encoding="utf-8"))
if not isinstance(audit, dict) or "vulnerabilities" not in audit or "warnings" not in audit:
    raise SystemExit("cargo-audit evidence is malformed")
audit_policy = manifest.get("security_audit", {})
ignored = audit.get("settings", {}).get("ignore", [])
if not isinstance(ignored, list) or not all(re.fullmatch(r"RUSTSEC-[0-9]{4}-[0-9]{4}", item or "") for item in ignored):
    raise SystemExit("cargo-audit ignored-advisory evidence is malformed")
if audit_policy.get("ignored_advisories") != sorted(set(ignored)):
    raise SystemExit("manifest does not record the exact cargo-audit exception policy")
if audit_policy.get("configuration_path") != ".cargo/audit.toml" or not re.fullmatch(
    r"[0-9a-f]{64}", audit_policy.get("configuration_sha256", "")
):
    raise SystemExit("manifest cargo-audit configuration identity is missing or invalid")
print(f"verified release bundle: {manifest['release_ref']} ({manifest['promotion_mode']}, {manifest['git_sha']})")
PY
