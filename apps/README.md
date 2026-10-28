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
- `http-hello-component`
  - minimal `wasi:http` component for `wasm32-wasip2`
  - validates the platform runtime's `wasi:http/incoming-handler` hosting path
  - `/probe/http/get` uses outgoing-handler to GET the operator-supplied `RESOURCE_PROBE_HTTP_TARGET`; without that variable it returns `http_probe_not_configured`
  - `/_platform/triggers/<id>` validates authoritative native trigger headers; `TRIGGER_PROBE_DELAY_MS` optionally delays this test handler, and `/probe/trigger-identity` verifies that ordinary HTTP sees no caller-supplied trigger ID
  - its outbound-policy regression is described in the [runtime guide](../crates/runtime/README.md#outbound-http-transport)
  - intended as the first real guest for future platform HTTP-component and gRPC-hosting work

See also:

- `http-p3-probe` is a disposable asynchronous WASI P3 HTTP/database transport
  fixture. It supports `/health`, `/probe/http`, `/probe/tcp`, `/probe/postgres`
  and `/probe/trigger-identity`; targets come only from operator environment.
  Build with `cargo build --target wasm32-wasip2 -p http-p3-probe`, then run
  `cargo test -p runtime --test p3_service -- --ignored --skip p3_postgres` in WSL.

- [`docs/grpc-compatibility.md`](../docs/grpc-compatibility.md)
