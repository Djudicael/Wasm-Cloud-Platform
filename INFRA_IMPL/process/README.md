# Local microVM application rehearsal

This directory records the repeatable process for provisioning the local Firecracker testbed, deploying an application and its service VMs, validating it through a browser-facing load balancer, and destroying only the recorded environment.

Use the [platform production deployment checklist](./PLATFORM_PRODUCTION_DEPLOYMENT_CHECKLIST.md) to qualify and launch the platform infrastructure, the [production secret lifecycle](./PRODUCTION_SECRET_LIFECYCLE.md) for the external-manager, rotation, revocation, rewrap, and redaction gate, the [production telemetry validation](./PRODUCTION_TELEMETRY_VALIDATION.md) for logs, traces, audit separation, buffering, and outage behavior, and the [real Vault Transit microVM validation](./VAULT_TRANSIT_MICROVM_VALIDATION.md) for the repeatable local Vault acceptance drill. The general [service microVM guide](../../docs/vm-testbed/service-microvms.md) documents the added image, state, credential, telemetry, and teardown behavior. Then use the reusable [application deployment readiness checklist](./APPLICATION_DEPLOYMENT_READINESS_CHECKLIST.md) for each workload. The worked example is [OpenID-Connect-WASI-Hub](./OPENID_CONNECT_WASI_HUB_MICROVM_REHEARSAL.md). It uses three Wasm Cloud Platform nodes, one NATS microVM, one PostgreSQL microVM, and a host HAProxy front door.

Use the [production alerting validation](./PRODUCTION_ALERTING_VALIDATION.md)
for the canonical rule inventory, deterministic expressions, live queries,
notification delivery, resolution, and deduplication gate.

Use the repository skills in this order:

1. `$provision-microvm-testbed`
2. `$deploy-test-application`
3. `$destroy-microvm-testbed` only after the user explicitly requests teardown

All commands must run in Linux or WSL2. Keep the same state-file path for every phase. The state file is the lifecycle authority: do not discover or remove VMs, TAP devices, bridges, or processes by broad patterns.

The production-like preset is a local production-shaped rehearsal, not a production deployment. Production still requires TLS termination, externally managed secrets, monitoring and alerting, backup/restore, a highly available NATS deployment, and an application replica strategy.

PostgreSQL, Vault, and the disposable telemetry backends are external
integration fixtures. The platform pipeline validates its behavior against
them; it does not own their production deployment or certify their high
availability. PostgreSQL appears in the worked example because the OIDC
application requires it. Vault appears because production-mode platform nodes
require a supported non-exportable external seal root. Operators may choose a
different supported service and retain responsibility for its production SLA.
