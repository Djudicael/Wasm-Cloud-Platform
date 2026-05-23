# Level 3 - Hardened Production

Use this guide when you are running a serious multi-node Linux deployment and want stronger operational trust and tighter controls than the baseline.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- you run multiple nodes
- the admin plane is fronted by infrastructure you control
- you want stronger release discipline and monitoring
- you want a hardened shared-nothing production posture

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

- signed upgrade provenance enabled
- release manifest generation in CI
- pinned release inputs
- tested rollback path

## Validation

- run the multi-node regressions in CI
- verify peer artifact transfers
- verify secret rotation across more than one node
- verify rolling upgrades across more than one node
- verify operational alerts trigger on forced failure cases

## Move To Level 4 When

- you want the strongest currently supported Linux posture
- you want formal rollout rehearsal and stricter operator discipline

Next guide:

- [`Level 4 - High-Assurance`](deployment-level-4-high-assurance.md)
