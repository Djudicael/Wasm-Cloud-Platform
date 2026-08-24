#!/usr/bin/env bash

set -euo pipefail

NIGHTLY="nightly-2026-08-20"
BPF_LINKER_VERSION="0.11.0"
BPF_LINKER_SHA256="10f62ba9ab7e544d538370552660efcb4f1a19153d5752bbf0f6b51f3bada450"
EBPF_BIN_DIR="${EBPF_BIN_DIR:-${HOME}/.local/bin}"

[[ "$(uname -s)-$(uname -m)" == "Linux-x86_64" ]] || {
    echo "This installer currently supports Linux x86_64 only." >&2
    exit 1
}
command -v curl >/dev/null || { echo "curl is required." >&2; exit 1; }
command -v zstd >/dev/null || {
    echo "zstd is required (Ubuntu/WSL: sudo apt-get install zstd)." >&2
    exit 1
}

rustup toolchain install "$NIGHTLY" --profile minimal --component rust-src --component rustfmt

download_dir="$(mktemp -d)"
cleanup() { rm -rf -- "$download_dir"; }
trap cleanup EXIT
archive="$download_dir/bpf-linker.tar.zst"
curl -fL --retry 3 -o "$archive" \
    "https://github.com/aya-rs/bpf-linker/releases/download/v${BPF_LINKER_VERSION}/bpf-linker-x86_64-unknown-linux-musl.tar.zst"
echo "${BPF_LINKER_SHA256}  ${archive}" | sha256sum -c -
mkdir -p "$EBPF_BIN_DIR"
tar -xpf "$archive" -C "$EBPF_BIN_DIR"

echo "Installed $NIGHTLY and bpf-linker $BPF_LINKER_VERSION in $EBPF_BIN_DIR."
