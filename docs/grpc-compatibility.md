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

The runtime currently instantiates components and looks for CLI-style entry points such as:

- `wasi:cli/run@0.2.x#run`
- top-level `run`
- top-level `_start`

It does **not** currently host `wasi:http` incoming handler exports as the app-serving model.

That means:

- even if a WASI gRPC server component compiles,
- the platform still needs a runtime/hosting path for `wasi:http`
- before platform-level gRPC can be claimed

## Current Status

### Supported

- native Rust gRPC development and testing in the repository

### Not Supported Yet

- deploying a gRPC server app to the platform and claiming end-to-end gRPC compatibility

## What Would Need To Change

To support gRPC seriously, one of these needs to happen:

1. add platform support for `wasi:http` incoming-handler style components
2. find a real `wasm32-wasip2` gRPC server stack that does not depend on the unsupported Tokio transport server path

Right now, the more realistic path is `wasi:http` component hosting.
