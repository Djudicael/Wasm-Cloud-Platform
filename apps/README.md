# Example Apps

These sample apps are used as platform fixtures first and reference implementations second.

- `hello-axum`
  - production-like enough for HTTP serving, routing, bind-address handling, and east-west request tests
  - honors `PORT`, `BIND_ADDR`, and `HOST` in both native and Wasm execution
- `echo-service`
  - production-like enough for simple service-to-service and route/gateway tests
  - honors `PORT`, `BIND_ADDR`, and `HOST` in both native and Wasm execution
- `postgres-app`
  - smoke-test-oriented example for outbound TCP and minimal PostgreSQL wire-protocol behavior
  - now exposes the same `/`, `/health`, and `/query` surface in both native and Wasm execution
  - still intentionally minimal; it is not a full SQL client example
