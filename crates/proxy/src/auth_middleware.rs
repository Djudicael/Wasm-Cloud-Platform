//! Admin API authentication middleware.
//!
//! This module implements bearer-token authentication with separate read/write
//! permission levels, per-IP rate limiting, and Prometheus metrics for the
//! admin API.
//!
//! # Architecture
//!
//! The middleware runs as an Axum layer before any admin handler:
//!
//! ```text
//! Request -> rate limit check -> auth check -> permission check -> handler
//! ```
//!
//! Public endpoints (`/health`, `/status/metrics`) bypass authentication entirely
//! so that load balancers and Prometheus can probe the node without credentials.

mod admin_utils;
mod client_ip;
mod core;
mod metrics;
mod rate_limit;

#[cfg(test)]
mod tests;

pub use admin_utils::{
    check_admin_tls_requirement, check_config_file_permissions, validate_rotation_request,
    RotateTokenRequest,
};
pub use client_ip::extract_client_ip;
pub use core::{
    auth_middleware, is_public_endpoint, required_permission, AuditCallback, AuditInfo, AuthState,
};
pub use metrics::AuthMetrics;
pub use rate_limit::AdminRateLimiter;

// Re-export axum::http for use in this module's public API.
pub use axum::http;
