/// Reason a policy check denied an operation.
/// Returned as an error from WASI host functions.
#[derive(Debug, Clone)]
pub enum PolicyDenied {
    NetworkDisabled {
        protocol: &'static str,
    },
    DestinationDenied {
        ip: String,
        reason: String,
    },
    ConnectionLimitExceeded {
        current: u32,
        limit: u32,
    },
    EgressLimitExceeded {
        current: u64,
        requested: u64,
        limit: u64,
    },
    DnsDisabled,
    BindDenied {
        port: u16,
        allowed: Vec<u16>,
    },
    FdLimitExceeded {
        current: u32,
        limit: u32,
    },
    FsWriteLimitExceeded {
        current: u64,
        requested: u64,
        limit: u64,
    },
    FileCreateDenied,
    FileDeleteDenied,
}

impl std::fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyDenied::NetworkDisabled { protocol } => {
                write!(
                    f,
                    "outbound {} connections are disabled by policy",
                    protocol
                )
            }
            PolicyDenied::DestinationDenied { ip, reason } => {
                write!(f, "connection to {} denied: {}", ip, reason)
            }
            PolicyDenied::ConnectionLimitExceeded { current, limit } => {
                write!(
                    f,
                    "outbound connection limit exceeded ({}/{})",
                    current, limit
                )
            }
            PolicyDenied::EgressLimitExceeded {
                current,
                requested,
                limit,
            } => {
                write!(
                    f,
                    "egress limit exceeded ({}+{} > {})",
                    current, requested, limit
                )
            }
            PolicyDenied::DnsDisabled => write!(f, "DNS lookups are disabled by policy"),
            PolicyDenied::BindDenied { port, allowed } => {
                write!(
                    f,
                    "binding to port {} denied (allowed: {:?})",
                    port, allowed
                )
            }
            PolicyDenied::FdLimitExceeded { current, limit } => {
                write!(f, "FD limit exceeded ({}/{})", current, limit)
            }
            PolicyDenied::FsWriteLimitExceeded {
                current,
                requested,
                limit,
            } => {
                write!(
                    f,
                    "filesystem write limit exceeded ({}+{} > {})",
                    current, requested, limit
                )
            }
            PolicyDenied::FileCreateDenied => write!(f, "file creation is disabled by policy"),
            PolicyDenied::FileDeleteDenied => write!(f, "file deletion is disabled by policy"),
        }
    }
}

impl std::error::Error for PolicyDenied {}
