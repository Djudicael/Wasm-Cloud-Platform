//! Policy-controlled HTTP transport. DNS is resolved once and only approved
//! socket addresses are passed to connect; TLS still verifies the URI hostname.

use super::{
    socket_policy::compose_http_addr_check, SocketAddrCheckFn, SocketAddrUse, SocketPolicyCheck,
};
use crate::policy_tracker::PolicyEnforcer;
use bytes::Bytes;
use common::policy::{canonical_http_host, is_mesh_address, is_mesh_http_host, NetworkProfile};
use futures::future::poll_fn;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::{timeout_at, Instant, Sleep};
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

type IoFuture = Box<dyn Future<Output = Result<(), Error>> + Send>;
type SendFuture = Box<dyn Future<Output = Result<(Response<WasiBody>, IoFuture), Error>> + Send>;
type ResolveFuture = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;
type Resolver = Arc<dyn Fn(String, u16) -> ResolveFuture + Send + Sync>;

#[derive(Clone)]
struct HttpDeadlineCount {
    counters: Arc<crate::policy_tracker::PolicyCounters>,
    recorded: Arc<AtomicBool>,
}
impl HttpDeadlineCount {
    fn record(&self) {
        if !self.recorded.swap(true, Ordering::AcqRel) {
            self.counters
                .io_http_timeouts
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy)]
struct HttpLimits {
    connect: Duration,
    first_byte: Duration,
    between_bytes: Duration,
    total: Duration,
    response_bytes: u64,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(30),
            between_bytes: Duration::from_secs(30),
            total: Duration::from_secs(120),
            response_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub(super) struct PolicyHttpHooks {
    enforcer: PolicyEnforcer,
    check: SocketAddrCheckFn,
    resolver: Resolver,
    tls: Arc<rustls::ClientConfig>,
    limits: HttpLimits,
}

impl PolicyHttpHooks {
    pub(super) fn new(enforcer: PolicyEnforcer, extra: Option<SocketAddrCheckFn>) -> Self {
        let check = compose_http_addr_check(
            SocketPolicyCheck::from_instance_policy(&enforcer.policy),
            enforcer.clone(),
            extra,
        );
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("default TLS protocols")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Self {
            enforcer,
            check,
            resolver: Arc::new(|host, port| {
                Box::pin(async move {
                    tokio::net::lookup_host((host.as_str(), port))
                        .await
                        .map(Iterator::collect)
                })
            }),
            tls: Arc::new(tls),
            limits: HttpLimits::default(),
        }
    }

    async fn connect(&self, host: &str, port: u16) -> Result<PolicyStream, Error> {
        let profile = self.enforcer.policy.network.profile;
        let literal = host
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, port));
        let local_mesh = literal.is_some_and(is_mesh_address);
        let mesh_name = profile.is_some() && is_mesh_http_host(host);
        if mesh_name && port != common::INTERNAL_GATEWAY_PORT {
            return Err(Error::DestinationIpProhibited);
        }
        if profile == Some(NetworkProfile::MeshOnly) && !mesh_name && !local_mesh {
            return Err(Error::HttpRequestDenied);
        }
        let allowed_hosts = &self.enforcer.policy.network.allowed_http_hosts;
        if (!allowed_hosts.is_empty() || profile == Some(NetworkProfile::EgressAllowlist))
            && !mesh_name
            && !local_mesh
        {
            let host = canonical_http_host(host).map_err(|_| Error::HttpRequestDenied)?;
            if !allowed_hosts.contains(&host) {
                return Err(Error::HttpRequestDenied);
            }
        }
        let addresses = if mesh_name {
            vec![SocketAddr::from((
                [127, 0, 0, 1],
                common::INTERNAL_GATEWAY_PORT,
            ))]
        } else {
            match host.parse::<IpAddr>() {
                Ok(ip) => vec![SocketAddr::new(ip, port)],
                Err(_) => {
                    if profile == Some(NetworkProfile::EgressAllowlist) {
                        // The exact hostname was authorized above. Raw WASI
                        // DNS remains disabled, preventing an alternate DNS
                        // exfiltration path around the HTTP hostname list.
                        self.enforcer
                            .counters
                            .dns_lookups_total
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.enforcer
                            .check_dns_lookup()
                            .map_err(|_| Error::HttpRequestDenied)?;
                    }
                    (self.resolver)(host.to_owned(), port)
                        .await
                        .map_err(|_| Error::DnsError {
                            rcode: None,
                            info_code: None,
                        })?
                }
            }
        };
        let mut last_error = Error::DestinationIpProhibited;
        for address in addresses {
            if !(self.check)(address, SocketAddrUse::TcpConnect).await {
                continue;
            }
            // The composed check reserves exactly one slot; every subsequent
            // error/cancellation releases it, even before a TCP socket exists.
            let slot = ConnectionSlot(self.enforcer.clone());
            let socket = if address.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            }
            .map_err(Error::Connect)?;
            let bind = if address.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            socket
                .bind(bind.parse().expect("literal bind address"))
                .map_err(Error::Connect)?;
            let source_port = socket.local_addr().map_err(Error::Connect)?.port();
            if !self
                .enforcer
                .counters
                .track_host_tcp_close(source_port, address.port())
            {
                self.enforcer
                    .counters
                    .connection_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(Error::ConnectionLimitReached);
            }
            match socket.connect(address).await {
                Ok(stream) => return Ok(PolicyStream { stream, slot }),
                Err(err) => last_error = Error::Connect(err),
            }
        }
        Err(last_error)
    }

    async fn send(
        self,
        mut request: Request<WasiBody>,
        options: Option<RequestOptions>,
        timeout_count: HttpDeadlineCount,
    ) -> Result<(Response<WasiBody>, IoFuture), Error> {
        let options = options.unwrap_or_default();
        let deadline = Instant::now() + self.limits.total;
        let connect_deadline = deadline.min(
            Instant::now()
                + options
                    .connect_timeout
                    .unwrap_or(self.limits.connect)
                    .min(self.limits.connect),
        );
        let scheme = request
            .uri()
            .scheme_str()
            .ok_or(Error::HttpRequestUriInvalid)?;
        if scheme != "http" && scheme != "https" {
            return Err(Error::HttpProtocolError);
        }
        let tls = scheme == "https";
        let authority = request
            .uri()
            .authority()
            .ok_or(Error::HttpRequestUriInvalid)?
            .clone();
        if authority.as_str().contains('@') {
            return Err(Error::HttpRequestUriInvalid);
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = match authority.port_u16() {
            Some(port) => port,
            None if authority.as_str() == authority.host() => {
                if tls {
                    443
                } else {
                    80
                }
            }
            None => return Err(Error::HttpRequestUriInvalid),
        };
        let stream = timeout_at(connect_deadline, self.connect(host, port))
            .await
            .map_err(|_| Error::ConnectionTimeout)??;
        // Always derive TLS identity from the original authority, never the
        // approved address or a caller-supplied Host header.
        let stream: Box<dyn HttpStream> = if tls {
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|_| Error::HttpRequestUriInvalid)?;
            let connector = tokio_rustls::TlsConnector::from(self.tls.clone());
            Box::new(
                timeout_at(connect_deadline, connector.connect(name, stream))
                    .await
                    .map_err(|_| Error::ConnectionTimeout)?
                    .map_err(Error::Tls)?,
            )
        } else {
            Box::new(stream)
        };
        let (mut sender, connection) = timeout_at(
            connect_deadline,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|_| Error::ConnectionTimeout)??;
        request.headers_mut().insert(
            hyper::header::HOST,
            hyper::header::HeaderValue::from_str(authority.as_str())
                .map_err(|_| Error::HttpRequestUriInvalid)?,
        );
        *request.uri_mut() = hyper::Uri::builder()
            .path_and_query(
                request
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/"),
            )
            .build()
            .map_err(|_| Error::HttpRequestUriInvalid)?;
        let first_deadline = deadline.min(
            Instant::now()
                + options
                    .first_byte_timeout
                    .unwrap_or(self.limits.first_byte)
                    .min(self.limits.first_byte),
        );
        let send = timeout_at(first_deadline, sender.send_request(request));
        let mut send = pin!(send);
        let mut connection = Some(Box::pin(connection));
        // Drive Hyper on the same dedicated runtime thread as the workload.
        let response = poll_fn(|cx| {
            if let Poll::Ready(result) = send.as_mut().poll(cx) {
                return Poll::Ready(
                    result
                        .map_err(|_| Error::ConnectionReadTimeout)?
                        .map_err(Error::from),
                );
            }
            if let Some(conn) = connection.as_mut() {
                if let Poll::Ready(result) = conn.as_mut().poll(cx) {
                    connection = None;
                    if let Err(err) = result {
                        return Poll::Ready(Err(Error::from(err)));
                    }
                    return send.as_mut().poll(cx).map(|result| {
                        result
                            .map_err(|_| Error::ConnectionReadTimeout)?
                            .map_err(Error::from)
                    });
                }
            }
            Poll::Pending
        })
        .await?;
        let io_count = timeout_count.clone();
        let io = Box::new(async move {
            if let Some(conn) = connection {
                timeout_at(deadline, conn).await.map_err(|_| {
                    io_count.record();
                    Error::HttpResponseTimeout
                })??;
            }
            Ok(())
        });
        if response.body().size_hint().lower() > self.limits.response_bytes {
            return Err(Error::HttpResponseBodySize(Some(
                self.limits.response_bytes,
            )));
        }
        let idle = options
            .between_bytes_timeout
            .unwrap_or(self.limits.between_bytes)
            .min(self.limits.between_bytes);
        let response = response.map(|inner| {
            LimitedResponseBody {
                inner,
                total: Box::pin(tokio::time::sleep_until(deadline)),
                idle: Box::pin(tokio::time::sleep(idle)),
                idle_duration: idle,
                remaining: self.limits.response_bytes,
                finished: false,
                timeout_count,
            }
            .boxed_unsync()
        });
        Ok((response, io as IoFuture))
    }
}

impl WasiHttpHooks for PolicyHttpHooks {
    fn send_request(
        &mut self,
        request: Request<WasiBody>,
        options: Option<RequestOptions>,
        completion: IoFuture,
    ) -> SendFuture {
        // WASIp2 supplies an already-completed future here, as in the default
        // transport. Transport ownership is carried by the returned I/O future.
        drop(completion);
        let hooks = self.clone();
        Box::new(async move {
            let count = HttpDeadlineCount {
                counters: hooks.enforcer.counters.clone(),
                recorded: Arc::new(AtomicBool::new(false)),
            };
            let result = hooks.send(request, options, count.clone()).await;
            if matches!(
                &result,
                Err(Error::ConnectionTimeout
                    | Error::ConnectionReadTimeout
                    | Error::HttpResponseTimeout)
            ) {
                count.record();
            }
            result
        })
    }
}

trait HttpStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> HttpStream for T {}

struct ConnectionSlot(PolicyEnforcer);
impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.record_outbound_disconnect();
    }
}

struct PolicyStream {
    stream: TcpStream,
    slot: ConnectionSlot,
}

impl AsyncRead for PolicyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PolicyStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let enforcer = &self.slot.0;
        let limit = enforcer.policy.network.max_egress_bytes;
        let count = if limit == 0 {
            buf.len()
        } else {
            (buf.len() as u64)
                .min(limit.saturating_sub(enforcer.counters.egress_bytes.load(Ordering::Acquire)))
                as usize
        };
        let reserve = count.max(1) as u64;
        if let Err(err) = enforcer.check_and_record_egress(reserve) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("HTTP egress quota: {err:?}"),
            )));
        }
        let result = Pin::new(&mut self.stream).poll_write(cx, &buf[..count]);
        let written = match &result {
            Poll::Ready(Ok(n)) => *n as u64,
            _ => 0,
        };
        self.slot
            .0
            .counters
            .egress_bytes
            .fetch_sub(reserve - written, Ordering::AcqRel);
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct LimitedResponseBody {
    timeout_count: HttpDeadlineCount,
    inner: Incoming,
    total: Pin<Box<Sleep>>,
    idle: Pin<Box<Sleep>>,
    idle_duration: Duration,
    remaining: u64,
    finished: bool,
}

impl Body for LimitedResponseBody {
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let result = if self.total.as_mut().poll(cx).is_ready() {
            self.timeout_count.record();
            Poll::Ready(Some(Err(Error::HttpResponseTimeout)))
        } else if self.idle.as_mut().poll(cx).is_ready() {
            self.timeout_count.record();
            Poll::Ready(Some(Err(Error::ConnectionReadTimeout)))
        } else {
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let bytes = frame.data_ref().map(|data| data.len() as u64).unwrap_or(0);
                    if bytes > self.remaining {
                        Poll::Ready(Some(Err(Error::HttpResponseBodySize(None))))
                    } else {
                        self.remaining -= bytes;
                        let next = Instant::now() + self.idle_duration;
                        self.idle.as_mut().reset(next);
                        Poll::Ready(Some(Ok(frame)))
                    }
                }
                Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err.into()))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        };
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            self.finished = true;
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.finished || self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests;
