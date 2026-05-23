# http-hello-component

Minimal `wasi:http` sample component for the platform.

It exports `wasi:http/incoming-handler` and is meant to validate the runtime's
`wasi:http` hosting path, not to demonstrate application structure.

Build in WSL:

```bash
cargo build --manifest-path apps/http-hello-component/Cargo.toml --target wasm32-wasip2 --release
```

Expected routes:

- `/` -> `Hello from wasi:http!`
- `/health` -> `{"status":"healthy"}`
