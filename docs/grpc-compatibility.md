# gRPC Compatibility

This document records the current state of gRPC support in the Wasm Cloud Platform.

## Short Version

- native Rust gRPC is fine
- platform-level gRPC on `wasm32-wasip2` is **not** ready to claim today
- the blocker is not Rust gRPC in general, it is the mismatch between available WASI server models and the platform's current runtime model

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

That closes the earlier hosting-model gap, but it does **not** by itself mean platform-level gRPC is ready:

- the current adapter path serves requests through an HTTP/1.1 local bridge
- gRPC needs an end-to-end HTTP/2 path plus trailer semantics
- we still do not have a proven `wasm32-wasip2` gRPC server stack for this platform model

That means:

- even if a WASI gRPC server component compiles,
- the platform still needs a runtime/hosting path for `wasi:http`
- before platform-level gRPC can be claimed

## Current Status

### Supported

- native Rust gRPC development and testing in the repository
- hosting `wasi:http` incoming-handler components on the runtime

### Not Supported Yet

- deploying a gRPC server app to the platform and claiming end-to-end gRPC compatibility

## What Would Need To Change

To support gRPC seriously, one of these needs to happen:

1. find a real `wasm32-wasip2` gRPC server stack that fits the current `wasi:http` hosting path
2. add an HTTP/2-capable request bridge for `wasi:http` components, with correct gRPC status/trailer propagation

Right now, the runtime foundation is `wasi:http` component hosting, but the gRPC-specific transport path is still the missing piece.
