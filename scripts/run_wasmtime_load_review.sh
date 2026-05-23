#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT_DIR="${1:-target/wasmtime-load-review}"
SEQUENTIAL_SPAWNS="${SEQUENTIAL_SPAWNS:-64}"
PEAK_LIVE_INSTANCES="${PEAK_LIVE_INSTANCES:-32}"
COMPONENT_PATH="${COMPONENT_PATH:-target/wasm32-wasip2/release/hello-axum.wasm}"

mkdir -p "$OUT_DIR"

echo "Building hello-axum component for runtime load review..."
cargo build --quiet --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release

scenarios=(baseline cache pooling cache-pooling)
for scenario in "${scenarios[@]}"; do
  echo "Running scenario: $scenario"
  cargo run --quiet -p runtime --example wasmtime_load_probe -- \
    --scenario "$scenario" \
    --component "$COMPONENT_PATH" \
    --sequential-spawns "$SEQUENTIAL_SPAWNS" \
    --peak-live-instances "$PEAK_LIVE_INSTANCES" \
    > "$OUT_DIR/$scenario.env"
done

value_from() {
  local file="$1"
  local key="$2"
  sed -n "s/^${key}=//p" "$file" | tail -n 1
}

baseline_avg="$(value_from "$OUT_DIR/baseline.env" sequential_spawn_avg_ms)"
pooling_avg="$(value_from "$OUT_DIR/pooling.env" sequential_spawn_avg_ms)"
baseline_peak_rss="$(value_from "$OUT_DIR/baseline.env" rss_after_peak_live_bytes)"
pooling_peak_rss="$(value_from "$OUT_DIR/pooling.env" rss_after_peak_live_bytes)"
cache_cold_compile_ms="$(value_from "$OUT_DIR/cache.env" cold_compile_ms)"
cache_warm_compile_ms="$(value_from "$OUT_DIR/cache.env" warm_compile_ms)"
cache_pooling_avg="$(value_from "$OUT_DIR/cache-pooling.env" sequential_spawn_avg_ms)"
cache_pooling_peak_rss="$(value_from "$OUT_DIR/cache-pooling.env" rss_after_peak_live_bytes)"

summary="$OUT_DIR/summary.md"
{
  echo "# Wasmtime Sustained Load Review"
  echo
  echo "- sequential spawns: $SEQUENTIAL_SPAWNS"
  echo "- peak live instances: $PEAK_LIVE_INSTANCES"
  echo "- component: \`$COMPONENT_PATH\`"
  echo
  echo "| Scenario | Cold Compile ms | Warm Compile ms | Avg Spawn ms | Peak-live Spawn ms | RSS After Peak Live bytes |"
  echo "|---|---:|---:|---:|---:|---:|"
  for scenario in "${scenarios[@]}"; do
    file="$OUT_DIR/$scenario.env"
    echo "| $scenario | $(value_from "$file" cold_compile_ms) | $(value_from "$file" warm_compile_ms) | $(value_from "$file" sequential_spawn_avg_ms) | $(value_from "$file" peak_live_spawn_ms) | $(value_from "$file" rss_after_peak_live_bytes) |"
  done
  echo
} > "$summary"

python3 - "$baseline_avg" "$pooling_avg" "$baseline_peak_rss" "$pooling_peak_rss" "$cache_cold_compile_ms" "$cache_warm_compile_ms" "$cache_pooling_avg" "$cache_pooling_peak_rss" "$summary" <<'PY'
import sys
baseline_avg = float(sys.argv[1])
pooling_avg = float(sys.argv[2])
baseline_peak_rss = int(sys.argv[3] or "0")
pooling_peak_rss = int(sys.argv[4] or "0")
cache_cold_compile_ms = float(sys.argv[5])
cache_warm_compile_ms = float(sys.argv[6] or "0")
cache_pooling_avg = float(sys.argv[7])
cache_pooling_peak_rss = int(sys.argv[8] or "0")
summary_path = sys.argv[9]

pooling_improvement = 0.0
if baseline_avg > 0:
    pooling_improvement = ((baseline_avg - pooling_avg) / baseline_avg) * 100.0

cache_compile_improvement = 0.0
if cache_cold_compile_ms > 0 and cache_warm_compile_ms > 0:
    cache_compile_improvement = ((cache_cold_compile_ms - cache_warm_compile_ms) / cache_cold_compile_ms) * 100.0

pooling_rss_multiplier = 0.0
if baseline_peak_rss > 0:
    pooling_rss_multiplier = pooling_peak_rss / baseline_peak_rss

cache_pooling_rss_multiplier = 0.0
if baseline_peak_rss > 0:
    cache_pooling_rss_multiplier = cache_pooling_peak_rss / baseline_peak_rss

recommendation = []
recommendation.append(f"- cache warm compile improvement: {cache_compile_improvement:.2f}%")
recommendation.append(f"- pooling sequential spawn improvement vs baseline: {pooling_improvement:.2f}%")
recommendation.append(f"- pooling peak-live RSS multiplier vs baseline: {pooling_rss_multiplier:.2f}x")
recommendation.append(f"- cache+pooling avg spawn ms: {cache_pooling_avg:.3f}")
recommendation.append(f"- cache+pooling peak-live RSS multiplier vs baseline: {cache_pooling_rss_multiplier:.2f}x")

if pooling_improvement >= 15.0 and pooling_rss_multiplier <= 1.5:
    recommendation.append("- recommendation: pooling allocator is acceptable for this workload shape if node memory sizing matches the measured RSS footprint.")
else:
    recommendation.append("- recommendation: keep pooling allocator disabled by default for this workload; cache-only remains the safer baseline unless a deployment-specific rerun shows a clear benefit.")

with open(summary_path, "a", encoding="utf-8") as f:
    f.write("## Recommendation\n\n")
    for line in recommendation:
        f.write(line + "\n")
PY

cat "$summary"
