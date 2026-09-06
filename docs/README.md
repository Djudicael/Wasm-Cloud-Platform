# Wasm Cloud Platform manual

This directory is the maintained user and operator manual. Start with the
journey that matches the work you are doing. Implementation records under
`INFRA_IMPL/` explain design history; they do not replace these operating
instructions.

## Start and deploy

- [Getting started](getting-started.md) builds and starts a development node.
- [Deployment levels](deployment-levels.md) selects an operating profile and
  links each level's requirements.
- [Deploying applications](deploying-applications.md) covers artifacts,
  manifests, routes, policy, secrets, updates, and removal.
- [Full-stack example](full-stack-example.md) connects applications, PostgreSQL,
  OIDC, and the node-local internal mesh.
- [gRPC compatibility](grpc-compatibility.md) defines the tested native and
  `wasi:http` component paths.

## Operate the platform

- [NATS setup](nats-setup.md) covers JetStream, authentication, TLS, monitoring,
  backup, and recovery.
- [Deploy ingress operations](deploy-ingress-operations.md) covers the separate
  CI/CD ingress, artifact credentials, HA, and rotation.
- [Internal mesh](internal-mesh.md) defines node-local `.internal` routing,
  placement, dependencies, and namespace authorization.
- [Observability](observability.md) covers metrics, logs, traces, health, alerts,
  and incident playbooks.
- [eBPF](ebpf.md) covers activation, degraded behavior, security signals, and
  recovery actions.
- [Performance benchmarks](performance-benchmarks.md) documents reproducible
  cold-start and runtime measurements.

## Gateway

- [OIDC setup](gateway/oidc-setup.md)
- [CORS examples](gateway/cors-examples.md)
- [Circuit-breaker tuning](gateway/circuit-breaker-tuning.md)

## Deployment profiles

- [Level 0: local development](deployment-level-0-local-development.md)
- [Level 1: single-node private environment](deployment-level-1-single-node-private.md)
- [Level 2: production baseline controls](deployment-level-2-production-baseline.md)
- [Level 3: hardened multi-node controls](deployment-level-3-hardened-production.md)
- [Level 4: high-assurance controls](deployment-level-4-high-assurance.md)

Levels describe increasing operator controls. They do not certify an environment
as production-ready. For any production rollout, complete the repository's
[production deployment checklist](../INFRA_IMPL/process/PLATFORM_PRODUCTION_DEPLOYMENT_CHECKLIST.md)
against the exact release and infrastructure.

## Local Firecracker testbed

- [Architecture](vm-testbed/architecture.md)
- [WSL2 quickstart](vm-testbed/wsl-quickstart.md)
- [Manual setup and script reference](vm-testbed/manual-setup.md)
- [CI integration](vm-testbed/ci-integration.md)
- [Service microVMs](vm-testbed/service-microvms.md)

The Firecracker environment is a local platform testbed. Its results do not
validate production PKI, highly available NATS, external load balancing,
provider behavior, real host capacity, or physical-host failure tolerance.
