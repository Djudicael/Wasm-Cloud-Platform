# Performance Benchmarks

The repository includes two different measurement paths:

1. `scripts/run_wasmtime_load_review.sh`
   - runtime-focused
   - measures compile, prepare, and instantiation behavior inside the Wasmtime path

2. `scripts/run_platform_cold_start_benchmark.sh`
   - platform-focused
   - measures the real `node + proxy` cold path

## Platform Cold Start Benchmark

Run this in the environment you want to measure:

```bash
bash scripts/run_platform_cold_start_benchmark.sh
```

Optional knobs:

```bash
COLD_START_BENCH_ITERATIONS=10 \
COLD_START_BENCH_FOLLOW_UP_REQUESTS=1 \
bash scripts/run_platform_cold_start_benchmark.sh target/cold-start-benchmark
```

Default shape:

- `10` independent cold-start iterations
- `1` immediate follow-up request per iteration

That is intentional. Cold-start estimation should come from repeated independent starts, not from many requests against a single already-warm instance.

Outputs:

- `target/cold-start-benchmark/platform_cold_start.json`
- `target/cold-start-benchmark/platform_cold_start.md`

What it measures:

- deploy event publish
- artifact upload and authorization on the node artifact server
- compile / store / spawn
- route readiness
- first successful HTTP response through the proxy
- immediate follow-up request latency after the app is already serving

The report separates:

- `initial deploy-and-first-hit`
- `on-demand spawn`
- `ready-to-first-success tail`
- immediate follow-up request latency

What it does not prove:

- a universal cold-start number across all workloads
- a network-distributed production deployment path
- remote artifact fetch latency from another node or external artifact source
- sustained warm-traffic behavior for a long-lived service

Use this benchmark for claims about the platform path. Use the Wasmtime load review for claims about runtime internals.
