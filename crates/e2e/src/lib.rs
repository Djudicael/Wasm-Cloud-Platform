//! # E2E & Chaos Testing Framework for the Wasm Cloud Platform
//!
//! This crate provides a comprehensive chaos testing framework that systematically
//! injects failures into a running Wasm Cloud Platform cluster and verifies that
//! the system recovers correctly.
//!
//! ## Architecture
//!
//! - **`fixture`** — Cluster setup/teardown (NATS container + wasm-node instances)
//! - **`injector`** — Fault injection primitives (L1–L6 failure levels)
//! - **`verifier`** — Recovery verification primitives with TTR measurement
//! - **`reporter`** — Structured test reports (pass/fail, TTR, JSON export)
//! - **`chaos`** — Pre-built chaos test scenarios (L1–L6)
//! - **`helpers`** — HTTP helpers, wait-for, retry logic
//!
//! ## WSL Requirement
//!
//! Chaos tests **must** run inside WSL (Windows Subsystem for Linux) or a native
//! Linux host. They require:
//!
//! - Unix process signals (`SIGKILL`, `SIGTERM`) for process management
//! - `tc` / `iptables` for network partition simulation (L5)
//! - Podman or Docker for testcontainers (NATS containers)
//! - `CAP_NET_ADMIN` for `tc netem` (L5 NATS partition tests)
//!
//! ```bash
//! # Build inside WSL
//! wsl cargo build -p e2e
//!
//! # Run chaos tests inside WSL
//! wsl cargo test -p e2e -- --ignored --test-threads=1 chaos
//! ```
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use e2e::fixture::ClusterFixture;
//! use e2e::chaos;
//!
//! #[tokio::test]
//! #[ignore] // Requires NATS + built binaries
//! async fn test_my_chaos_scenario() {
//!     let report = chaos::l1_instance_crash::test_l1_instance_crash_recovery().await;
//!     report.print_summary();
//!     assert!(report.passed());
//! }
//! ```
//!
//! ## Failure Levels
//!
//! | Level | Failure Type              | Target TTR  | Max TTR |
//! |-------|---------------------------|-------------|---------|
//! | L1    | Instance crash (OOM/trap) | under 5s    | 10s     |
//! | L2    | Node process restart      | under 30s   | 60s     |
//! | L3    | Redb partial corruption   | under 10s   | 30s     |
//! | L4    | Full node rebuild         | under 120s  | 300s    |
//! | L5    | NATS partition (30s)      | under 45s   | 90s     |
//! | L6    | Multi-node failure        | under 300s  | 600s    |

pub mod fixture;
pub mod helpers;
pub mod injector;
pub mod reporter;
pub mod verifier;

pub mod chaos;

// Re-export the primary public API types for convenience.
pub use fixture::{ClusterFixture, NatsContainer, NodeProcess};
pub use injector::InjectionResult;
pub use reporter::{TestReport, TestResult};
pub use verifier::{CheckResult, VerificationResult};
