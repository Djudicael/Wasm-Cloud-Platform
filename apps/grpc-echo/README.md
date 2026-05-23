# grpc-echo

Minimal gRPC echo service used to validate the Rust gRPC stack in this repository.

## What It Proves

- native `tonic` gRPC works in the workspace
- unary roundtrip testing is straightforward

## What It Does Not Prove

- it does **not** prove platform-level gRPC compatibility
- it does **not** prove `wasm32-wasip2` server-side gRPC support

Today the crate intentionally fails on `wasm32-wasip2` with a clear compile error because the current `tonic` transport server path depends on Tokio networking and HTTP/2 server features that are not available on WASI yet.

## Validate

Native test:

```bash
cargo test -p grpc-echo test_native_grpc_roundtrip -- --nocapture
```

Expected WASI result today:

```bash
cargo build -p grpc-echo --target wasm32-wasip2
```

This should fail with the explicit compatibility message in `src/main.rs`.
