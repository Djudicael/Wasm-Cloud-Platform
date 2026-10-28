//! Host-side pending-I/O deadlines. Listening sockets, clocks, files, and
//! standard I/O are not network waits. Network watches survive cancellation
//! of poll futures, so polling a clock alongside a socket cannot renew them.
use super::StoreState;
use crate::policy_tracker::PolicyCounters;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};
use wasmtime::component::{Linker, Resource, ResourceType};
use wasmtime_wasi::p2::bindings::{
    io::{poll, streams},
    sockets::{ip_name_lookup as lookup, network, tcp, udp},
};
use wasmtime_wasi::p2::{DynInputStream, DynOutputStream, DynPollable};
use wasmtime_wasi::sockets::WasiSocketsView;

#[derive(Clone, Copy, Debug)]
enum Operation {
    Connect,
    Read,
    Write,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Debug)]
struct Watch {
    operation: Operation,
    deadline: Option<Instant>,
}

#[derive(Debug)]
pub(super) struct SocketDeadlines {
    resources: HashMap<u32, Watch>,
    pollables: HashMap<u32, u32>,
    counters: Arc<PolicyCounters>,
    tripped: Option<Operation>,
    connect: Duration,
    io: Duration,
}

impl SocketDeadlines {
    pub(super) fn tripped(&self) -> bool {
        self.tripped.is_some()
    }
    pub(super) fn connecting(&self, rep: u32) -> bool {
        self.resources
            .get(&rep)
            .is_some_and(|watch| matches!(watch.operation, Operation::Connect))
    }
    pub(super) fn new(counters: Arc<PolicyCounters>) -> Self {
        Self {
            resources: HashMap::new(),
            pollables: HashMap::new(),
            counters,
            tripped: None,
            connect: crate::limits::RAW_SOCKET_CONNECT_TIMEOUT,
            io: crate::limits::RAW_SOCKET_IO_TIMEOUT,
        }
    }
    pub(super) fn read_stream(&mut self, rep: u32) {
        self.track(rep, Operation::Read);
    }
    pub(super) fn write_stream(&mut self, rep: u32) {
        self.track(rep, Operation::Write);
    }
    fn track(&mut self, rep: u32, operation: Operation) {
        self.resources.insert(
            rep,
            Watch {
                operation,
                deadline: None,
            },
        );
    }
    pub(super) fn forget(&mut self, rep: u32) {
        self.resources.remove(&rep);
        self.pollables.retain(|_, parent| *parent != rep);
    }
    fn subscribe(&mut self, child: u32, parent: u32) {
        self.pollables.remove(&child);
        if self.resources.contains_key(&parent) {
            self.pollables.insert(child, parent);
        }
    }
    pub(super) fn progress(&mut self, rep: u32) {
        if let Some(watch) = self.resources.get_mut(&rep) {
            watch.deadline = None;
        }
    }
    fn expired(&mut self, op: Operation) -> wasmtime::Error {
        if self.tripped.is_none() {
            self.tripped = Some(op);
            let counter = match op {
                Operation::Connect => &self.counters.io_connect_timeouts,
                Operation::Read => &self.counters.io_read_timeouts,
                Operation::Write => &self.counters.io_write_timeouts,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                operation = op.name(),
                "WASI socket host I/O deadline exceeded"
            );
        }
        wasmtime::format_err!("WASI socket host {} deadline exceeded", op.name())
    }
    pub(super) fn check(&mut self, _rep: u32) -> wasmtime::Result<()> {
        if let Some(op) = self.tripped {
            return Err(self.expired(op));
        }
        if let Some((due, operation)) = self.earliest() {
            if Instant::now() >= due {
                return Err(self.expired(operation));
            }
        }
        Ok(())
    }
    fn arm(&mut self, rep: u32) -> wasmtime::Result<Option<(Instant, Operation)>> {
        self.check(rep)?;
        if let Some(watch) = self.resources.get_mut(&rep) {
            let duration = match watch.operation {
                Operation::Connect => self.connect,
                _ => self.io,
            };
            watch
                .deadline
                .get_or_insert_with(|| Instant::now() + duration);
        }
        Ok(self.earliest())
    }
    fn earliest(&self) -> Option<(Instant, Operation)> {
        self.resources
            .values()
            .filter_map(|watch| watch.deadline.map(|due| (due, watch.operation)))
            .min_by_key(|(due, _)| *due)
    }
    fn ready_pollable(&mut self, rep: u32) {
        if let Some(parent) = self.pollables.get(&rep).copied() {
            self.progress(parent);
        }
    }
    fn poll_deadline(
        &mut self,
        reps: impl IntoIterator<Item = u32>,
    ) -> wasmtime::Result<Option<(Instant, Operation)>> {
        self.check(0)?;
        for child in reps {
            if let Some(parent) = self.pollables.get(&child).copied() {
                self.arm(parent)?;
            }
        }
        Ok(self.earliest())
    }
}

async fn bounded<T>(
    future: impl Future<Output = wasmtime::Result<T>>,
    deadline: Option<(Instant, Operation)>,
) -> Result<T, DeadlineError> {
    match deadline {
        Some((due, op)) => {
            if Instant::now() >= due {
                return Err(DeadlineError::Timeout(op));
            }
            let result = timeout_at(due, future).await;
            // Tokio polls a ready inner future before its timer. A late ready
            // result must not defeat the absolute deadline after a stall.
            if Instant::now() >= due {
                return Err(DeadlineError::Timeout(op));
            }
            result
                .map_err(|_| DeadlineError::Timeout(op))?
                .map_err(DeadlineError::Host)
        }
        None => future.await.map_err(DeadlineError::Host),
    }
}
enum DeadlineError {
    Timeout(Operation),
    Host(wasmtime::Error),
}
fn finish<T>(state: &mut StoreState, result: Result<T, DeadlineError>) -> wasmtime::Result<T> {
    result.map_err(|error| match error {
        DeadlineError::Timeout(op) => state.io_deadlines.expired(op),
        DeadlineError::Host(err) => err,
    })
}

async fn guarded_poll(
    state: &mut StoreState,
    pollables: Vec<Resource<DynPollable>>,
) -> wasmtime::Result<Vec<u32>> {
    let due = state
        .io_deadlines
        .poll_deadline(pollables.iter().map(Resource::rep))?;
    let reps: Vec<_> = pollables.iter().map(Resource::rep).collect();
    let result = bounded(poll::Host::poll(&mut state.table, pollables), due).await;
    let ready = finish(state, result)?;
    for index in &ready {
        state.io_deadlines.ready_pollable(reps[*index as usize]);
    }
    Ok(ready)
}
async fn guarded_block(
    state: &mut StoreState,
    pollable: Resource<DynPollable>,
) -> wasmtime::Result<()> {
    let due = state.io_deadlines.poll_deadline([pollable.rep()])?;
    let rep = pollable.rep();
    let result = bounded(poll::HostPollable::block(&mut state.table, pollable), due).await;
    finish(state, result)?;
    state.io_deadlines.ready_pollable(rep);
    Ok(())
}
async fn guarded_ready(
    state: &mut StoreState,
    pollable: Resource<DynPollable>,
) -> wasmtime::Result<bool> {
    state.io_deadlines.poll_deadline([pollable.rep()])?;
    let rep = pollable.rep();
    let ready = poll::HostPollable::ready(&mut state.table, pollable).await?;
    state.io_deadlines.check(rep)?;
    if ready {
        state.io_deadlines.ready_pollable(rep);
    }
    Ok(ready)
}

macro_rules! async_host {
    ($linker:expr, $async_io:expr, $interface:expr, $name:expr, |$state:ident, $params:tt : $ty:ty| $body:expr) => {{
        if $async_io {
            $linker
                .instance($interface)?
                .func_wrap_async($name, |mut store, $params: $ty| {
                    Box::new(async move {
                        let $state = store.data_mut();
                        $body
                    })
                })?;
        } else {
            $linker
                .instance($interface)?
                .func_wrap($name, |mut store, $params: $ty| {
                    let $state = store.data_mut();
                    wasmtime_wasi::runtime::in_tokio(async { $body })
                })?;
        }
    }};
}

// Resource destructors are linker resource definitions, not imported funcs
// named `[resource-drop]...`. Preserve the Rust host type when replacing them.
macro_rules! async_drop {
    ($linker:expr, $async_io:expr, $interface:expr, $name:expr, $ty:ty, |$state:ident, $resource:ident| $body:expr) => {{
        if $async_io {
            $linker.instance($interface)?.resource_async(
                $name,
                ResourceType::host::<$ty>(),
                |mut store, rep| {
                    Box::new(async move {
                        let $state = store.data_mut();
                        let $resource = Resource::<$ty>::new_own(rep);
                        $body
                    })
                },
            )?;
        } else {
            $linker.instance($interface)?.resource(
                $name,
                ResourceType::host::<$ty>(),
                |mut store, rep| {
                    let $state = store.data_mut();
                    let $resource = Resource::<$ty>::new_own(rep);
                    wasmtime_wasi::runtime::in_tokio(async { $body })
                },
            )?;
        }
    }};
}

pub(super) fn add_to_linker(
    linker: &mut Linker<StoreState>,
    async_io: bool,
) -> wasmtime::Result<()> {
    linker.allow_shadowing(true);
    let result = register(linker, async_io);
    linker.allow_shadowing(false);
    result
}
fn register(linker: &mut Linker<StoreState>, async_io: bool) -> wasmtime::Result<()> {
    const POLL: &str = "wasi:io/poll@0.2.12";
    const STREAMS: &str = "wasi:io/streams@0.2.12";
    const TCP: &str = "wasi:sockets/tcp@0.2.12";
    const UDP: &str = "wasi:sockets/udp@0.2.12";
    async_host!(linker, async_io, POLL, "poll", |state,
                                                 (pollables,): (
        Vec<Resource<DynPollable>>,
    )| {
        Ok((guarded_poll(state, pollables).await?,))
    });
    async_host!(
        linker,
        async_io,
        POLL,
        "[method]pollable.block",
        |state, (pollable,): (Resource<DynPollable>,)| guarded_block(state, pollable).await
    );
    async_host!(
        linker,
        async_io,
        POLL,
        "[method]pollable.ready",
        |state, (pollable,): (Resource<DynPollable>,)| {
            Ok((guarded_ready(state, pollable).await?,))
        }
    );
    linker.instance(POLL)?.resource(
        "pollable",
        ResourceType::host::<DynPollable>(),
        |mut store, rep| {
            let state = store.data_mut();
            let pollable = Resource::<DynPollable>::new_own(rep);
            poll::HostPollable::drop(&mut state.table, pollable)?;
            state.io_deadlines.pollables.remove(&rep);
            Ok(())
        },
    )?;

    linker.instance(TCP)?.func_wrap(
        "[method]tcp-socket.start-connect",
        |mut store,
         (socket, network, address): (
            Resource<tcp::TcpSocket>,
            Resource<network::Network>,
            network::IpSocketAddress,
        )| {
            let state = store.data_mut();
            let rep = socket.rep();
            state.io_deadlines.check(rep)?;
            let destination: std::net::SocketAddr = address.into();
            if state
                .policy_enforcer
                .check_outbound_tcp_connect(destination.ip(), destination.port())
                .is_err()
            {
                return Ok((Err(network::ErrorCode::AccessDenied),));
            }
            let result =
                tcp::HostTcpSocket::start_connect(&mut state.sockets(), socket, network, address);
            if result.is_ok() {
                state.raw_connections.insert(rep);
                state.io_deadlines.track(rep, Operation::Connect);
                state.io_deadlines.arm(rep)?;
            } else {
                state.policy_enforcer.record_outbound_disconnect();
            }
            Ok((socket_result(result)?,))
        },
    )?;
    linker.instance(TCP)?.func_wrap(
        "[method]tcp-socket.subscribe",
        |mut store, (socket,): (Resource<tcp::TcpSocket>,)| {
            let state = store.data_mut();
            let rep = socket.rep();
            let child = tcp::HostTcpSocket::subscribe(&mut state.sockets(), socket)?;
            state.io_deadlines.subscribe(child.rep(), rep);
            Ok((child,))
        },
    )?;
    linker.instance(TCP)?.resource(
        "tcp-socket",
        ResourceType::host::<tcp::TcpSocket>(),
        |mut store, rep| {
            let state = store.data_mut();
            let socket = Resource::<tcp::TcpSocket>::new_own(rep);
            tcp::HostTcpSocket::drop(&mut state.sockets(), socket)?;
            if state.raw_connections.remove(&rep) {
                state.policy_enforcer.record_outbound_disconnect();
            }
            state.io_deadlines.forget(rep);
            Ok(())
        },
    )?;

    register_streams(linker, async_io, STREAMS)?;
    register_udp(linker, async_io, UDP)?;
    register_lookup(linker)?;
    Ok(())
}

fn register_lookup(linker: &mut Linker<StoreState>) -> wasmtime::Result<()> {
    const LOOKUP: &str = "wasi:sockets/ip-name-lookup@0.2.12";
    linker.instance(LOOKUP)?.func_wrap(
        "resolve-addresses",
        |mut store, (network, name): (Resource<network::Network>, String)| {
            let state = store.data_mut();
            let result = lookup::Host::resolve_addresses(&mut state.sockets(), network, name);
            if let Ok(resource) = &result {
                state.io_deadlines.track(resource.rep(), Operation::Connect);
                state.io_deadlines.arm(resource.rep())?;
            }
            Ok((socket_result(result)?,))
        },
    )?;
    linker.instance(LOOKUP)?.func_wrap(
        "[method]resolve-address-stream.resolve-next-address",
        |mut store, (resource,): (Resource<lookup::ResolveAddressStream>,)| {
            let state = store.data_mut();
            let rep = resource.rep();
            state.io_deadlines.check(rep)?;
            let result = lookup::HostResolveAddressStream::resolve_next_address(
                &mut state.sockets(),
                resource,
            );
            if !result
                .as_ref()
                .is_err_and(|error| error.downcast_ref() == Some(&network::ErrorCode::WouldBlock))
            {
                state.io_deadlines.forget(rep);
            }
            Ok((socket_result(result)?,))
        },
    )?;
    linker.instance(LOOKUP)?.func_wrap(
        "[method]resolve-address-stream.subscribe",
        |mut store, (resource,): (Resource<lookup::ResolveAddressStream>,)| {
            let state = store.data_mut();
            let rep = resource.rep();
            let child =
                lookup::HostResolveAddressStream::subscribe(&mut state.sockets(), resource)?;
            state.io_deadlines.subscribe(child.rep(), rep);
            Ok((child,))
        },
    )?;
    linker.instance(LOOKUP)?.resource(
        "resolve-address-stream",
        ResourceType::host::<lookup::ResolveAddressStream>(),
        |mut store, rep| {
            let state = store.data_mut();
            lookup::HostResolveAddressStream::drop(&mut state.sockets(), Resource::new_own(rep))?;
            state.io_deadlines.forget(rep);
            Ok(())
        },
    )?;
    Ok(())
}

fn socket_result<T>(
    result: wasmtime_wasi::p2::SocketResult<T>,
) -> wasmtime::Result<Result<T, network::ErrorCode>> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) => Ok(Err(error.downcast()?)),
    }
}
fn stream_result<T>(
    table: &mut wasmtime::component::ResourceTable,
    result: wasmtime_wasi::p2::StreamResult<T>,
) -> wasmtime::Result<Result<T, streams::StreamError>> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) => Ok(Err(streams::Host::convert_stream_error(table, error)?)),
    }
}

fn register_streams(
    linker: &mut Linker<StoreState>,
    async_io: bool,
    interface: &str,
) -> wasmtime::Result<()> {
    linker.instance(interface)?.func_wrap(
        "[method]input-stream.subscribe",
        |mut store, (stream,): (Resource<DynInputStream>,)| {
            let state = store.data_mut();
            let rep = stream.rep();
            let child = streams::HostInputStream::subscribe(&mut state.table, stream)?;
            state.io_deadlines.subscribe(child.rep(), rep);
            Ok((child,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]output-stream.subscribe",
        |mut store, (stream,): (Resource<DynOutputStream>,)| {
            let state = store.data_mut();
            let rep = stream.rep();
            let child = streams::HostOutputStream::subscribe(&mut state.table, stream)?;
            state.io_deadlines.subscribe(child.rep(), rep);
            Ok((child,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]input-stream.read",
        |mut store, (stream, len): (Resource<DynInputStream>, u64)| {
            let state = store.data_mut();
            let rep = stream.rep();
            state.io_deadlines.check(rep)?;
            let result = streams::HostInputStream::read(&mut state.table, stream, len);
            if result.as_ref().is_ok_and(|bytes| !bytes.is_empty()) {
                state.io_deadlines.progress(rep);
            }
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]input-stream.skip",
        |mut store, (stream, len): (Resource<DynInputStream>, u64)| {
            let state = store.data_mut();
            let rep = stream.rep();
            state.io_deadlines.check(rep)?;
            let result = streams::HostInputStream::skip(&mut state.table, stream, len);
            if result.as_ref().is_ok_and(|count| *count > 0) {
                state.io_deadlines.progress(rep);
            }
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]output-stream.check-write",
        |mut store, (stream,): (Resource<DynOutputStream>,)| {
            let state = store.data_mut();
            let rep = stream.rep();
            state.io_deadlines.check(rep)?;
            let result = streams::HostOutputStream::check_write(&mut state.table, stream);
            if result.as_ref().is_ok_and(|count| *count > 0) {
                state.io_deadlines.progress(rep);
            }
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]output-stream.write",
        |mut store, (stream, bytes): (Resource<DynOutputStream>, Vec<u8>)| {
            let state = store.data_mut();
            let rep = stream.rep();
            state.io_deadlines.check(rep)?;
            let result = streams::HostOutputStream::write(&mut state.table, stream, bytes);
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]output-stream.write-zeroes",
        |mut store, (stream, len): (Resource<DynOutputStream>, u64)| {
            let state = store.data_mut();
            state.io_deadlines.check(stream.rep())?;
            let result = streams::HostOutputStream::write_zeroes(&mut state.table, stream, len);
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]output-stream.flush",
        |mut store, (stream,): (Resource<DynOutputStream>,)| {
            let state = store.data_mut();
            state.io_deadlines.check(stream.rep())?;
            let result = streams::HostOutputStream::flush(&mut state.table, stream);
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]output-stream.splice",
        |mut store,
         (dest, src, len): (Resource<DynOutputStream>, Resource<DynInputStream>, u64)| {
            let state = store.data_mut();
            let source = src.rep();
            let target = dest.rep();
            state.io_deadlines.check(source)?;
            state.io_deadlines.check(target)?;
            let result = streams::HostOutputStream::splice(&mut state.table, dest, src, len);
            if result.as_ref().is_ok_and(|count| *count > 0) {
                state.io_deadlines.progress(source);
                state.io_deadlines.progress(target);
            }
            Ok((stream_result(&mut state.table, result)?,))
        },
    )?;

    async_host!(
        linker,
        async_io,
        interface,
        "[method]input-stream.blocking-read",
        |state, (stream, len): (Resource<DynInputStream>, u64)| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                async {
                    let result =
                        streams::HostInputStream::blocking_read(&mut state.table, stream, len)
                            .await;
                    stream_result(&mut state.table, result)
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if result.as_ref().is_ok_and(|bytes| !bytes.is_empty()) {
                state.io_deadlines.progress(rep);
            }
            Ok((result,))
        }
    );
    async_host!(
        linker,
        async_io,
        interface,
        "[method]input-stream.blocking-skip",
        |state, (stream, len): (Resource<DynInputStream>, u64)| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                async {
                    let result =
                        streams::HostInputStream::blocking_skip(&mut state.table, stream, len)
                            .await;
                    stream_result(&mut state.table, result)
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if result.as_ref().is_ok_and(|count| *count > 0) {
                state.io_deadlines.progress(rep);
            }
            Ok((result,))
        }
    );
    async_host!(
        linker,
        async_io,
        interface,
        "[method]output-stream.blocking-write-and-flush",
        |state, (stream, bytes): (Resource<DynOutputStream>, Vec<u8>)| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                async {
                    let result = streams::HostOutputStream::blocking_write_and_flush(
                        &mut state.table,
                        stream,
                        bytes,
                    )
                    .await;
                    stream_result(&mut state.table, result)
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if result.is_ok() {
                state.io_deadlines.progress(rep);
            }
            Ok((result,))
        }
    );
    async_host!(
        linker,
        async_io,
        interface,
        "[method]output-stream.blocking-write-zeroes-and-flush",
        |state, (stream, len): (Resource<DynOutputStream>, u64)| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                async {
                    let result = streams::HostOutputStream::blocking_write_zeroes_and_flush(
                        &mut state.table,
                        stream,
                        len,
                    )
                    .await;
                    stream_result(&mut state.table, result)
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if result.is_ok() {
                state.io_deadlines.progress(rep);
            }
            Ok((result,))
        }
    );
    async_host!(
        linker,
        async_io,
        interface,
        "[method]output-stream.blocking-flush",
        |state, (stream,): (Resource<DynOutputStream>,)| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                async {
                    let result =
                        streams::HostOutputStream::blocking_flush(&mut state.table, stream).await;
                    stream_result(&mut state.table, result)
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if result.is_ok() {
                state.io_deadlines.progress(rep);
            }
            Ok((result,))
        }
    );
    async_host!(
        linker,
        async_io,
        interface,
        "[method]output-stream.blocking-splice",
        |state, (dest, src, len): (Resource<DynOutputStream>, Resource<DynInputStream>, u64)| {
            let source = src.rep();
            let target = dest.rep();
            let due = [
                state.io_deadlines.arm(source)?,
                state.io_deadlines.arm(target)?,
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(due, _)| *due);
            let result = bounded(
                async {
                    let result = streams::HostOutputStream::blocking_splice(
                        &mut state.table,
                        dest,
                        src,
                        len,
                    )
                    .await;
                    stream_result(&mut state.table, result)
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if result.as_ref().is_ok_and(|count| *count > 0) {
                state.io_deadlines.progress(source);
                state.io_deadlines.progress(target);
            }
            Ok((result,))
        }
    );
    async_drop!(
        linker,
        async_io,
        interface,
        "input-stream",
        DynInputStream,
        |state, stream| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                streams::HostInputStream::drop(&mut state.table, stream),
                due,
            )
            .await;
            finish(state, result)?;
            state.io_deadlines.forget(rep);
            Ok(())
        }
    );
    async_drop!(
        linker,
        async_io,
        interface,
        "output-stream",
        DynOutputStream,
        |state, stream| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                streams::HostOutputStream::drop(&mut state.table, stream),
                due,
            )
            .await;
            finish(state, result)?;
            state.io_deadlines.forget(rep);
            Ok(())
        }
    );
    Ok(())
}

fn register_udp(
    linker: &mut Linker<StoreState>,
    async_io: bool,
    interface: &str,
) -> wasmtime::Result<()> {
    async_host!(
        linker,
        async_io,
        interface,
        "[method]udp-socket.stream",
        |state, (socket, address): (Resource<udp::UdpSocket>, Option<network::IpSocketAddress>)| {
            let due = Some((
                Instant::now() + state.io_deadlines.connect,
                Operation::Connect,
            ));
            let result = bounded(
                async {
                    socket_result(
                        udp::HostUdpSocket::stream(&mut state.sockets(), socket, address).await,
                    )
                },
                due,
            )
            .await;
            let result = finish(state, result)?;
            if let Ok((input, output)) = &result {
                state.io_deadlines.read_stream(input.rep());
                state.io_deadlines.write_stream(output.rep());
            }
            Ok((result,))
        }
    );
    linker.instance(interface)?.func_wrap(
        "[method]incoming-datagram-stream.subscribe",
        |mut store, (stream,): (Resource<udp::IncomingDatagramStream>,)| {
            let state = store.data_mut();
            let parent = stream.rep();
            let child = udp::HostIncomingDatagramStream::subscribe(&mut state.sockets(), stream)?;
            state.io_deadlines.subscribe(child.rep(), parent);
            Ok((child,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]outgoing-datagram-stream.subscribe",
        |mut store, (stream,): (Resource<udp::OutgoingDatagramStream>,)| {
            let state = store.data_mut();
            let parent = stream.rep();
            let child = udp::HostOutgoingDatagramStream::subscribe(&mut state.sockets(), stream)?;
            state.io_deadlines.subscribe(child.rep(), parent);
            Ok((child,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]incoming-datagram-stream.receive",
        |mut store, (stream, max): (Resource<udp::IncomingDatagramStream>, u64)| {
            let state = store.data_mut();
            let rep = stream.rep();
            state.io_deadlines.check(rep)?;
            let result =
                udp::HostIncomingDatagramStream::receive(&mut state.sockets(), stream, max);
            if result.as_ref().is_ok_and(|datagrams| !datagrams.is_empty()) {
                state.io_deadlines.progress(rep);
            }
            Ok((socket_result(result)?,))
        },
    )?;
    linker.instance(interface)?.func_wrap(
        "[method]outgoing-datagram-stream.check-send",
        |mut store, (stream,): (Resource<udp::OutgoingDatagramStream>,)| {
            let state = store.data_mut();
            let rep = stream.rep();
            state.io_deadlines.check(rep)?;
            let result = udp::HostOutgoingDatagramStream::check_send(&mut state.sockets(), stream);
            if result.as_ref().is_ok_and(|count| *count > 0) {
                state.io_deadlines.progress(rep);
            }
            Ok((socket_result(result)?,))
        },
    )?;
    linker.instance(interface)?.resource(
        "incoming-datagram-stream",
        ResourceType::host::<udp::IncomingDatagramStream>(),
        |mut store, rep| {
            let state = store.data_mut();
            let stream = Resource::<udp::IncomingDatagramStream>::new_own(rep);
            udp::HostIncomingDatagramStream::drop(&mut state.sockets(), stream)?;
            state.io_deadlines.forget(rep);
            Ok(())
        },
    )?;
    async_drop!(
        linker,
        async_io,
        interface,
        "outgoing-datagram-stream",
        udp::OutgoingDatagramStream,
        |state, stream| {
            let rep = stream.rep();
            let due = state.io_deadlines.arm(rep)?;
            let result = bounded(
                udp::HostOutgoingDatagramStream::drop(&mut state.sockets(), stream),
                due,
            )
            .await;
            finish(state, result)?;
            state.io_deadlines.forget(rep);
            Ok(())
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::{AppConfig, AppId};
    use wasmtime_wasi::p2::{InputStream, OutputStream, Pollable, StreamResult};

    struct Never;
    #[async_trait::async_trait]
    impl Pollable for Never {
        async fn ready(&mut self) {
            std::future::pending::<()>().await;
        }
    }
    struct Ready;
    #[async_trait::async_trait]
    impl Pollable for Ready {
        async fn ready(&mut self) {}
    }
    struct BlockedInput;
    #[async_trait::async_trait]
    impl Pollable for BlockedInput {
        async fn ready(&mut self) {
            std::future::pending::<()>().await;
        }
    }
    #[async_trait::async_trait]
    impl InputStream for BlockedInput {
        fn read(&mut self, _: usize) -> StreamResult<bytes::Bytes> {
            Ok(bytes::Bytes::new())
        }
    }
    struct BlockedOutput;
    #[async_trait::async_trait]
    impl Pollable for BlockedOutput {
        async fn ready(&mut self) {
            std::future::pending::<()>().await;
        }
    }
    #[async_trait::async_trait]
    impl OutputStream for BlockedOutput {
        fn check_write(&mut self) -> StreamResult<usize> {
            Ok(0)
        }
        fn write(&mut self, _: bytes::Bytes) -> StreamResult<()> {
            panic!("not writable")
        }
        fn flush(&mut self) -> StreamResult<()> {
            Ok(())
        }
    }
    fn state() -> StoreState {
        let config = AppConfig::default_for(AppId::new("deadlines", "v1"));
        let mut state = super::super::build_store_state(&config, vec![], 8080, None, None).unwrap();
        state.io_deadlines.io = Duration::from_millis(15);
        state.io_deadlines.connect = Duration::from_millis(15);
        state
    }
    fn never(state: &mut StoreState, operation: Operation) -> Resource<DynPollable> {
        let parent = state.table.push(Never).unwrap();
        let child = wasmtime_wasi::p2::subscribe(
            &mut state.table,
            Resource::<Never>::new_borrow(parent.rep()),
        )
        .unwrap();
        state.io_deadlines.track(parent.rep(), operation);
        state.io_deadlines.subscribe(child.rep(), parent.rep());
        child
    }

    #[tokio::test]
    async fn stalled_connect_poll_and_block_trap_once_without_epoch_ticks() {
        for block in [false, true] {
            let mut state = state();
            let child = never(&mut state, Operation::Connect);
            let result = if block {
                guarded_block(&mut state, child).await
            } else {
                guarded_poll(&mut state, vec![child]).await.map(|_| ())
            };
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("connect deadline exceeded"));
            assert!(state.io_deadlines.check(999).is_err());
            assert_eq!(
                state
                    .policy_enforcer
                    .counters
                    .io_connect_timeouts
                    .load(Ordering::Acquire),
                1
            );
        }
    }

    #[tokio::test]
    async fn mixed_clock_polling_cannot_extend_pending_read_deadline() {
        let mut state = state();
        let child = never(&mut state, Operation::Read);
        let ready = state.table.push(Ready).unwrap();
        let clock = wasmtime_wasi::p2::subscribe(&mut state.table, ready).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let result = guarded_poll(
                    &mut state,
                    vec![
                        Resource::new_borrow(child.rep()),
                        Resource::new_borrow(clock.rep()),
                    ],
                )
                .await;
                if result.is_err() {
                    break;
                }
                assert_eq!(result.unwrap(), vec![1]);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            state
                .policy_enforcer
                .counters
                .io_read_timeouts
                .load(Ordering::Acquire),
            1
        );
    }

    #[tokio::test]
    async fn idle_listener_or_clock_has_no_socket_deadline() {
        let mut state = state();
        let parent = state.table.push(Never).unwrap();
        let child = wasmtime_wasi::p2::subscribe(&mut state.table, parent).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(35), guarded_block(&mut state, child))
                .await
                .is_err()
        );
        assert!(!state.io_deadlines.tripped());
    }

    #[tokio::test]
    async fn pending_connect_cannot_hide_behind_an_unrelated_clock_wait() {
        let mut state = state();
        let socket = never(&mut state, Operation::Connect);
        assert!(!guarded_ready(&mut state, socket).await.unwrap());
        let clock = state.table.push(Never).unwrap();
        let child = wasmtime_wasi::p2::subscribe(&mut state.table, clock).unwrap();
        assert!(guarded_block(&mut state, child)
            .await
            .unwrap_err()
            .to_string()
            .contains("connect deadline exceeded"));
        assert_eq!(
            state
                .policy_enforcer
                .counters
                .io_connect_timeouts
                .load(Ordering::Acquire),
            1
        );
    }

    #[tokio::test]
    async fn direct_blocking_stream_read_and_write_are_bounded() {
        for read in [true, false] {
            let mut state = state();
            let result = if read {
                let resource = state
                    .table
                    .push(Box::new(BlockedInput) as DynInputStream)
                    .unwrap();
                state.io_deadlines.read_stream(resource.rep());
                let due = state.io_deadlines.arm(resource.rep()).unwrap();
                bounded(
                    async {
                        let result =
                            streams::HostInputStream::blocking_read(&mut state.table, resource, 1)
                                .await;
                        stream_result(&mut state.table, result)
                    },
                    due,
                )
                .await
                .map(|_| ())
            } else {
                let resource = state
                    .table
                    .push(Box::new(BlockedOutput) as DynOutputStream)
                    .unwrap();
                state.io_deadlines.write_stream(resource.rep());
                let due = state.io_deadlines.arm(resource.rep()).unwrap();
                bounded(
                    async {
                        let result = streams::HostOutputStream::blocking_write_and_flush(
                            &mut state.table,
                            resource,
                            vec![1],
                        )
                        .await;
                        stream_result(&mut state.table, result)
                    },
                    due,
                )
                .await
                .map(|_| ())
            };
            assert!(finish(&mut state, result).is_err());
            let counters = state.policy_enforcer.counters.clone();
            assert_eq!(
                counters.io_read_timeouts.load(Ordering::Acquire),
                u64::from(read)
            );
            assert_eq!(
                counters.io_write_timeouts.load(Ordering::Acquire),
                u64::from(!read)
            );
        }
    }

    #[tokio::test]
    async fn ready_results_cannot_bypass_an_already_expired_absolute_deadline() {
        let result = bounded(
            async { Ok(()) },
            Some((Instant::now() - Duration::from_millis(1), Operation::Read)),
        )
        .await;
        assert!(matches!(
            result,
            Err(DeadlineError::Timeout(Operation::Read))
        ));
    }

    #[tokio::test]
    async fn completed_connect_readiness_stops_its_pending_deadline() {
        let mut state = state();
        let parent = state.table.push(Ready).unwrap();
        let child = wasmtime_wasi::p2::subscribe(
            &mut state.table,
            Resource::<Ready>::new_borrow(parent.rep()),
        )
        .unwrap();
        state.io_deadlines.track(parent.rep(), Operation::Connect);
        state.io_deadlines.subscribe(child.rep(), parent.rep());
        guarded_block(&mut state, child).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        state.io_deadlines.check(parent.rep()).unwrap();
        assert!(state.io_deadlines.connecting(parent.rep()));
    }
}
