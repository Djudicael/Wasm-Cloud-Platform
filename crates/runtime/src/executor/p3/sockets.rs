//! P3 outbound TCP uses owned resources and guarded native streams. UDP and
//! inbound raw sockets are deliberately unavailable on this hosting path.
use super::*;
use crate::policy_tracker::PolicyEnforcer;
use bytes::BytesMut;
use std::sync::atomic::Ordering;

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::time::{Instant as TokioInstant, Sleep};
use wasmtime::component::{
    Destination, FutureReader, Resource, ResourceType, Source, StreamConsumer, StreamProducer,
    StreamReader, StreamResult,
};
use wasmtime::StoreContextMut;
use wasmtime_wasi::p3::bindings::sockets::types::{
    ErrorCode, IpAddressFamily, IpSocketAddress, TcpSocket, UdpSocket,
};

struct Connection {
    io: TcpStream,
    enforcer: PolicyEnforcer,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.enforcer.record_outbound_disconnect();
    }
}
struct Socket {
    family: IpAddressFamily,
    attempted: bool,
    stream: Option<Arc<Connection>>,
    reservation: Option<PolicyEnforcer>,
    sending: bool,
    receiving: bool,
}
impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(enforcer) = self.reservation.take() {
            enforcer.record_outbound_disconnect();
        }
    }
}
fn handle(socket: &Resource<TcpSocket>) -> Resource<Socket> {
    Resource::new_borrow(socket.rep())
}

pub(super) fn add_to_linker(linker: &mut Linker<StoreState>) -> wasmtime::Result<()> {
    linker.allow_shadowing(true);
    let result = (|| {
        let mut types = linker.instance("wasi:sockets/types@0.3.0")?;
        types.func_wrap(
            "[static]tcp-socket.create",
            |mut store, (family,): (IpAddressFamily,)| {
                let socket = store.data_mut().table.push(Socket {
                    family,
                    attempted: false,
                    stream: None,
                    reservation: None,
                    sending: false,
                    receiving: false,
                })?;
                Ok((Ok::<_, ErrorCode>(Resource::<TcpSocket>::new_own(
                    socket.rep(),
                )),))
            },
        )?;
        types.resource(
            "tcp-socket",
            ResourceType::host::<TcpSocket>(),
            |mut store, rep| {
                store
                    .data_mut()
                    .table
                    .delete(Resource::<Socket>::new_own(rep))?;
                Ok(())
            },
        )?;
        types.func_wrap_concurrent(
            "[method]tcp-socket.connect",
            |accessor, (socket, address): (Resource<TcpSocket>, IpSocketAddress)| {
                Box::pin(async move {
                    let address = SocketAddr::from(address);
                    let deadline = TokioInstant::now() + crate::limits::RAW_SOCKET_CONNECT_TIMEOUT;
                    let (check, enforcer) = accessor.with(|mut store| {
                        let state = store.data_mut();
                        let resource = state.table.get_mut(&handle(&socket))?;
                        wasmtime::error::ensure!(
                            !resource.attempted,
                            "P3 socket already connected or connecting"
                        );
                        resource.attempted = true;
                        Ok::<_, wasmtime::Error>((
                            state.p3_socket_check.clone(),
                            state.policy_enforcer.clone(),
                        ))
                    })?;
                    let work = async {
                        let allowed = match tokio::time::timeout_at(
                            deadline,
                            (check)(address, SocketAddrUse::TcpConnect),
                        )
                        .await
                        {
                            Ok(allowed) => allowed,
                            Err(_) => {
                                enforcer
                                    .counters
                                    .io_connect_timeouts
                                    .fetch_add(1, Ordering::Relaxed);
                                return Ok(Err(ErrorCode::Timeout));
                            }
                        };
                        if !allowed {
                            return Ok::<_, wasmtime::Error>(Err(ErrorCode::AccessDenied));
                        }
                        if enforcer
                            .check_outbound_tcp_connect(address.ip(), address.port())
                            .is_err()
                        {
                            return Ok(Err(ErrorCode::AccessDenied));
                        }
                        // Resource ownership releases the slot on failure, cancellation or drop.
                        accessor.with(|mut store| {
                            store
                                .data_mut()
                                .table
                                .get_mut(&handle(&socket))
                                .map(|r| r.reservation = Some(enforcer.clone()))
                        })?;
                        let result =
                            tokio::time::timeout_at(deadline, TcpStream::connect(address)).await;
                        let stream = match result {
                            Ok(Ok(stream)) => stream,
                            other => {
                                if other.is_err() {
                                    enforcer
                                        .counters
                                        .io_connect_timeouts
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                accessor.with(|mut store| {
                                    store.data_mut().table.get_mut(&handle(&socket)).map(|r| {
                                        r.reservation.take();
                                    })
                                })?;
                                enforcer.record_outbound_disconnect();
                                return Ok(Err(if other.is_err() {
                                    ErrorCode::Timeout
                                } else {
                                    ErrorCode::ConnectionRefused
                                }));
                            }
                        };
                        if let (Ok(local), Ok(remote)) = (stream.local_addr(), stream.peer_addr()) {
                            enforcer
                                .counters
                                .track_host_tcp_close(local.port(), remote.port());
                        }
                        accessor.with(|mut store| {
                            store.data_mut().table.get_mut(&handle(&socket)).map(|r| {
                                r.reservation.take();
                                r.stream = Some(Arc::new(Connection {
                                    io: stream,
                                    enforcer: enforcer.clone(),
                                }));
                            })
                        })?;
                        enforcer.record_outbound_connect();
                        Ok(Ok(()))
                    };
                    Ok((work.await?,))
                })
            },
        )?;
        types.func_wrap(
            "[method]tcp-socket.get-address-family",
            |mut store, (s,): (Resource<TcpSocket>,)| {
                Ok((store.data_mut().table.get(&handle(&s))?.family,))
            },
        )?;
        for (name, remote) in [
            ("[method]tcp-socket.get-local-address", false),
            ("[method]tcp-socket.get-remote-address", true),
        ] {
            types.func_wrap(name, move |mut store, (s,): (Resource<TcpSocket>,)| {
                let r = store.data_mut().table.get(&handle(&s))?;
                let result = r
                    .stream
                    .as_ref()
                    .ok_or(ErrorCode::InvalidState)
                    .and_then(|stream| {
                        (if remote {
                            stream.io.peer_addr()
                        } else {
                            stream.io.local_addr()
                        })
                        .map(IpSocketAddress::from)
                        .map_err(|_| ErrorCode::InvalidState)
                    });
                Ok((result,))
            })?;
        }
        types.func_wrap(
            "[method]tcp-socket.send",
            |mut store, (s, data): (Resource<TcpSocket>, StreamReader<u8>)| {
                let state = store.data_mut();
                let enforcer = state.policy_enforcer.clone();
                let socket = state.table.get_mut(&handle(&s))?;
                wasmtime::error::ensure!(!socket.sending, "P3 TCP send stream already taken");
                let stream = socket
                    .stream
                    .clone()
                    .ok_or_else(|| wasmtime::format_err!("P3 TCP socket not connected"))?;
                socket.sending = true;
                let (tx, rx) = oneshot::channel();
                data.pipe(
                    &mut store,
                    Send {
                        stream,
                        enforcer,
                        result: Some(tx),
                        deadline: None,
                    },
                )?;
                Ok((FutureReader::new(&mut store, async move {
                    wasmtime::error::Ok(rx.await.unwrap_or(Err(ErrorCode::ConnectionAborted)))
                })?,))
            },
        )?;
        types.func_wrap(
            "[method]tcp-socket.receive",
            |mut store, (s,): (Resource<TcpSocket>,)| {
                let state = store.data_mut();
                let counters = state.policy_enforcer.counters.clone();
                let socket = state.table.get_mut(&handle(&s))?;
                wasmtime::error::ensure!(!socket.receiving, "P3 TCP receive stream already taken");
                let stream = socket
                    .stream
                    .clone()
                    .ok_or_else(|| wasmtime::format_err!("P3 TCP socket not connected"))?;
                socket.receiving = true;
                let (tx, rx) = oneshot::channel();
                let stream = StreamReader::new(
                    &mut store,
                    Receive {
                        stream,
                        counters,
                        result: Some(tx),
                        deadline: None,
                    },
                )?;
                let result = FutureReader::new(&mut store, async move {
                    wasmtime::error::Ok(rx.await.unwrap_or(Err(ErrorCode::ConnectionAborted)))
                })?;
                Ok(((stream, result),))
            },
        )?;
        // Fail closed instead of exposing the unmetered upstream UDP implementation.
        types.func_wrap_concurrent(
            "[static]udp-socket.create",
            |_, (_family,): (IpAddressFamily,)| {
                Box::pin(async { Ok((Err::<Resource<UdpSocket>, _>(ErrorCode::AccessDenied),)) })
            },
        )?;
        types.func_wrap_concurrent(
            "[method]tcp-socket.bind",
            |_, (_socket, _address): (Resource<TcpSocket>, IpSocketAddress)| {
                Box::pin(async { Ok((Err::<(), _>(ErrorCode::AccessDenied),)) })
            },
        )?;
        Ok(())
    })();
    linker.allow_shadowing(false);
    result
}

type Sender = oneshot::Sender<Result<(), ErrorCode>>;
struct Send {
    stream: Arc<Connection>,
    enforcer: PolicyEnforcer,
    result: Option<Sender>,
    deadline: Option<Pin<Box<Sleep>>>,
}
struct Receive {
    stream: Arc<Connection>,
    counters: Arc<PolicyCounters>,
    result: Option<Sender>,
    deadline: Option<Pin<Box<Sleep>>>,
}
fn close(result: &mut Option<Sender>, error: Result<(), ErrorCode>) {
    if let Some(tx) = result.take() {
        let _ = tx.send(error);
    }
}
fn expired(deadline: &mut Option<Pin<Box<Sleep>>>, cx: &mut Context<'_>) -> bool {
    deadline
        .get_or_insert_with(|| {
            Box::pin(tokio::time::sleep_until(
                TokioInstant::now() + crate::limits::RAW_SOCKET_IO_TIMEOUT,
            ))
        })
        .as_mut()
        .poll(cx)
        .is_ready()
}
impl Drop for Send {
    fn drop(&mut self) {
        close(&mut self.result, Ok(()));
    }
}
impl Drop for Receive {
    fn drop(&mut self) {
        close(&mut self.result, Ok(()));
    }
}
impl StreamConsumer<StoreState> for Send {
    type Item = u8;
    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<StoreState>,
        src: Source<u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if self.result.is_none() {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if expired(&mut self.deadline, cx) {
            self.enforcer
                .counters
                .io_write_timeouts
                .fetch_add(1, Ordering::Relaxed);
            close(&mut self.result, Err(ErrorCode::Timeout));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let mut src = src.as_direct(store);
        match self.stream.io.poll_write_ready(cx) {
            Poll::Pending => {
                return Poll::Ready(Ok(if finish {
                    StreamResult::Cancelled
                } else {
                    return Poll::Pending;
                }))
            }
            Poll::Ready(Err(_)) => {
                close(&mut self.result, Err(ErrorCode::ConnectionAborted));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            _ => {}
        }
        let bytes = src.remaining();
        if bytes.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let len = bytes.len().min(64 * 1024);
        if self.enforcer.check_and_record_egress(len as u64).is_err() {
            close(&mut self.result, Err(ErrorCode::AccessDenied));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let result = self.stream.io.try_write(&bytes[..len]);
        let sent = result.as_ref().copied().unwrap_or(0);
        self.enforcer
            .counters
            .egress_bytes
            .fetch_sub((len - sent) as u64, Ordering::AcqRel);
        match result {
            Ok(n) => {
                src.mark_read(n);
                self.deadline = None;
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(_) => {
                close(&mut self.result, Err(ErrorCode::ConnectionAborted));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}
impl StreamProducer<StoreState> for Receive {
    type Item = u8;
    type Buffer = BytesMut;
    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, StoreState>,
        dst: Destination<'a, u8, BytesMut>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if self.result.is_none() {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if expired(&mut self.deadline, cx) {
            self.counters
                .io_read_timeouts
                .fetch_add(1, Ordering::Relaxed);
            close(&mut self.result, Err(ErrorCode::Timeout));
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if dst.remaining(&mut store) == Some(0) {
            return self
                .stream
                .io
                .poll_read_ready(cx)
                .map(|_| Ok(StreamResult::Completed));
        }
        match self.stream.io.poll_read_ready(cx) {
            Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_)) => {
                close(&mut self.result, Err(ErrorCode::ConnectionAborted));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            _ => {}
        }
        let mut dst = dst.as_direct(store, 64 * 1024);
        match self.stream.io.try_read(dst.remaining()) {
            Ok(0) => {
                close(&mut self.result, Ok(()));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Ok(n) => {
                dst.mark_written(n);
                self.deadline = None;
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(_) => {
                close(&mut self.result, Err(ErrorCode::ConnectionAborted));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}
