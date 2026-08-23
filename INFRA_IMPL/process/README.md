# Local microVM application rehearsal

This directory records the repeatable process for provisioning the local Firecracker testbed, deploying an application and its service VMs, validating it through a browser-facing load balancer, and destroying only the recorded environment.

Use the [platform production deployment checklist](./PLATFORM_PRODUCTION_DEPLOYMENT_CHECKLIST.md) to qualify and launch the platform infrastructure. Then use the reusable [application deployment readiness checklist](./APPLICATION_DEPLOYMENT_READINESS_CHECKLIST.md) for each workload. The worked example is [OpenID-Connect-WASI-Hub](./OPENID_CONNECT_WASI_HUB_MICROVM_REHEARSAL.md). It uses three Wasm Cloud Platform nodes, one NATS microVM, one PostgreSQL microVM, and a host HAProxy front door.

Use the repository skills in this order:

1. `$provision-microvm-testbed`
2. `$deploy-test-application`
3. `$destroy-microvm-testbed` only after the user explicitly requests teardown

All commands must run in Linux or WSL2. Keep the same state-file path for every phase. The state file is the lifecycle authority: do not discover or remove VMs, TAP devices, bridges, or processes by broad patterns.

The production-like preset is a local production-shaped rehearsal, not a production deployment. Production still requires TLS termination, externally managed secrets, monitoring and alerting, backup/restore, a highly available NATS deployment, and an application replica strategy.
