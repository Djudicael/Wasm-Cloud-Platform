#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "usage: $0 <release.tar.gz> <extracted-artifact-dir> <owner/repo> <source-git-sha> <source-ref>" >&2
  exit 1
fi

archive="$1"
artifact_dir="$2"
repository="$3"
source_sha="$4"
source_ref="$5"
workflow="$repository/.github/workflows/release.yml"

command -v gh >/dev/null || { echo "GitHub CLI (gh) is required" >&2; exit 1; }
[[ "$repository" =~ ^[^/]+/[^/]+$ ]] || { echo "repository must be owner/name" >&2; exit 1; }
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || { echo "source SHA must be 40 lowercase hexadecimal characters" >&2; exit 1; }

bash "$(dirname "$0")/verify-release-bundle.sh" "$archive" "$source_sha"
gh attestation verify "$archive" --repo "$repository" --signer-workflow "$workflow" \
  --source-digest "$source_sha" --source-ref "$source_ref"
while IFS= read -r subject; do
  path="$artifact_dir/$subject"
  gh attestation verify "$path" --repo "$repository" --signer-workflow "$workflow" \
    --source-digest "$source_sha" --source-ref "$source_ref"
  gh attestation verify "$path" --repo "$repository" --signer-workflow "$workflow" \
    --source-digest "$source_sha" --source-ref "$source_ref" \
    --predicate-type https://spdx.dev/Document/v2.3
done < <(awk '{print $2}' "$artifact_dir/SHA256SUMS")

echo "release provenance and SPDX attestations verified"
