#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=kernel-testbed.env
source "$script_dir/kernel-testbed.env"

releases_url=https://www.kernel.org/releases.json
latest=$(curl --fail --location --silent --show-error "$releases_url" | \
  KERNEL_SERIES_VALUE="$KERNEL_SERIES" python3 -c '
import json, os, sys
series = os.environ["KERNEL_SERIES_VALUE"] + "."
for release in json.load(sys.stdin)["releases"]:
    if release.get("moniker") == "longterm" and release["version"].startswith(series):
        print(release["version"])
        break
else:
    raise SystemExit("pinned LTS series is absent from kernel.org releases")
')

if [[ "$latest" != "$KERNEL_VERSION" ]]; then
  echo "pinned guest kernel $KERNEL_VERSION is not current in LTS series $KERNEL_SERIES (latest: $latest)" >&2
  echo "update kernel-testbed.env and rerun the microVM validation evidence" >&2
  exit 1
fi
echo "guest kernel patch baseline is current: $KERNEL_VERSION"
