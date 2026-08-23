use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::policy::{FilesystemPolicy, InstancePolicy, NetworkPolicy};

use super::{PolicyCounters, PolicyDenied, PolicyEnforcer};

fn make_policy() -> InstancePolicy {
    InstancePolicy {
        network: NetworkPolicy::default(),
        filesystem: FilesystemPolicy::default(),
    }
}

#[test]
fn test_policy_counters_new() {
    let counters = PolicyCounters::new();
    assert_eq!(
        counters.outbound_connections_active.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        counters.outbound_connections_total.load(Ordering::Relaxed),
        0
    );
    assert_eq!(counters.egress_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(counters.dns_lookups_total.load(Ordering::Relaxed), 0);
    assert_eq!(
        counters.inbound_connections_active.load(Ordering::Relaxed),
        0
    );
    assert_eq!(counters.open_fds.load(Ordering::Relaxed), 0);
    assert_eq!(counters.open_fds_peak.load(Ordering::Relaxed), 0);
    assert_eq!(counters.fd_open_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.fs_write_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(counters.fs_read_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(counters.file_creates_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.file_deletes_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.connection_denied_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.egress_denied_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.fd_denied_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.fs_write_denied_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.bind_denied_total.load(Ordering::Relaxed), 0);
    assert_eq!(counters.dns_denied_total.load(Ordering::Relaxed), 0);
}

#[test]
fn test_policy_enforcer_new() {
    let enforcer = PolicyEnforcer::new(make_policy());
    assert_eq!(enforcer.allowed_cidrs_parsed.len(), 0);
    assert_eq!(enforcer.denied_cidrs_parsed.len(), 0);
}

#[test]
fn test_check_outbound_tcp_connect_allowed() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_outbound_tcp: true,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    let ip: IpAddr = "93.184.216.34".parse().unwrap();
    assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        1
    );
    enforcer.record_outbound_disconnect();
}

#[test]
fn test_check_outbound_tcp_connect_denied_by_policy() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_outbound_tcp: false,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    let ip: IpAddr = "93.184.216.34".parse().unwrap();
    let result = enforcer.check_outbound_tcp_connect(ip, 443);
    assert!(result.is_err());
    match result.unwrap_err() {
        PolicyDenied::NetworkDisabled { protocol } => assert_eq!(protocol, "tcp"),
        _ => panic!("Expected NetworkDisabled"),
    }
}

#[test]
fn test_check_outbound_tcp_connect_denied_by_cidr() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_outbound_tcp: true,
            denied_cidrs: vec!["10.0.0.0/8".to_string()],
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    let ip: IpAddr = "10.1.2.3".parse().unwrap();
    let result = enforcer.check_outbound_tcp_connect(ip, 80);
    assert!(result.is_err());
    match result.unwrap_err() {
        PolicyDenied::DestinationDenied { reason, .. } => {
            assert!(reason.contains("denied_cidrs"));
        }
        _ => panic!("Expected DestinationDenied"),
    }
}

#[test]
fn test_check_outbound_tcp_connect_allowed_cidr() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_outbound_tcp: true,
            allowed_cidrs: vec!["93.184.216.0/24".to_string()],
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    let ip: IpAddr = "93.184.216.34".parse().unwrap();
    assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
    enforcer.record_outbound_disconnect();

    let denied_ip: IpAddr = "10.0.0.1".parse().unwrap();
    let result = enforcer.check_outbound_tcp_connect(denied_ip, 80);
    assert!(result.is_err());
}

#[test]
fn test_check_outbound_tcp_connect_connection_limit() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_outbound_tcp: true,
            max_outbound_connections: 2,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    let ip: IpAddr = "93.184.216.34".parse().unwrap();

    assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
    assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());

    let result = enforcer.check_outbound_tcp_connect(ip, 443);
    assert!(result.is_err());
    match result.unwrap_err() {
        PolicyDenied::ConnectionLimitExceeded { current, limit } => {
            assert_eq!(current, 2);
            assert_eq!(limit, 2);
        }
        _ => panic!("Expected ConnectionLimitExceeded"),
    }

    enforcer.record_outbound_disconnect();
    assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());

    enforcer.record_outbound_disconnect();
    enforcer.record_outbound_disconnect();
}

#[test]
fn test_record_outbound_connect_and_disconnect() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_outbound_tcp: true,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    let ip: IpAddr = "93.184.216.34".parse().unwrap();

    assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        1
    );

    enforcer.record_outbound_connect();
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_total
            .load(Ordering::Relaxed),
        1
    );

    enforcer.record_outbound_disconnect();
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
}

#[test]
fn test_record_outbound_disconnect_underflow_guard() {
    let enforcer = PolicyEnforcer::new(make_policy());
    enforcer.record_outbound_disconnect();
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
}

#[test]
fn test_check_egress_unlimited() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            max_egress_bytes: 0,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    assert!(enforcer.check_and_record_egress(1_000_000).is_ok());
    assert_eq!(
        enforcer.counters.egress_bytes.load(Ordering::Relaxed),
        1_000_000
    );
}

#[test]
fn test_check_egress_limited() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            max_egress_bytes: 1000,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);

    assert!(enforcer.check_and_record_egress(500).is_ok());
    assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 500);

    assert!(enforcer.check_and_record_egress(500).is_ok());
    assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 1000);

    let result = enforcer.check_and_record_egress(1);
    assert!(result.is_err());
    match result.unwrap_err() {
        PolicyDenied::EgressLimitExceeded {
            current,
            requested,
            limit,
        } => {
            assert_eq!(current, 1000);
            assert_eq!(requested, 1);
            assert_eq!(limit, 1000);
        }
        _ => panic!("Expected EgressLimitExceeded"),
    }
}

#[test]
fn test_record_egress() {
    let enforcer = PolicyEnforcer::new(make_policy());
    enforcer
        .counters
        .egress_bytes
        .fetch_add(42, Ordering::Relaxed);
    assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 42);
}

#[test]
fn test_check_dns_lookup() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allow_dns: true,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    assert!(enforcer.check_dns_lookup().is_ok());

    let policy_denied = InstancePolicy {
        network: NetworkPolicy {
            allow_dns: false,
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer_denied = PolicyEnforcer::new(policy_denied);
    assert!(enforcer_denied.check_dns_lookup().is_err());
}

#[test]
fn test_check_bind() {
    let policy = InstancePolicy {
        network: NetworkPolicy {
            allowed_bind_ports: vec![8080, 9090],
            ..NetworkPolicy::default()
        },
        filesystem: FilesystemPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    assert!(enforcer.check_bind(8080).is_ok());
    assert!(enforcer.check_bind(9090).is_ok());
    assert!(enforcer.check_bind(3000).is_err());
}

#[test]
fn test_check_fd_open() {
    let policy = InstancePolicy {
        filesystem: FilesystemPolicy {
            max_open_fds: 2,
            ..FilesystemPolicy::default()
        },
        network: NetworkPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);

    assert!(enforcer.check_fd_open().is_ok());
    assert!(enforcer.check_fd_open().is_ok());

    let result = enforcer.check_fd_open();
    assert!(result.is_err());
    match result.unwrap_err() {
        PolicyDenied::FdLimitExceeded { current, limit } => {
            assert_eq!(current, 2);
            assert_eq!(limit, 2);
        }
        _ => panic!("Expected FdLimitExceeded"),
    }

    enforcer.record_fd_close();
    assert!(enforcer.check_fd_open().is_ok());

    enforcer.record_fd_close();
    enforcer.record_fd_close();
}

#[test]
fn test_record_fd_open_and_close() {
    let enforcer = PolicyEnforcer::new(make_policy());

    assert!(enforcer.check_fd_open().is_ok());
    assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 1);
    assert_eq!(enforcer.counters.open_fds_peak.load(Ordering::Relaxed), 1);

    enforcer.record_fd_open();
    assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 1);
    assert_eq!(enforcer.counters.open_fds_peak.load(Ordering::Relaxed), 1);
    assert_eq!(enforcer.counters.fd_open_total.load(Ordering::Relaxed), 1);

    assert!(enforcer.check_fd_open().is_ok());
    assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 2);
    assert_eq!(enforcer.counters.open_fds_peak.load(Ordering::Relaxed), 2);

    enforcer.record_fd_close();
    enforcer.record_fd_close();
    assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 0);
    assert_eq!(enforcer.counters.open_fds_peak.load(Ordering::Relaxed), 2);
}

#[test]
fn test_record_fd_close_underflow_guard() {
    let enforcer = PolicyEnforcer::new(make_policy());
    enforcer.record_fd_close();
    assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 0);
}

#[test]
fn test_reset_active_counters_clears_only_live_resource_counts() {
    let enforcer = PolicyEnforcer::new(make_policy());
    enforcer
        .check_outbound_tcp_connect("93.184.216.34".parse().unwrap(), 443)
        .unwrap();
    enforcer.check_fd_open().unwrap();
    enforcer.record_outbound_connect();
    enforcer.record_fd_open();

    enforcer.reset_active_counters();

    assert_eq!(
        enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 0);
    assert_eq!(enforcer.counters.open_fds_peak.load(Ordering::Relaxed), 1);
    assert_eq!(
        enforcer
            .counters
            .outbound_connections_total
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(enforcer.counters.fd_open_total.load(Ordering::Relaxed), 1);
}

#[test]
fn test_release_tracked_connections_preserves_other_store_reservations() {
    let counters = Arc::new(PolicyCounters::new());
    let first = PolicyEnforcer::with_counters(make_policy(), counters.clone());
    let second = PolicyEnforcer::with_counters(make_policy(), counters.clone());

    first
        .check_outbound_tcp_connect("93.184.216.34".parse().unwrap(), 443)
        .unwrap();
    second
        .check_outbound_tcp_connect("93.184.216.35".parse().unwrap(), 443)
        .unwrap();
    assert_eq!(
        counters.outbound_connections_active.load(Ordering::Relaxed),
        2
    );

    first.release_tracked_outbound_connections();
    assert_eq!(
        counters.outbound_connections_active.load(Ordering::Relaxed),
        1
    );

    second.release_tracked_outbound_connections();
    assert_eq!(
        counters.outbound_connections_active.load(Ordering::Relaxed),
        0
    );
}

#[test]
fn test_check_fs_write() {
    let policy = InstancePolicy {
        filesystem: FilesystemPolicy {
            max_fs_write_bytes: 1000,
            ..FilesystemPolicy::default()
        },
        network: NetworkPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);

    assert!(enforcer.check_and_record_fs_write(500).is_ok());
    assert_eq!(
        enforcer.counters.fs_write_bytes.load(Ordering::Relaxed),
        500
    );

    assert!(enforcer.check_and_record_fs_write(500).is_ok());
    assert_eq!(
        enforcer.counters.fs_write_bytes.load(Ordering::Relaxed),
        1000
    );

    let result = enforcer.check_and_record_fs_write(1);
    assert!(result.is_err());
    match result.unwrap_err() {
        PolicyDenied::FsWriteLimitExceeded {
            current,
            requested,
            limit,
        } => {
            assert_eq!(current, 1000);
            assert_eq!(requested, 1);
            assert_eq!(limit, 1000);
        }
        _ => panic!("Expected FsWriteLimitExceeded"),
    }
}

#[test]
fn test_record_fs_write() {
    let enforcer = PolicyEnforcer::new(make_policy());
    enforcer
        .counters
        .fs_write_bytes
        .fetch_add(42, Ordering::Relaxed);
    assert_eq!(enforcer.counters.fs_write_bytes.load(Ordering::Relaxed), 42);
}

#[test]
fn test_check_file_create() {
    let policy = InstancePolicy {
        filesystem: FilesystemPolicy {
            allow_file_create: true,
            ..FilesystemPolicy::default()
        },
        network: NetworkPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    assert!(enforcer.check_file_create().is_ok());

    let policy_denied = InstancePolicy {
        filesystem: FilesystemPolicy {
            allow_file_create: false,
            ..FilesystemPolicy::default()
        },
        network: NetworkPolicy::default(),
    };
    let enforcer_denied = PolicyEnforcer::new(policy_denied);
    assert!(enforcer_denied.check_file_create().is_err());
}

#[test]
fn test_check_file_delete() {
    let policy = InstancePolicy {
        filesystem: FilesystemPolicy {
            allow_file_delete: true,
            ..FilesystemPolicy::default()
        },
        network: NetworkPolicy::default(),
    };
    let enforcer = PolicyEnforcer::new(policy);
    assert!(enforcer.check_file_delete().is_ok());

    let policy_denied = InstancePolicy {
        filesystem: FilesystemPolicy {
            allow_file_delete: false,
            ..FilesystemPolicy::default()
        },
        network: NetworkPolicy::default(),
    };
    let enforcer_denied = PolicyEnforcer::new(policy_denied);
    assert!(enforcer_denied.check_file_delete().is_err());
}

#[test]
fn test_ip_in_cidrs() {
    let cidrs: Vec<ipnet::IpNet> = vec![
        "10.0.0.0/8".parse().unwrap(),
        "192.168.0.0/16".parse().unwrap(),
    ];
    let ip_in: IpAddr = "10.1.2.3".parse().unwrap();
    let ip_in2: IpAddr = "192.168.1.1".parse().unwrap();
    let ip_out: IpAddr = "93.184.216.34".parse().unwrap();

    assert!(PolicyEnforcer::ip_in_cidrs(ip_in, &cidrs));
    assert!(PolicyEnforcer::ip_in_cidrs(ip_in2, &cidrs));
    assert!(!PolicyEnforcer::ip_in_cidrs(ip_out, &cidrs));
}

#[test]
fn test_parse_cidrs_invalid_skipped() {
    let cidrs = vec![
        "10.0.0.0/8".to_string(),
        "not-a-cidr".to_string(),
        "192.168.0.0/16".to_string(),
    ];
    let parsed = PolicyEnforcer::parse_cidrs(&cidrs);
    assert_eq!(parsed.len(), 2);
}
