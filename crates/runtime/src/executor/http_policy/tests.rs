use super::*;
use common::policy::InstancePolicy;
use http_body_util::Empty;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn hooks() -> PolicyHttpHooks {
    PolicyHttpHooks::new(PolicyEnforcer::new(InstancePolicy::default()), None)
}

#[tokio::test]
async fn store_extended_limits_bound_http_wire_writes_and_raw_connects() {
    use common::types::{AppConfig, AppId, ExtendedLimitsConfig};
    for connections in [0, 1] {
        let server = spawn_server(OK, Duration::ZERO, false).await;
        let mut config = AppConfig::default_for(AppId::new("extended-limits", "v1"));
        config.extended_limits = Some(ExtendedLimitsConfig {
            max_net_egress_bytes: Some(1),
            max_outbound_connections: Some(connections),
            max_open_fds: None,
            max_fs_write_bytes: None,
            max_table_elements: None,
        });
        let mut state = super::super::build_store_state(&config, vec![], 8080, None, None).unwrap();
        let enforcer = state.policy_enforcer.clone();
        // A raw socket reservation competes with HTTP for the same ceiling.
        let raw = enforcer.check_outbound_tcp_connect(server.addr.ip(), server.addr.port());
        assert_eq!(raw.is_ok(), connections == 1);
        assert!(
            send(&mut state.http_hooks, &format!("http://{}/", server.addr))
                .await
                .is_err()
        );
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 0);
        if connections == 1 {
            enforcer.record_outbound_disconnect();
            // Once the slot is free, the HTTP request hits the one-byte ceiling.
            assert!(
                send(&mut state.http_hooks, &format!("http://{}/", server.addr))
                    .await
                    .is_err()
            );
            assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 1);
            assert!(
                enforcer
                    .counters
                    .egress_denied_total
                    .load(Ordering::Relaxed)
                    > 0
            );
        }
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
        assert!(
            enforcer
                .counters
                .connection_denied_total
                .load(Ordering::Relaxed)
                > 0
        );
    }
}

#[tokio::test]
async fn extended_wire_budget_is_shared_between_request_stores() {
    use common::types::{AppConfig, AppId, ExtendedLimitsConfig};
    let mut config = AppConfig::default_for(AppId::new("shared-limits", "v1"));
    config.extended_limits = Some(ExtendedLimitsConfig {
        max_net_egress_bytes: Some(1),
        max_outbound_connections: Some(1),
        max_open_fds: None,
        max_fs_write_bytes: None,
        max_table_elements: None,
    });
    let counters = Arc::new(crate::policy_tracker::PolicyCounters::new());
    for _ in 0..2 {
        let server = spawn_server(OK, Duration::ZERO, false).await;
        let mut state =
            super::super::build_store_state(&config, vec![], 8080, None, Some(counters.clone()))
                .unwrap();
        assert!(
            send(&mut state.http_hooks, &format!("http://{}/", server.addr))
                .await
                .is_err()
        );
        assert_eq!(counters.egress_bytes.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters.outbound_connections_active.load(Ordering::Relaxed),
            0
        );
    }
    assert!(counters.egress_denied_total.load(Ordering::Relaxed) >= 2);
}

fn request(uri: &str) -> Request<WasiBody> {
    Request::builder()
        .uri(uri)
        .body(Empty::<Bytes>::new().map_err(Error::from).boxed_unsync())
        .unwrap()
}

async fn send(
    hooks: &mut PolicyHttpHooks,
    uri: &str,
) -> Result<(Response<WasiBody>, IoFuture), Error> {
    Pin::from(hooks.send_request(request(uri), None, Box::new(async { Ok(()) }))).await
}

fn resolve_to(
    hooks: &mut PolicyHttpHooks,
    addresses: Vec<SocketAddr>,
) -> Arc<std::sync::atomic::AtomicUsize> {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    hooks.resolver = Arc::new(move |_, _| {
        count.fetch_add(1, Ordering::Relaxed);
        let addresses = addresses.clone();
        Box::pin(async move { Ok(addresses) })
    });
    calls
}

struct Server {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_server(response: &'static [u8], pause: Duration, tls: bool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream: Box<dyn HttpStream> = if tls {
            let cert = rustls::pki_types::CertificateDer::from(
                include_bytes!("../../../testdata/http-policy-cert.der").to_vec(),
            );
            let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
                include_bytes!("../../../testdata/http-policy-key.der").to_vec(),
            );
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key.into())
            .unwrap();
            match tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(stream)
                .await
            {
                Ok(stream) => Box::new(stream),
                Err(_) => return,
            }
        } else {
            Box::new(stream)
        };
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if stream.read_exact(&mut byte).await.is_err() {
                return;
            }
            headers.push(byte[0]);
        }
        if tls {
            let text = String::from_utf8(headers).unwrap();
            assert!(text.to_ascii_lowercase().contains("host: allowed.test:"));
        }
        tokio::time::sleep(pause).await;
        let _ = stream.write_all(response).await;
        // Incomplete bodies remain open so tests exercise real host deadlines.
        if !response.ends_with(b"ok") {
            std::future::pending::<()>().await;
        }
    });
    Server { addr, task }
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

async fn collect(response: Response<WasiBody>, io: IoFuture) -> Result<Bytes, Error> {
    let task = tokio::spawn(Pin::from(io));
    let body = tokio::time::timeout(Duration::from_secs(2), response.into_body().collect())
        .await
        .expect("transport must finish within test deadline");
    task.abort();
    let _ = task.await;
    body.map(|body| body.to_bytes())
}

#[tokio::test]
async fn outgoing_http_uses_store_policy_including_ipv4_ipv6_and_mapped_addresses() {
    for target in [
        "http://127.0.0.1:1/",
        "http://[::1]:1/",
        "http://[::ffff:127.0.0.1]:1/",
    ] {
        let mut config =
            common::types::AppConfig::default_for(common::types::AppId::new("restricted", "v1"));
        config.policy = Some(common::policy::PolicyConfig {
            network: Some(common::policy::NetworkPolicyConfig {
                allow_outbound_tcp: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut state = super::super::build_store_state(&config, vec![], 8080, None, None).unwrap();
        use wasmtime_wasi_http::WasiHttpView;
        let future =
            state
                .http()
                .hooks
                .send_request(request(target), None, Box::new(async { Ok(()) }));
        assert!(matches!(
            Pin::from(future).await,
            Err(Error::DestinationIpProhibited)
        ));
        assert_eq!(
            state
                .policy_enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
    }
}

#[tokio::test]
async fn cidr_denials_cover_metadata_ipv6_and_mapped_ipv4() {
    let mut policy = InstancePolicy::default();
    policy.network.denied_cidrs = vec![
        "169.254.0.0/16".into(),
        "127.0.0.0/8".into(),
        "::1/128".into(),
    ];
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    for target in [
        "http://169.254.169.254/",
        "http://127.0.0.1:4222/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
    ] {
        assert!(matches!(
            send(&mut hooks, target).await,
            Err(Error::DestinationIpProhibited)
        ));
    }
}

#[tokio::test]
async fn dns_disabled_does_not_invoke_resolver_but_ip_literals_work() {
    let server = spawn_server(OK, Duration::ZERO, false).await;
    let mut policy = InstancePolicy::default();
    policy.network.allow_dns = false;
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    let calls = resolve_to(&mut hooks, vec![server.addr]);
    assert!(matches!(
        send(&mut hooks, "http://allowed.test/").await,
        Err(Error::HttpRequestDenied)
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let (response, io) = send(&mut hooks, &format!("http://{}/", server.addr))
        .await
        .unwrap();
    assert_eq!(collect(response, io).await.unwrap(), "ok");
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn resolves_once_and_connects_only_to_allowed_result() {
    let server = spawn_server(OK, Duration::ZERO, false).await;
    let mut policy = InstancePolicy::default();
    policy.network.allowed_cidrs = vec!["127.0.0.1/32".into()];
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    let calls = resolve_to(
        &mut hooks,
        vec!["169.254.169.254:80".parse().unwrap(), server.addr],
    );
    let (response, io) = send(&mut hooks, "http://allowed.test/").await.unwrap();
    assert_eq!(collect(response, io).await.unwrap(), "ok");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        hooks
            .enforcer
            .counters
            .connection_denied_total
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
    assert!(hooks.enforcer.counters.egress_bytes.load(Ordering::Relaxed) > 0);
}

#[tokio::test]
async fn supervisor_denial_and_cancelled_check_release_slots() {
    for pending in [false, true] {
        let check: SocketAddrCheckFn = Arc::new(move |_, _| {
            Box::pin(async move {
                if pending {
                    std::future::pending::<()>().await;
                }
                false
            })
        });
        let mut hooks =
            PolicyHttpHooks::new(PolicyEnforcer::new(InstancePolicy::default()), Some(check));
        hooks.limits.connect = Duration::from_millis(20);
        let error = send(&mut hooks, "http://127.0.0.1:4222/")
            .await
            .err()
            .unwrap();
        if pending {
            assert!(matches!(error, Error::ConnectionTimeout));
        } else {
            assert!(matches!(error, Error::DestinationIpProhibited));
        }
        assert_eq!(
            hooks
                .enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
    }
}

#[tokio::test]
async fn denied_name_and_failed_connect_leave_no_reservations() {
    let mut hooks = hooks();
    let mut policy = hooks.enforcer.policy.clone();
    policy.network.denied_cidrs = vec!["169.254.0.0/16".into()];
    hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    resolve_to(&mut hooks, vec!["169.254.169.254:80".parse().unwrap()]);
    assert!(matches!(
        send(&mut hooks, "http://metadata.test/").await,
        Err(Error::DestinationIpProhibited)
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    assert!(send(&mut hooks, &format!("http://{addr}/")).await.is_err());
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn transport_slot_lasts_until_connection_drop() {
    let server = spawn_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n",
        Duration::ZERO,
        false,
    )
    .await;
    let mut policy = InstancePolicy::default();
    policy.network.max_outbound_connections = 1;
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    let uri = format!("http://{}/", server.addr);
    let (response, io) = send(&mut hooks, &uri).await.unwrap();
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        1
    );
    assert!(send(&mut hooks, &uri).await.is_err());
    drop(response);
    drop(io);
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn host_transport_close_is_marked_before_kernel_observation() {
    let server = spawn_server(OK, Duration::ZERO, false).await;
    let hooks = hooks();
    hooks.enforcer.counters.enable_host_tcp_close_tracking();
    let stream = hooks
        .connect("127.0.0.1", server.addr.port())
        .await
        .unwrap();
    let port = stream.stream.local_addr().unwrap().port();
    drop(stream);
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
    assert!(hooks
        .enforcer
        .counters
        .consume_host_tcp_close(port, server.addr.port()));
    assert!(!hooks
        .enforcer
        .counters
        .consume_host_tcp_close(port, server.addr.port()));
}

#[tokio::test]
async fn stalled_headers_and_body_are_bounded_by_host_timeouts() {
    for headers in [false, true] {
        let server = spawn_server(
            if headers {
                b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n"
            } else {
                OK
            },
            if headers {
                Duration::ZERO
            } else {
                Duration::from_secs(5)
            },
            false,
        )
        .await;
        let mut hooks = hooks();
        hooks.limits.first_byte = Duration::from_millis(25);
        hooks.limits.between_bytes = Duration::from_millis(25);
        let result = send(&mut hooks, &format!("http://{}/", server.addr)).await;
        if headers {
            let (response, io) = result.unwrap();
            assert!(matches!(
                collect(response, io).await,
                Err(Error::ConnectionReadTimeout)
            ));
        } else {
            assert!(matches!(result, Err(Error::ConnectionReadTimeout)));
        }
        assert_eq!(
            hooks
                .enforcer
                .counters
                .io_http_timeouts
                .load(Ordering::Acquire),
            1
        );
    }
}

#[tokio::test]
async fn oversized_response_and_wire_egress_quota_are_enforced() {
    let server = spawn_server(OK, Duration::ZERO, false).await;
    let mut hooks = hooks();
    hooks.limits.response_bytes = 1;
    assert!(matches!(
        send(&mut hooks, &format!("http://{}/", server.addr)).await,
        Err(Error::HttpResponseBodySize(_))
    ));
    let server = spawn_server(OK, Duration::ZERO, false).await;
    let mut policy = InstancePolicy::default();
    policy.network.max_egress_bytes = 1;
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    assert!(send(&mut hooks, &format!("http://{}/", server.addr))
        .await
        .is_err());
    assert_eq!(
        hooks.enforcer.counters.egress_bytes.load(Ordering::Relaxed),
        1
    );
    assert!(
        hooks
            .enforcer
            .counters
            .egress_denied_total
            .load(Ordering::Relaxed)
            > 0
    );
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn https_validates_original_hostname_and_rejects_mismatch_and_untrusted_roots() {
    for (host, trusted) in [
        ("allowed.test", true),
        ("wrong.test", true),
        ("allowed.test", false),
    ] {
        let server = spawn_server(OK, Duration::ZERO, true).await;
        let mut hooks = hooks();
        resolve_to(&mut hooks, vec![server.addr]);
        if trusted {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(rustls::pki_types::CertificateDer::from(
                    include_bytes!("../../../testdata/http-policy-cert.der").to_vec(),
                ))
                .unwrap();
            hooks.tls = Arc::new(
                rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
            );
        }
        let result = send(
            &mut hooks,
            &format!("https://{host}:{}/", server.addr.port()),
        )
        .await;
        if trusted && host == "allowed.test" {
            let (response, io) = result.unwrap();
            assert_eq!(collect(response, io).await.unwrap(), "ok");
        } else {
            assert!(matches!(result, Err(Error::Tls(_))));
        }
        assert_eq!(
            hooks
                .enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
    }
}

#[tokio::test]
async fn chunked_response_size_and_total_body_deadline_are_enforced() {
    let server = spawn_server(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
        Duration::ZERO,
        false,
    )
    .await;
    let mut hooks = hooks();
    hooks.limits.response_bytes = 1;
    let (response, io) = send(&mut hooks, &format!("http://{}/", server.addr))
        .await
        .unwrap();
    assert!(matches!(
        collect(response, io).await,
        Err(Error::HttpResponseBodySize(_))
    ));
    let server = spawn_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n",
        Duration::ZERO,
        false,
    )
    .await;
    hooks.limits.total = Duration::from_millis(25);
    hooks.limits.response_bytes = 100;
    hooks.limits.between_bytes = Duration::from_secs(2);
    let (response, io) = send(&mut hooks, &format!("http://{}/", server.addr))
        .await
        .unwrap();
    assert!(matches!(
        collect(response, io).await,
        Err(Error::HttpResponseTimeout)
    ));
}

#[tokio::test]
async fn guest_options_cannot_extend_host_caps_and_dns_wait_is_bounded() {
    let server = spawn_server(OK, Duration::from_secs(5), false).await;
    let mut hooks = hooks();
    hooks.limits.first_byte = Duration::from_millis(20);
    let options = RequestOptions {
        first_byte_timeout: Some(Duration::from_secs(600)),
        ..Default::default()
    };
    let future = hooks.send_request(
        request(&format!("http://{}/", server.addr)),
        Some(options),
        Box::new(async { Ok(()) }),
    );
    assert!(matches!(
        Pin::from(future).await,
        Err(Error::ConnectionReadTimeout)
    ));
    hooks.limits.connect = Duration::from_millis(20);
    hooks.resolver = Arc::new(|_, _| Box::pin(std::future::pending()));
    assert!(matches!(
        send(&mut hooks, "http://never-resolves.test/").await,
        Err(Error::ConnectionTimeout)
    ));
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn redirects_are_returned_and_each_next_hop_is_checked() {
    let server = spawn_server(
        b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\n\r\n",
        Duration::ZERO,
        false,
    )
    .await;
    let mut policy = InstancePolicy::default();
    policy.network.denied_cidrs = vec!["169.254.0.0/16".into()];
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    let (response, io) = send(&mut hooks, &format!("http://{}/", server.addr))
        .await
        .unwrap();
    assert_eq!(response.status(), hyper::StatusCode::FOUND);
    let target = response.headers()[hyper::header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    collect(response, io).await.unwrap();
    assert!(matches!(
        send(&mut hooks, &target).await,
        Err(Error::DestinationIpProhibited)
    ));
    assert_eq!(
        hooks
            .enforcer
            .counters
            .outbound_connections_total
            .load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn mapped_ipv4_still_reaches_supervisor_loopback_gate() {
    let check: SocketAddrCheckFn = Arc::new(|addr, _| {
        Box::pin(async move {
            assert_eq!(addr, "127.0.0.1:4222".parse::<SocketAddr>().unwrap());
            false
        })
    });
    let mut hooks =
        PolicyHttpHooks::new(PolicyEnforcer::new(InstancePolicy::default()), Some(check));
    assert!(matches!(
        send(&mut hooks, "http://[::ffff:127.0.0.1]:4222/").await,
        Err(Error::DestinationIpProhibited)
    ));
}

#[tokio::test]
async fn invalid_authority_port_cannot_fall_back_to_a_default_port() {
    let mut hooks = hooks();
    assert!(matches!(
        send(&mut hooks, "http://127.0.0.1:65536/").await,
        Err(Error::HttpRequestUriInvalid)
    ));
}

fn profiled_hooks(profile: NetworkProfile, gate: Option<SocketAddrCheckFn>) -> PolicyHttpHooks {
    let cfg = common::policy::PolicyConfig {
        network: Some(common::policy::NetworkPolicyConfig {
            profile: Some(profile),
            allowed_cidrs: (profile == NetworkProfile::EgressAllowlist)
                .then(|| vec!["203.0.113.0/24".into()]),
            allowed_http_hosts: (profile == NetworkProfile::EgressAllowlist)
                .then(|| vec!["models.example.com".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    PolicyHttpHooks::new(PolicyEnforcer::new(cfg.resolve(8080).unwrap()), gate)
}

#[tokio::test]
async fn restrictive_mesh_names_use_only_the_local_gateway_without_dns() {
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = observed.clone();
    let gate: SocketAddrCheckFn = Arc::new(move |addr, _| {
        let seen = seen.clone();
        Box::pin(async move {
            assert_eq!(addr, "127.0.0.1:9080".parse::<SocketAddr>().unwrap());
            seen.store(true, Ordering::Relaxed);
            false
        })
    });
    let mut hooks = profiled_hooks(NetworkProfile::MeshOnly, Some(gate));
    let dns = resolve_to(&mut hooks, vec!["203.0.113.4:80".parse().unwrap()]);
    assert!(send(&mut hooks, "http://broker.agents.internal:9080/")
        .await
        .is_err());
    assert!(observed.load(Ordering::Relaxed));
    observed.store(false, Ordering::Relaxed);
    for url in [
        "http://broker.agents.internal:4222/",
        "http://example.com/",
        "http://127.0.0.1:4222/",
        "http://[::1]:9080/",
    ] {
        assert!(send(&mut hooks, url).await.is_err());
    }
    assert!(!observed.load(Ordering::Relaxed));
    assert_eq!(dns.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn egress_hostname_cidr_and_raw_socket_checks_cannot_bypass_each_other() {
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = observed.clone();
    let gate: SocketAddrCheckFn = Arc::new(move |_, _| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.store(true, Ordering::Relaxed);
            false
        })
    });
    let mut hooks = profiled_hooks(NetworkProfile::EgressAllowlist, Some(gate));
    let dns = resolve_to(&mut hooks, vec!["203.0.113.4:443".parse().unwrap()]);
    for url in [
        "https://evil.example.com/",
        "https://models.example.com.evil.test/",
        "http://203.0.113.4/",
    ] {
        assert!(matches!(
            send(&mut hooks, url).await,
            Err(Error::HttpRequestDenied)
        ));
    }
    assert_eq!(dns.load(Ordering::Relaxed), 0);
    assert!(send(&mut hooks, "https://MODELS.EXAMPLE.COM./")
        .await
        .is_err());
    assert!(observed.load(Ordering::Relaxed));
    assert_eq!(dns.load(Ordering::Relaxed), 1);
    observed.store(false, Ordering::Relaxed);
    resolve_to(&mut hooks, vec!["169.254.169.254:443".parse().unwrap()]);
    assert!(send(&mut hooks, "https://models.example.com/")
        .await
        .is_err());
    assert!(!observed.load(Ordering::Relaxed));
    assert!(hooks
        .enforcer
        .check_outbound_tcp_connect("203.0.113.4".parse().unwrap(), 443)
        .is_err());
}

#[tokio::test]
async fn raw_database_exception_does_not_authorize_arbitrary_http_names() {
    let mut policy = profiled_hooks(NetworkProfile::EgressAllowlist, None)
        .enforcer
        .policy;
    policy.network.allowed_http_hosts.clear();
    policy.network.allowed_tcp_endpoints = vec!["203.0.113.4:5432".parse().unwrap()];
    let mut hooks = PolicyHttpHooks::new(PolicyEnforcer::new(policy), None);
    let dns = resolve_to(&mut hooks, vec!["203.0.113.4:443".parse().unwrap()]);
    assert!(matches!(
        send(&mut hooks, "https://models.example.com/").await,
        Err(Error::HttpRequestDenied)
    ));
    assert_eq!(dns.load(Ordering::Relaxed), 0);
    hooks
        .enforcer
        .check_outbound_tcp_connect("203.0.113.4".parse().unwrap(), 5432)
        .unwrap();
    hooks.enforcer.record_outbound_disconnect();
}
