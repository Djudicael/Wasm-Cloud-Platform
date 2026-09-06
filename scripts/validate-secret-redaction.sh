#!/usr/bin/env bash
# Fail when a known sentinel secret appears in captured logs or reports.

set -euo pipefail

if (($# == 0)); then
  echo "Usage: WASM_SECRET_REDACTION_SENTINEL=<value> $0 LOG_OR_REPORT [...]" >&2
  exit 2
fi

sentinel=${WASM_SECRET_REDACTION_SENTINEL:-}
if [[ ${#sentinel} -lt 16 ]]; then
  echo "WASM_SECRET_REDACTION_SENTINEL must contain at least 16 characters." >&2
  exit 2
fi

pattern_file=$(mktemp)
chmod 600 "$pattern_file"
trap 'rm -f -- "$pattern_file"' EXIT
printf '%s' "$sentinel" >"$pattern_file"

for artifact in "$@"; do
  [[ -f "$artifact" ]] || {
    echo "Missing log/redaction artifact: $artifact" >&2
    exit 2
  }
  if grep -Fq -f "$pattern_file" -- "$artifact"; then
    echo "Secret-redaction validation failed for: $artifact" >&2
    exit 1
  fi
done

echo "Secret-redaction validation passed for $# artifact(s)."
