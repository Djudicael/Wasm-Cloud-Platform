# Dependency security exception register

Reviewed: `2026-09-06`

Owner: platform maintainers

This register defines every advisory that the locked workspace is permitted to
ignore. `.cargo/audit.toml` is the executable policy. The release manifest binds
that file by SHA-256 and copies the exact exception list reported by
`cargo-audit`; a candidate cannot conceal or silently add an exception.

An exception is neither a claim that an advisory is false nor a permanent
waiver. Review all entries before every signed promotion, on any relevant
dependency update, and no later than `2026-10-06`. Remove an entry immediately
when its dependency disappears or a compatible upstream fix is available.

| Advisory | Locked path and classification | Reachability and temporary control | Removal condition |
|---|---|---|---|
| `RUSTSEC-2024-0437` | `protobuf 2.28.0` through `pingora-core 0.8.1 -> prometheus 0.13.4`; denial-of-service vulnerability in recursive parsing. | The Pingora path gathers platform-created metric families and uses `TextEncoder`; the transitive protobuf code constructs/encodes messages. It does not parse attacker-provided protobuf. Keep metrics endpoints access-controlled and size/rate limited. | Upgrade Pingora to a version that no longer selects vulnerable protobuf, or patch the dependency graph, then rerun gateway and metrics tests. |
| `RUSTSEC-2026-0253` | `lru 0.16.4` through Pingora pool/cache; conditional use-after-free if a key's `Drop` implementation panics during `LruCache::pop()`. | Exercised Pingora cache keys are integer cache identifiers (`u64`/`u128`) and connection identifiers, whose drop cannot panic. Do not introduce a panicking-drop key into this path. Cargo Deny is configured to inspect unsound advisories in transitive dependencies. | Upgrade when Pingora accepts `lru >=0.18.2`, or carry a reviewed patch, then run proxy cache/pool, load, and fault tests. |
| `RUSTSEC-2025-0069` | `daemonize 0.5.0` via Pingora; unmaintained. | Maintenance/supply-chain debt, not a published vulnerability. The platform is supervised externally in the supported deployment model. | Pingora removes/replaces it, or the platform removes that dependency path. |
| `RUSTSEC-2024-0388` | `derivative 2.2.0` via Pingora; unmaintained. | Maintenance/supply-chain debt; keep it pinned by `Cargo.lock` and covered by source/build attestations. | Upstream removes/replaces it or a reviewed patch is adopted. |
| `RUSTSEC-2024-0384` | `instant 0.1.13` through the current WASI dependency graph; unmaintained. | Maintenance/supply-chain debt; WASI targets are compiled explicitly and exercised by the application validation. | The upstream dependency graph removes/replaces it. |
| `RUSTSEC-2026-0173` | `proc-macro-error2 2.0.1` through `tabled` and Pingora dependencies; unmaintained and future-incompatible. | Build-time procedural macro only; it is not linked as a runtime request-processing capability. Frozen builds and exact-source attestations limit supply-chain drift. | Upgrade/remove the upstream users before the compiler rejects it. |
| `RUSTSEC-2025-0134` | `rustls-pemfile 2.2.0` through `pingora-rustls 0.8.1`; unmaintained. | Maintenance/supply-chain debt. Production TLS material remains validated by the platform's fail-closed TLS configuration and integration tests. | Upgrade Pingora to a release that uses the replacement API, or patch that dependency path, then rerun proxy TLS tests. |

## Rejected exception

`RUSTSEC-2023-0071` (`rsa 0.9.10`) is not accepted. The OIDC/JWT implementation
uses `jsonwebtoken` with its `aws-lc-rs` backend, and tests generate ephemeral
RSA keys with that same backend rather than the vulnerable Rust RSA key
generator. No private key is committed. The release check must confirm that
`cargo tree -i rsa@0.9.10 --locked` finds no matching package.

## Required review commands

Run in WSL/Linux:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
cargo audit --deny warnings

cd /tmp
cargo audit \
  --file /mnt/d/dev/Wasm-Cloud-Platform/Cargo.lock \
  --json --deny warnings > wcp-unfiltered-audit.json
```

The first command enforces the approved policy. The second deliberately runs
outside the repository so `.cargo/audit.toml` cannot filter the review view.
Compare every unfiltered finding with this register before signing or
promoting a release.
