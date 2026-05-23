# Example Apps

These sample apps are used as platform fixtures first and reference implementations second.

- `hello-axum`
  - production-like enough for HTTP serving, routing, bind-address handling, and east-west request tests
  - honors `PORT`, `BIND_ADDR`, and `HOST` in both native and Wasm execution
- `echo-service`
  - production-like enough for simple service-to-service and route/gateway tests
  - honors `PORT`, `BIND_ADDR`, and `HOST` in both native and Wasm execution
- `postgres-app`
  - PostgreSQL example built on `wasi-pg-client` for both native and Wasm execution
  - exposes the same `/`, `/health`, and `/query` surface in both targets
  - still intentionally small; it demonstrates the connection path, not a full application data model
- `grpc-echo`
  - native gRPC reference app based on `tonic`
  - validates that the Rust gRPC stack itself works in the repo
  - intentionally does **not** claim `wasm32-wasip2` support today; the current `tonic` transport server path depends on Tokio networking features that are not available on WASI yet

See also:

- [`docs/grpc-compatibility.md`](../docs/grpc-compatibility.md)
