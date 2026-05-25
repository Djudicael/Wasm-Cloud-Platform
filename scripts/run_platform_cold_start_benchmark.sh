#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT_DIR="${1:-target/cold-start-benchmark}"
ABS_OUT_DIR="$ROOT/$OUT_DIR"
ITERATIONS="${COLD_START_BENCH_ITERATIONS:-10}"
FOLLOW_UP_REQUESTS="${COLD_START_BENCH_FOLLOW_UP_REQUESTS:-1}"

mkdir -p "$ABS_OUT_DIR"

echo "Building benchmark prerequisites..."
cargo build --quiet --release -p node
cargo build --quiet --release --target wasm32-wasip2 -p hello-axum

echo "Running platform cold-start benchmark..."
COLD_START_BENCH_OUTPUT="$ABS_OUT_DIR" \
COLD_START_BENCH_ITERATIONS="$ITERATIONS" \
COLD_START_BENCH_FOLLOW_UP_REQUESTS="$FOLLOW_UP_REQUESTS" \
cargo test --quiet -p e2e --test cold_start_benchmark benchmark_platform_cold_start -- --ignored --nocapture --test-threads=1

echo
echo "Benchmark outputs:"
echo "  $ABS_OUT_DIR/platform_cold_start.json"
echo "  $ABS_OUT_DIR/platform_cold_start.md"
cat "$ABS_OUT_DIR/platform_cold_start.md"
