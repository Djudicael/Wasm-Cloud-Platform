# gRPC Compatibility

This document records the current state of gRPC support in the Wasm Cloud Platform.

## Short Version

- native Rust gRPC is fine
- gRPC through the platform works for the current `wasi:http` component path
- validated in WSL for:
  - unary
  - server streaming
  - client streaming
  - bidirectional streaming
  - trailer/error propagation

## What Was Tested

### 1. Native `tonic` server in this repo

The sample app [`apps/grpc-echo`](../apps/grpc-echo/README.md) was added and tested natively.

Validated in WSL:

```bash
cargo test -p grpc-echo test_native_grpc_roundtrip -- --nocapture
```

Result:

- **works**

### 2. `tonic::transport::Server` on `wasm32-wasip2`

Validated in WSL:

```bash
cargo build -p grpc-echo --target wasm32-wasip2
```

Result:

- **does not work**
- reason: the standard Tokio networking / HTTP2 server stack used by `tonic` is not available in the required form on `wasm32-wasip2`

The sample app now fails with an explicit compile-time message instead of an ambiguous dependency failure.

### 3. `fermyon/wasi-grpc-server-rust`

This was evaluated separately because it uses a different model: gRPC over `wasi:http`, not a direct TCP listener.

Validated in WSL:

```bash
cd /tmp/wasi-grpc-server-rust/examples/route-guide-server
cargo build --target wasm32-wasip1 --release
cargo build --target wasm32-wasip2 --release
```

Result:

- `wasm32-wasip1`: **builds**
- `wasm32-wasip2`: **fails**

The current maintained example is a `Spin` / `wasi:http` component path built around `wasm32-wasip1`, not the `wasm32-wasip2` CLI-style component path this platform currently runs.

## What This Means For This Platform

The runtime now supports two hosting shapes:

- CLI-style components:
  - `wasi:cli/run@0.2.x#run`
  - top-level `run`
  - top-level `_start`
- `wasi:http/incoming-handler@0.2.x#handle` components via a local adapter server bound on the allocated instance port

That hosting path is now sufficient for platform-level gRPC on the tested unary and streaming shapes.

## Current Status

### Supported

- native Rust gRPC development and testing in the repository
- hosting `wasi:http` incoming-handler components on the runtime
- unary gRPC requests through the full platform path
- server-streaming gRPC requests through the full platform path
- client-streaming gRPC requests through the full platform path
- bidirectional-streaming gRPC requests through the full platform path
- trailer-accurate failure propagation through the full platform path

## Streaming Validation Status

The repository now contains focused ignored E2E tests in
[`crates/e2e/tests/wasi_grpc_component.rs`](../crates/e2e/tests/wasi_grpc_component.rs)
for the different RPC shapes.

Validated in WSL:

```bash
cargo test -p e2e --test wasi_grpc_component test_wasi_grpc_unary_and_server_streaming -- --ignored --nocapture
cargo test -p e2e --test wasi_grpc_component test_wasi_grpc_client_streaming -- --ignored --nocapture
cargo test -p e2e --test wasi_grpc_component test_wasi_grpc_bidi_streaming -- --ignored --nocapture
cargo test -p e2e --test wasi_grpc_component test_wasi_grpc_failure_trailers -- --ignored --nocapture
```

Current observed state:

- unary: passes
- server streaming: passes
- client streaming: passes
- bidi streaming: passes
- failure trailers: pass with the expected tonic `Code`

The critical runtime changes that closed the gap were:

1. keep the `wasi:http` guest task alive until the response body is fully drained
2. run guest request handling on the blocking path that matches the WASI blocking stream API
3. preserve an end-to-end HTTP/2/h2c path for `wasi:http` upstreams
4. preserve gRPC trailers through the bridge instead of collapsing them into `Unknown`

## Current Boundary

The platform can now claim tested gRPC support for unary and streaming RPCs on the `wasi:http` component path.

The boundary that still matters is app model:

- supported gRPC app model: `wasi:http` incoming-handler components
- unsupported gRPC app model: direct `tonic::transport::Server` on `wasm32-wasip2`
