---
name: deploy-test-application
description: Build, deploy, and verify a Wasm application in an existing local Wasm Cloud Platform microVM testbed. Use for local end-to-end validation after provisioning, including artifact upload, NATS deployment, route creation, and HTTP response checks. Do not target production clusters.
---

# Deploy and verify a test application

1. Confirm the testbed state file exists, then run the VM CLI `status` command. Stop if any node is dead or unhealthy.
   For a production-shaped rehearsal, use `INFRA_IMPL/process/APPLICATION_DEPLOYMENT_READINESS_CHECKLIST.md` to record the application contract, required services, runtime limits, migration/rollback plan, and evidence. The checklist contains production operator gates; this skill executes only the local subset.
2. Use `bash scripts/vm/deploy-test-application.sh` from the repository. It defaults to building and deploying `apps/hello-axum`.
3. For another component, provide both `--app NAME` and either `--manifest PATH` or `--wasm PATH`. Use `--route-host` to make the HTTP verification deterministic. Repeat `--route-path` for multiple prefix routes, repeat `--env KEY=VALUE` for configuration, use `--health-path none` for a component without a health endpoint, and select a meaningful `--verify-path`.
4. Treat deployment as successful only when the script receives a successful HTTP response using the configured Host header and proves application identity. Pass `--verify-contains TEXT` with content unique to the expected workload whenever a shared front door has a default or fallback backend; a 2xx response alone can belong to the wrong application. When the provisioning state has an HAProxy companion service, the script verifies through that front door; otherwise it verifies through the first node proxy. Use `--verify-direct-node` for internal canaries that are intentionally absent from an application-specific front-door configuration. Preserve the printed response and endpoint as test evidence.
5. If verification times out, inspect the testbed status, node logs, NATS connectivity, artifact upload, and route propagation. Do not repeatedly redeploy without diagnosing the first failure.

The shared default state path is `.vm-testbed-state.json`. Override it consistently across provision, deploy, and destroy commands when concurrent testbeds are intentionally used.

For OpenID-Connect-WASI-Hub, use `bash scripts/vm/deploy-oidc-hub-test.sh --state-file PATH --app-dir PATH`. It builds the locked frontend and both `wasm32-wasip2` components, applies migrations, seeds local test data, deploys the two WASI apps under separate internal hosts, assigns the backend enough Wasm fuel for Argon2 password verification, and configures the recorded HAProxy with same-origin routing plus separate application-aware health pools. Treat the printed browser URL as ready only after frontend, database readiness, discovery issuer, SPA-route, and seeded-realm login checks all pass. Do not assume every platform proxy serves every scheduled application; use application-level health checks when placing a load balancer in front of multiple nodes.
