# Level 3 - Hardened Production

Use this guide for a multi-node Linux deployment that needs stronger operational
trust and tighter controls than Level 2. Each claimed failure domain must be
validated on production-equivalent infrastructure.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- you run multiple nodes
- the admin plane is fronted by infrastructure you control
- you want stronger release discipline and monitoring
- you need a hardened shared-nothing operating profile

## What To Install

1. everything from Level 2
2. environment-specific NATS credentials
3. alerting and metrics collection for node, proxy, NATS, and eBPF signals
4. CI coverage that runs the live multi-node regressions

## Starting Config

Start from:

- the Level 2 production config you already validated

Recommended additions:

- `vault-transit` or `aws-kms-hmac` instead of `file`
- dedicated NATS credentials per environment
- explicit artifact advertisement for routable peer exchange
- production OIDC/JWT gateway policy where external auth is required
- narrow `auth.trusted_proxies` configuration

## Setup Steps

1. move the seal-key source to `vault-transit` or `aws-kms-hmac` if you have not already
2. issue dedicated NATS credentials for the environment
3. front the admin plane only with infrastructure you trust
4. configure trusted proxies narrowly
5. enable routable peer artifact exchange where required
6. add CI coverage for live multi-node regressions
7. add alerting for node health, NATS disconnects, artifact transfer failures, secret rotation failures, and eBPF incidents

## Release Discipline

- promote only an approved semantic-version tag
- independently verify the release manifest, checksums, SLSA provenance, and SPDX attestations
- keep release inputs pinned and locked
- test rollback with the exact admitted bundle

## Validation

- run the multi-node regressions in CI
- verify peer artifact transfers
- verify secret rotation across more than one node
- verify rolling upgrades across more than one node
- verify operational alerts trigger on forced failure cases
- if physical-host fault tolerance is claimed, validate it across at least two
  physical hosts; a multi-VM run on one host does not prove that property

## Move To Level 4 When

- you need the high-assurance control profile
- you want formal rollout rehearsal and stricter operator discipline

Next guide:

- [`Level 4 - High-Assurance`](deployment-level-4-high-assurance.md)
