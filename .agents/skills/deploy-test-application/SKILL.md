---
name: deploy-test-application
description: Build, deploy, and verify a Wasm application in an existing local Wasm Cloud Platform microVM testbed. Use for local end-to-end validation after provisioning, including artifact upload, NATS deployment, route creation, and HTTP response checks. Do not target production clusters.
---

# Deploy and verify a test application

1. Confirm the testbed state file exists, then run the VM CLI `status` command. Stop if any node is dead or unhealthy.
2. Use `bash scripts/vm/deploy-test-application.sh` from the repository. It defaults to building and deploying `apps/hello-axum`.
3. For another component, provide both `--app NAME` and either `--manifest PATH` or `--wasm PATH`. Use `--route-host` to make the HTTP verification deterministic.
4. Treat deployment as successful only when the script receives a successful HTTP response using the configured Host header. When the provisioning state has an HAProxy companion service, the script verifies through that front door; otherwise it verifies through the first node proxy. Preserve the printed response and endpoint as test evidence.
5. If verification times out, inspect the testbed status, node logs, NATS connectivity, artifact upload, and route propagation. Do not repeatedly redeploy without diagnosing the first failure.

The shared default state path is `.vm-testbed-state.json`. Override it consistently across provision, deploy, and destroy commands when concurrent testbeds are intentionally used.
