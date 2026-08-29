#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <artifact-dir> <git-sha> <release-ref> <candidate|promotion> <source-date-epoch> <output-tar.gz>" >&2
  exit 1
fi

artifact_dir="$1"
git_sha="$2"
release_ref="$3"
promotion_mode="$4"
source_date_epoch="$5"
output_archive="$6"

[[ -f "$artifact_dir/sbom.spdx.json" ]] || { echo "missing SPDX SBOM" >&2; exit 1; }
(
  cd "$artifact_dir"
  find . -type f ! -name SHA256SUMS ! -name RELEASE-MANIFEST.json -printf '%P\0' \
    | LC_ALL=C sort -z | xargs -0 sha256sum > SHA256SUMS
)
bash scripts/create_release_manifest.sh \
  "$artifact_dir" "$git_sha" "$release_ref" "$promotion_mode" "$source_date_epoch" \
  "$artifact_dir/RELEASE-MANIFEST.json"
bash scripts/verify-release-bundle.sh "$artifact_dir" "$git_sha" "$release_ref" "$promotion_mode"
tar --sort=name --mtime="@${source_date_epoch}" --owner=0 --group=0 --numeric-owner \
  -cf - -C "$artifact_dir" . | gzip -n -9 > "$output_archive"
