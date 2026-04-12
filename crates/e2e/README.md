# E2E Tests

End-to-end tests for the Wasm Cloud Platform. These tests run the full stack:
- Real `wasm-node` binary
- Real NATS server (local or CI)
- Real redb database (temp files)
- Real Wasm binaries (hello-axum.wasm)

## Running Tests

```bash
# Start a local NATS server first
docker run -d --name nats-test -p 4222:4222 nats:latest

# Build the test Wasm app
cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release

# Run all E2E tests
cargo test -p e2e

# Stop NATS
docker stop nats-test && docker rm nats-test
```

## Test Philosophy

These tests use **real infrastructure**, not mocks:
- Real redb database via tempfile
- Real NATS via local server or testcontainers
- Real Wasmtime runtime
- Real compiled Wasm binaries

This ensures tests catch actual bugs that mocks would hide.
