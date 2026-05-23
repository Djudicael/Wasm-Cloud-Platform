#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <artifact-dir> <git-sha> <output-json>" >&2
  exit 1
fi

artifact_dir="$1"
git_sha="$2"
output_json="$3"

lock_hash="$(sha256sum Cargo.lock | awk '{print $1}')"
rust_version="$(rustc --version | tr -d '\n')"
cargo_version="$(cargo --version | tr -d '\n')"

{
  printf '{\n'
  printf '  "schema_version": 1,\n'
  printf '  "git_sha": "%s",\n' "$git_sha"
  printf '  "cargo_lock_sha256": "%s",\n' "$lock_hash"
  printf '  "rust_version": "%s",\n' "$rust_version"
  printf '  "cargo_version": "%s",\n' "$cargo_version"
  printf '  "artifacts": [\n'

  first=1
  while IFS= read -r -d '' file; do
    rel_path="${file#${artifact_dir}/}"
    sha="$(sha256sum "$file" | awk '{print $1}')"
    size="$(stat -c %s "$file")"
    if [[ $first -eq 0 ]]; then
      printf ',\n'
    fi
    first=0
    printf '    {"path":"%s","sha256":"%s","size_bytes":%s}' "$rel_path" "$sha" "$size"
  done < <(find "$artifact_dir" -maxdepth 1 -type f ! -name 'RELEASE-MANIFEST.json' -print0 | sort -z)

  printf '\n  ]\n'
  printf '}\n'
} > "$output_json"
