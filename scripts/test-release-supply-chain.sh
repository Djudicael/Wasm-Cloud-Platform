#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
work_dir="$(mktemp -d)"
cleanup() { rm -rf -- "$work_dir"; }
trap cleanup EXIT

fixture="$work_dir/release-artifacts"
mkdir -p "$fixture/ebpf"
for name in wasm-node wasm-ctl wasm-deploy-ingress; do
  install -m 0755 /bin/true "$fixture/$name"
done
printf '\0asm-release-fixture' > "$fixture/hello-axum.wasm"
for name in process_tracker tcp_monitor fd_watcher mem_pressure disk_monitor syscall_counter namespace_enforcer; do
  printf 'bpf-fixture-%s\n' "$name" > "$fixture/ebpf/$name"
done
cat > "$fixture/sbom.spdx.json" <<'JSON'
{
  "spdxVersion": "SPDX-2.3",
  "SPDXID": "SPDXRef-DOCUMENT",
  "name": "wasm-cloud-platform-release-test",
  "dataLicense": "CC0-1.0",
  "documentNamespace": "https://example.invalid/spdx/release-test",
  "creationInfo": {"created": "1970-01-01T00:00:00Z", "creators": ["Tool: test-release-supply-chain"]},
  "packages": [{"name": "wasm-cloud-platform", "SPDXID": "SPDXRef-Package", "downloadLocation": "NOASSERTION", "filesAnalyzed": false}]
}
JSON
cat > "$fixture/security-audit.json" <<'JSON'
{"database":{"advisory-count":1},"lockfile":{"dependency-count":1},"settings":{"ignore":["RUSTSEC-2025-0069"]},"vulnerabilities":{"count":0,"list":[]},"warnings":{}}
JSON

git_sha="0123456789abcdef0123456789abcdef01234567"
bash scripts/finalize-release-bundle.sh "$fixture" "$git_sha" candidate-test candidate 0 "$work_dir/first.tar.gz"
bash scripts/verify-release-bundle.sh "$work_dir/first.tar.gz" "$git_sha" candidate-test candidate
bash scripts/finalize-release-bundle.sh "$fixture" "$git_sha" candidate-test candidate 0 "$work_dir/second.tar.gz"
cmp "$work_dir/first.tar.gz" "$work_dir/second.tar.gz"

printf 'tampered\n' >> "$fixture/wasm-node"
if bash scripts/verify-release-bundle.sh "$fixture" "$git_sha" candidate-test candidate >/dev/null 2>&1; then
  echo "tampered artifact was accepted" >&2
  exit 1
fi

if bash scripts/create_release_manifest.sh "$fixture" "$git_sha" not-a-version promotion 0 "$work_dir/invalid.json" >/dev/null 2>&1; then
  echo "non-semantic promotion ref was accepted" >&2
  exit 1
fi

echo "release supply-chain tests passed"
