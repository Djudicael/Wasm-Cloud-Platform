#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <artifact-dir> <git-sha> <release-ref> <candidate|promotion> <source-date-epoch> <output-json>" >&2
  exit 1
fi

artifact_dir="$1"
git_sha="$2"
release_ref="$3"
promotion_mode="$4"
source_date_epoch="$5"
output_json="$6"

[[ -d "$artifact_dir" ]] || { echo "artifact directory not found: $artifact_dir" >&2; exit 1; }
[[ "$git_sha" =~ ^[0-9a-f]{40}$ ]] || { echo "git SHA must be 40 lowercase hexadecimal characters" >&2; exit 1; }
[[ "$promotion_mode" == candidate || "$promotion_mode" == promotion ]] || {
  echo "promotion mode must be candidate or promotion" >&2
  exit 1
}
[[ "$source_date_epoch" =~ ^[0-9]+$ ]] || { echo "SOURCE_DATE_EPOCH must be a non-negative integer" >&2; exit 1; }
if [[ "$promotion_mode" == promotion && ! "$release_ref" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]; then
  echo "promotable release refs must be semantic version tags such as v1.2.3" >&2
  exit 1
fi

lock_hash="$(sha256sum Cargo.lock | awk '{print $1}')"
toolchain_hash="$(sha256sum rust-toolchain.toml | awk '{print $1}')"
audit_config_hash="$(sha256sum .cargo/audit.toml | awk '{print $1}')"
rust_version="$(rustc --version | tr -d '\n')"
cargo_version="$(cargo --version | tr -d '\n')"

ARTIFACT_DIR="$artifact_dir" GIT_SHA="$git_sha" RELEASE_REF="$release_ref" \
PROMOTION_MODE="$promotion_mode" SOURCE_EPOCH="$source_date_epoch" \
LOCK_HASH="$lock_hash" TOOLCHAIN_HASH="$toolchain_hash" \
AUDIT_CONFIG_HASH="$audit_config_hash" \
RUST_VERSION="$rust_version" CARGO_VERSION="$cargo_version" \
python3 - "$output_json" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import sys

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
output = Path(sys.argv[1])
audit = json.loads((artifact_dir / "security-audit.json").read_text(encoding="utf-8"))
ignored_advisories = sorted(set(audit.get("settings", {}).get("ignore", [])))
artifacts = []
for path in sorted(artifact_dir.rglob("*")):
    if not path.is_file() or path.resolve() == output.resolve():
        continue
    if path.is_symlink():
        raise SystemExit(f"release artifacts must not contain symlinks: {path}")
    data = path.read_bytes()
    artifacts.append({"path": path.relative_to(artifact_dir).as_posix(),
                      "sha256": hashlib.sha256(data).hexdigest(),
                      "size_bytes": len(data)})

document = {
    "schema_version": 3,
    "git_sha": os.environ["GIT_SHA"],
    "release_ref": os.environ["RELEASE_REF"],
    "promotion_mode": os.environ["PROMOTION_MODE"],
    "promotable": os.environ["PROMOTION_MODE"] == "promotion",
    "source_tree_clean": True,
    "source_date_epoch": int(os.environ["SOURCE_EPOCH"]),
    "cargo_lock_sha256": os.environ["LOCK_HASH"],
    "rust_toolchain_sha256": os.environ["TOOLCHAIN_HASH"],
    "rust_version": os.environ["RUST_VERSION"],
    "cargo_version": os.environ["CARGO_VERSION"],
    "sbom": {"path": "sbom.spdx.json", "format": "SPDX", "spec_version": "2.3"},
    "security_audit": {
        "path": "security-audit.json",
        "policy": "cargo audit --json --deny warnings using .cargo/audit.toml",
        "configuration_path": ".cargo/audit.toml",
        "configuration_sha256": os.environ["AUDIT_CONFIG_HASH"],
        "ignored_advisories": ignored_advisories,
    },
    "attestation": {
        "issuer": "GitHub Actions OIDC / Sigstore",
        "provenance_predicate": "https://slsa.dev/provenance/v1",
        "verification_policy": "scripts/verify-release-bundle.sh plus gh attestation verify",
    },
    "artifacts": artifacts,
}
output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
