# Node End-to-End Tests

This directory contains end-to-end integration tests for the Wasm Cloud Platform.

## Tests Overview

### `e2e_smoke.rs`
Full platform smoke test that verifies the complete deployment pipeline:
- Builds hello-axum WASM app
- Starts NATS with JetStream
- Deploys app via event bus
- Verifies HTTP requests work through the proxy

### `e2e_graceful_shutdown.rs`
Graceful shutdown tests that demonstrate the shutdown mechanisms:

**Test 1: `test_graceful_shutdown_e2e`**
- Full platform test with graceful shutdown
- Deploys hello-axum with shutdown endpoint
- Sends requests to verify instance is running
- Triggers graceful shutdown via `POST /_platform/shutdown`
- Verifies instance exits cleanly

**Test 2: `test_shutdown_endpoint_direct`**
- Simpler test using wasmtime directly
- Runs hello-axum WASM app standalone
- Tests the `/_platform/shutdown` endpoint
- Verifies instance stops gracefully

## Prerequisites

### For all tests:
```bash
# Ensure you have the WASM target
rustup target add wasm32-wasip2
```

### For `e2e_smoke` and `test_graceful_shutdown_e2e`:
```bash
# Install podman or docker (for testcontainers)
# On Ubuntu/WSL:
sudo apt install podman

# Or Docker:
curl -fsSL https://get.docker.com | sh
```

### For `test_shutdown_endpoint_direct`:
```bash
# Install wasmtime
curl https://wasmtime.dev/install.sh -sSf | bash
# Or: cargo install wasmtime-cli
```

## Running the Tests

### Run all end-to-end tests:
```bash
# In WSL (recommended)
cargo test -p node --test e2e_graceful_shutdown

# Or specific test:
cargo test -p node --test e2e_graceful_shutdown test_shutdown_endpoint_direct -- --nocapture
```

### Run with verbose output to see what's happening:
```bash
cargo test -p node --test e2e_graceful_shutdown test_graceful_shutdown_e2e -- --nocapture
```

## What the Graceful Shutdown Tests Demonstrate

The `e2e_graceful_shutdown.rs` tests show:

1. **Building WASM with graceful shutdown support**
   - Uses `RUSTFLAGS="--cfg tokio_unstable"`
   - Target: `wasm32-wasip2`
   - App includes `/_platform/shutdown` endpoint

2. **Graceful shutdown flow**
   - Platform sends `POST /_platform/shutdown` to instance
   - Instance receives shutdown signal via `tokio::sync::Notify`
   - Axum drains in-flight requests
   - Instance exits cleanly

3. **Zero-downtime behavior**
   - Requests complete successfully before shutdown
   - No connection resets or 5xx errors
   - Port is released after shutdown

## Expected Output

### Successful run shows:
```
Building hello-axum with graceful shutdown to wasm32-wasip2...
Wasm bytes loaded: 873472 bytes
Artifact server listening on 127.0.0.1:19091
Starting NATS with JetStream...
Publishing AppConfigured event...
Sending test requests to the proxy...
Request 1: status=200, body=Hello from Wasm!
Testing graceful shutdown via /_platform/shutdown endpoint...
Shutdown endpoint response: status=200, body=shutting down gracefully
Instance successfully shut down (connection refused)

✅ Graceful shutdown test completed successfully!
```

## Troubleshooting

### "Failed to start NATS container"
- Ensure podman/docker is running
- Check if port 4222 is available
- Try: `podman ps` or `docker ps`

### "Failed to build hello-axum"
- Run manually: `RUSTFLAGS="--cfg tokio_unstable" cargo build -p hello-axum --target wasm32-wasip2 --release`
- Check that `rustup target add wasm32-wasip2` was run

### "wasmtime: command not found"
- Install wasmtime: `curl https://wasmtime.dev/install.sh -sSf | bash`
- Or skip `test_shutdown_endpoint_direct` test

### Test hangs or times out
- Increase timeouts in the test code
- Check that ports aren't already in use
- Verify NATS is running with JetStream enabled

## Architecture Insights from Tests

These tests demonstrate:

1. **Event-Driven Deployment**
   - `AppConfigured` → stores config
   - `ArtifactUploaded` → downloads and compiles WASM
   - `DeployRequested` → spawns instances
   - `InstanceReady` → adds to upstream routing

2. **Graceful Shutdown Mechanisms**
   - Mechanism 1: TCP close (via dropping listener)
   - Mechanism 3: HTTP endpoint (via `/_platform/shutdown`)
   - Supervisor calls both in sequence for maximum reliability

3. **Resource Management**
   - Pre-bound TCP listeners passed to WASM
   - Port allocation and release
   - Clean task termination with stats collection

## Next Steps

After running these tests successfully, you understand:
- ✓ How to build WASM apps with graceful shutdown
- ✓ How the platform deploys and manages instances
- ✓ How graceful shutdown works end-to-end
- ✓ How to verify zero-downtime behavior

Try modifying the tests to:
- Add concurrent requests during shutdown
- Test multiple instances shutting down
- Measure actual drain timeout behavior
