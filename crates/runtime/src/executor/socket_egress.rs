//! Meter P2 socket payloads at the resource boundary. Keep Wasmtime's socket
//! implementation, permissions, and resource ownership; replace only the host
//! calls which expose TCP output streams or send UDP datagrams.

use super::StoreState;
use crate::policy_tracker::PolicyEnforcer;
use bytes::Bytes;
use std::sync::atomic::Ordering;
use wasmtime::component::{Linker, Resource};
use wasmtime_wasi::p2::bindings::sockets::{network::ErrorCode, tcp, udp};
use wasmtime_wasi::p2::{DynOutputStream, OutputStream, Pollable, StreamError, StreamResult};
use wasmtime_wasi::sockets::WasiSocketsView;

// The pinned Wasmtime P2 bindings register 0.2.12. Its linker resolves older
// compatible 0.2 imports to these definitions too. Real guest tests protect
// this boundary against a dependency/version change bypassing the wrappers.
pub(super) fn add_to_linker(linker: &mut Linker<StoreState>) -> wasmtime::Result<()> {
    linker.allow_shadowing(true);
    let result = (|| {
        let mut tcp = linker.instance("wasi:sockets/tcp@0.2.12")?;
        tcp.func_wrap(
            "[method]tcp-socket.finish-connect",
            |mut store, (socket,): (Resource<tcp::TcpSocket>,)| {
                let state = store.data_mut();
                let socket_rep = socket.rep();
                state.io_deadlines.check(socket_rep)?;
                let result = tcp::HostTcpSocket::finish_connect(&mut state.sockets(), socket);
                let result = match result {
                    Ok((input, output)) => {
                        state.policy_enforcer.record_outbound_connect();
                        state.io_deadlines.forget(socket_rep);
                        state.io_deadlines.read_stream(input.rep());
                        wrap_output(state, &output)?;
                        Ok((input, output))
                    }
                    Err(error) => {
                        if error.downcast_ref() != Some(&ErrorCode::WouldBlock) {
                            if state.io_deadlines.connecting(socket_rep)
                                && state.raw_connections.remove(&socket_rep)
                            {
                                state.policy_enforcer.record_outbound_disconnect();
                            }
                            state.io_deadlines.forget(socket_rep);
                        }
                        Err(error.downcast()?)
                    }
                };
                Ok((result,))
            },
        )?;
        tcp.func_wrap(
            "[method]tcp-socket.accept",
            |mut store, (socket,): (Resource<tcp::TcpSocket>,)| {
                let state = store.data_mut();
                let result = tcp::HostTcpSocket::accept(&mut state.sockets(), socket);
                let result = match result {
                    Ok((socket, input, output)) => {
                        state.io_deadlines.read_stream(input.rep());
                        wrap_output(state, &output)?;
                        Ok((socket, input, output))
                    }
                    Err(error) => Err(error.downcast()?),
                };
                Ok((result,))
            },
        )?;
        linker.instance("wasi:sockets/udp@0.2.12")?.func_wrap(
            "[method]outgoing-datagram-stream.send",
            |mut store,
             (stream, datagrams): (
                Resource<udp::OutgoingDatagramStream>,
                Vec<udp::OutgoingDatagram>,
            )| {
                let state = store.data_mut();
                let rep = stream.rep();
                state.io_deadlines.check(rep)?;
                let enforcer = state.policy_enforcer.clone();
                let result = meter_datagrams(&enforcer, datagrams, |datagrams| {
                    udp::HostOutgoingDatagramStream::send(&mut state.sockets(), stream, datagrams)
                });
                if result.as_ref().is_ok_and(|count| *count > 0) {
                    state.io_deadlines.progress(rep);
                }
                let result = match result {
                    Ok(count) => Ok(count),
                    Err(error) => Err(error.downcast()?),
                };
                Ok((result,))
            },
        )?;
        Ok(())
    })();
    linker.allow_shadowing(false);
    result
}

fn meter_datagrams(
    enforcer: &PolicyEnforcer,
    datagrams: Vec<udp::OutgoingDatagram>,
    send: impl FnOnce(Vec<udp::OutgoingDatagram>) -> wasmtime_wasi::p2::SocketResult<u64>,
) -> wasmtime_wasi::p2::SocketResult<u64> {
    // Wasmtime returns the successfully accepted prefix. Reserve the whole
    // batch before queuing sends, then refund only the unsent suffix.
    let lengths: Vec<u64> = datagrams.iter().map(|d| d.data.len() as u64).collect();
    let bytes = lengths
        .iter()
        .try_fold(0u64, |sum, len| sum.checked_add(*len))
        .ok_or_else(|| {
            wasmtime_wasi::p2::SocketError::trap(wasmtime::format_err!(
                "UDP egress batch length overflow"
            ))
        })?;
    enforcer
        .check_and_record_egress(bytes)
        .map_err(|_| ErrorCode::AccessDenied)?;
    let result = send(datagrams);
    let sent = result.as_ref().copied().unwrap_or(0) as usize;
    let charged: u64 = lengths.iter().take(sent).sum();
    enforcer
        .counters
        .egress_bytes
        .fetch_sub(bytes - charged, Ordering::AcqRel);
    result
}

fn wrap_output(
    state: &mut StoreState,
    resource: &Resource<DynOutputStream>,
) -> wasmtime::Result<()> {
    state.io_deadlines.write_stream(resource.rep());
    let output = state.table.get_mut(resource)?;
    // Replace the boxed stream in place: the original handle, parent socket,
    // pollables, and table capacity accounting remain unchanged.
    let inner = std::mem::replace(
        output,
        Box::new(wasmtime_wasi::p2::pipe::ClosedOutputStream),
    );
    *output = Box::new(MeteredTcpOutput {
        inner,
        enforcer: state.policy_enforcer.clone(),
    });
    Ok(())
}

struct MeteredTcpOutput {
    inner: DynOutputStream,
    enforcer: PolicyEnforcer,
}

#[async_trait::async_trait]
impl OutputStream for MeteredTcpOutput {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.enforcer
            .check_and_record_egress(bytes.len() as u64)
            .map_err(|error| StreamError::LastOperationFailed(wasmtime::format_err!("{error}")))?;
        // Do not refund errors: Wasmtime may already have sent a prefix before
        // returning an error, or queued the write on a background task.
        self.inner.write(bytes)
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        self.inner.check_write()
    }

    fn flush(&mut self) -> StreamResult<()> {
        self.inner.flush()
    }

    async fn cancel(&mut self) {
        self.inner.cancel().await;
    }
}

#[async_trait::async_trait]
impl Pollable for MeteredTcpOutput {
    async fn ready(&mut self) {
        self.inner.ready().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::{AppConfig, AppId};
    use wasmtime_wasi::p2::bindings::sync::io::streams::HostOutputStream;
    use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

    fn datagrams() -> Vec<udp::OutgoingDatagram> {
        vec![
            udp::OutgoingDatagram {
                data: vec![1; 3],
                remote_address: None,
            },
            udp::OutgoingDatagram {
                data: vec![2; 5],
                remote_address: None,
            },
        ]
    }

    #[test]
    fn udp_batch_refunds_only_unaccepted_datagrams_including_error_paths() {
        for accepted in [Ok(0), Ok(1), Ok(2), Err(ErrorCode::WouldBlock)] {
            let enforcer = PolicyEnforcer::new(common::policy::InstancePolicy::default());
            let expected = match accepted {
                Ok(1) => 3,
                Ok(2) => 8,
                _ => 0,
            };
            let _ = meter_datagrams(&enforcer, datagrams(), |_| accepted.map_err(Into::into));
            assert_eq!(
                enforcer.counters.egress_bytes.load(Ordering::Acquire),
                expected
            );
        }
        let enforcer = PolicyEnforcer::new(common::policy::InstancePolicy::default());
        let result = meter_datagrams(&enforcer, datagrams(), |_| {
            Err(wasmtime_wasi::p2::SocketError::trap(wasmtime::format_err!(
                "invalid resource"
            )))
        });
        assert!(result.is_err());
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn udp_batch_quota_denial_never_delegates_to_socket() {
        let mut policy = common::policy::InstancePolicy::default();
        policy.network.max_egress_bytes = 4;
        let enforcer = PolicyEnforcer::new(policy);
        let result = meter_datagrams(&enforcer, datagrams(), |_| {
            panic!("denied datagrams must not be sent")
        });
        assert_eq!(
            result.unwrap_err().downcast().unwrap(),
            ErrorCode::AccessDenied
        );
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Acquire), 0);
        assert_eq!(
            enforcer
                .counters
                .egress_denied_total
                .load(Ordering::Acquire),
            1
        );
    }

    #[tokio::test]
    async fn raw_tcp_reservations_and_real_http_wire_writes_share_one_budget() {
        use http_body_util::BodyExt;
        use tokio::io::AsyncReadExt;
        use wasmtime_wasi_http::WasiHttpHooks;
        let mut config = AppConfig::default_for(AppId::new("mixed-meter", "v1"));
        config.policy = Some(common::policy::PolicyConfig {
            network: Some(common::policy::NetworkPolicyConfig {
                max_egress_bytes: Some(6),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut state = super::super::build_store_state(&config, vec![], 8080, None, None).unwrap();
        let pipe = MemoryOutputPipe::new(32);
        let output = state
            .table
            .push(Box::new(pipe.clone()) as DynOutputStream)
            .unwrap();
        wrap_output(&mut state, &output).unwrap();
        state
            .table
            .get_mut(&output)
            .unwrap()
            .blocking_write_and_flush(Bytes::from_static(b"test"))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let request = hyper::Request::builder()
            .uri(format!("http://{addr}/"))
            .body(
                http_body_util::Empty::<Bytes>::new()
                    .map_err(wasmtime_wasi_http::Error::from)
                    .boxed_unsync(),
            )
            .unwrap();
        let result = std::pin::Pin::from(state.http_hooks.send_request(
            request,
            None,
            Box::new(async { Ok(()) }),
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(
            state
                .policy_enforcer
                .counters
                .egress_bytes
                .load(Ordering::Acquire),
            6
        );
        assert_eq!(pipe.contents().as_ref(), b"test");
        let sent = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sent.len(), 2);
    }

    #[tokio::test]
    async fn tcp_stream_wrapper_covers_zeroes_blocking_writes_and_splice_in_place() {
        let config = AppConfig::default_for(AppId::new("meter", "v1"));
        let mut state = super::super::build_store_state(&config, vec![], 8080, None, None).unwrap();
        state.policy_enforcer.policy.network.max_egress_bytes = 8;
        let parent = state.table.push(42u32).unwrap();
        let pipe = MemoryOutputPipe::new(32);
        let output = state
            .table
            .push_child(Box::new(pipe.clone()) as DynOutputStream, &parent)
            .unwrap();
        let handle = output.rep();
        wrap_output(&mut state, &output).unwrap();
        assert_eq!(output.rep(), handle);
        // The wrapper retains the original resource and its parent relationship.
        assert!(state
            .table
            .delete(Resource::<u32>::new_own(parent.rep()))
            .is_err());
        let stream = state.table.get_mut(&output).unwrap();
        stream
            .blocking_write_and_flush(Bytes::from_static(b"ab"))
            .await
            .unwrap();
        stream.write_zeroes(2).unwrap();
        let input = state
            .table
            .push(Box::new(MemoryInputPipe::new("cdef")) as wasmtime_wasi::p2::DynInputStream)
            .unwrap();
        assert_eq!(
            HostOutputStream::splice(&mut state.table, Resource::new_borrow(handle), input, 4)
                .unwrap(),
            4
        );
        let stream = state.table.get_mut(&output).unwrap();
        assert!(matches!(
            stream.write(Bytes::from_static(b"x")),
            Err(StreamError::LastOperationFailed(_))
        ));
        assert_eq!(pipe.contents().as_ref(), b"ab\0\0cdef");
        assert_eq!(
            state
                .policy_enforcer
                .counters
                .egress_bytes
                .load(Ordering::Acquire),
            8
        );
        assert_eq!(
            state
                .policy_enforcer
                .counters
                .egress_denied_total
                .load(Ordering::Acquire),
            1
        );
    }

    #[test]
    fn tcp_write_error_keeps_conservative_charge() {
        let mut policy = common::policy::InstancePolicy::default();
        policy.network.max_egress_bytes = 4;
        let enforcer = PolicyEnforcer::new(policy);
        let mut stream = MeteredTcpOutput {
            inner: Box::new(wasmtime_wasi::p2::pipe::ClosedOutputStream),
            enforcer: enforcer.clone(),
        };
        assert!(stream.write(Bytes::from_static(b"test")).is_err());
        assert!(matches!(
            stream.write(Bytes::from_static(b"test")),
            Err(StreamError::LastOperationFailed(_))
        ));
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Acquire), 4);
    }

    #[test]
    fn concurrent_tcp_writers_share_atomic_egress_ceiling() {
        let mut policy = common::policy::InstancePolicy::default();
        policy.network.max_egress_bytes = 10;
        let enforcer = PolicyEnforcer::new(policy);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let enforcer = enforcer.clone();
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut stream = MeteredTcpOutput {
                        inner: Box::new(wasmtime_wasi::p2::pipe::SinkOutputStream),
                        enforcer,
                    };
                    barrier.wait();
                    let _ = stream.write(Bytes::from_static(b"test"));
                });
            }
        });
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Acquire), 8);
        assert_eq!(
            enforcer
                .counters
                .egress_denied_total
                .load(Ordering::Acquire),
            6
        );
    }
}
