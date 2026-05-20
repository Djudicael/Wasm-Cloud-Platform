# Security Policy

## Supported scope

This repository is still evolving, but security issues in the following areas should be treated as high priority:

- node control-plane authentication and TLS
- artifact upload/download authorization
- upgrade provenance and binary installation flow
- tenant isolation / runtime policy enforcement
- secret storage, key handling, and bootstrap secret transfer
- cluster messaging integrity and replay behavior

## Reporting a vulnerability

If you believe you have found a security vulnerability, please do **not** open a public GitHub issue with exploit details.

Instead:

1. prepare a private report with:
   - affected component(s)
   - impact
   - reproduction steps or proof of concept
   - version / commit tested
   - any suggested mitigation
2. send the report to the project maintainers through a private channel if one is available
3. if no private channel is available yet, open a minimal GitHub issue requesting a secure contact path **without including sensitive details**

## What to include

Useful report content:

- exact configuration used
- whether the issue requires cluster mode, eBPF, TLS, or special deployment topology
- whether the issue is local-only, same-host, or remotely exploitable
- whether tenant isolation, secrets, upgrade trust, or artifact integrity is affected
- logs, request samples, or failing test cases if safe to share privately

## Disclosure expectations

The preferred process is:

- private report
- maintainer triage and reproduction
- fix preparation
- coordinated public disclosure after a patch or mitigation exists

## Out of scope / lower priority examples

These usually do **not** qualify as security vulnerabilities by themselves unless they enable a concrete exploit path:

- performance-only regressions
- missing hardening that is already clearly documented as unsupported
- local development defaults used outside their documented scope
- issues that require source modification by an already trusted operator

## Operational note

Because this platform handles:

- tenant workloads
- artifacts
- upgrade binaries
- secrets
- node-to-node coordination

please err on the side of reporting anything that could affect:

- remote code execution
- artifact substitution
- privilege escalation
- cross-tenant access
- secret disclosure
- control-plane takeover
- bypass of runtime/network/filesystem policy
