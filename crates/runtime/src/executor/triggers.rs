//! Native-only invocation on the instance's dedicated, eBPF-registered thread.
use super::*;
use common::triggers::ScheduleTrigger;
use http_body_util::{BodyExt, Empty};
use tokio::sync::{mpsc, oneshot, Semaphore};

pub(super) fn denied_response() -> hyper::Response<ManagedOutgoingBody> {
    let body = Empty::<bytes::Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync();
    let (complete, _) = oneshot::channel();
    hyper::Response::builder()
        .status(403)
        .body(ManagedOutgoingBody::new(body, complete))
        .expect("fixed denial response")
}

pub struct Invocation {
    pub declaration: ScheduleTrigger,
    pub scheduled_at_ms: u64,
    pub node: String,
    pub reply: oneshot::Sender<Result<u16, String>>,
}

/// This capability never crosses the HTTP or guest boundary.
#[derive(Clone)]
pub struct TriggerHandle(pub(crate) mpsc::Sender<Invocation>);

impl TriggerHandle {
    pub async fn invoke(
        &self,
        declaration: ScheduleTrigger,
        scheduled_at_ms: u64,
        node: String,
    ) -> Result<u16, String> {
        let (reply, result) = oneshot::channel();
        self.0
            .try_send(Invocation {
                declaration,
                scheduled_at_ms,
                node,
                reply,
            })
            .map_err(|_| "trigger adapter unavailable or saturated".to_string())?;
        result
            .await
            .map_err(|_| "trigger instance stopped".to_string())?
    }
}

pub(super) struct Context {
    pub config: AppConfig,
    pub engine: Arc<Engine>,
    pub pre: Arc<ProxyPre<StoreState>>,
    pub env: Arc<Vec<(String, String)>>,
    pub socket_check: Option<SocketAddrCheckFn>,
    pub counters: Arc<PolicyCounters>,
    pub port: u16,
    pub limits: Vec<Arc<Semaphore>>,
}

pub(super) async fn invoke(context: Arc<Context>, invocation: Invocation) {
    let Invocation {
        declaration,
        scheduled_at_ms,
        node,
        mut reply,
    } = invocation;
    let result = async {
        let index = context
            .config
            .triggers
            .iter()
            .position(|t| t.schedule() == &declaration)
            .ok_or_else(|| {
                "trigger declaration differs from running instance; redeploy required".to_string()
            })?;
        let _permit = context.limits[index]
            .clone()
            .try_acquire_owned()
            .map_err(|_| "trigger concurrency exceeded".to_string())?;
        let deadline = common::triggers::duration(&declaration.timeout)?;
        tokio::time::timeout(
            deadline,
            execute(&context, &declaration, scheduled_at_ms, &node),
        )
        .await
        .map_err(|_| "trigger execution timeout".to_string())?
    };
    tokio::select! {
        biased;
        _ = reply.closed() => {},
        result = result => { let _ = reply.send(result); }
    }
}

async fn execute(
    context: &Context,
    s: &ScheduleTrigger,
    scheduled_at_ms: u64,
    node: &str,
) -> Result<u16, String> {
    let state = build_store_state(
        &context.config,
        context.env.as_ref().clone(),
        context.port,
        context.socket_check.clone(),
        Some(context.counters.clone()),
    )
    .map_err(|e| e.to_string())?;
    let mut store = Store::new(&context.engine, state);
    store.limiter(|s| &mut s.limiter);
    configure_store(&mut store, context.config.fuel_quota).map_err(|e| e.to_string())?;
    let request = hyper::Request::builder()
        .method(s.method.as_str())
        .uri(format!("http://localhost{}", s.route))
        .header(
            "x-trigger-id",
            format!("{}:{}:{}", context.config.id.0, s.id, scheduled_at_ms),
        )
        .header("x-trigger-name", &s.id)
        .header("x-trigger-scheduled-at-ms", scheduled_at_ms.to_string())
        .header("x-trigger-node", node)
        .body(Empty::<bytes::Bytes>::new())
        .map_err(|e| e.to_string())?;
    let request = store
        .data_mut()
        .http()
        .new_incoming_request(Scheme::Http, request)
        .map_err(|e| e.to_string())?;
    let (sender, receiver) = oneshot::channel();
    let out = store
        .data_mut()
        .http()
        .new_response_outparam(sender)
        .map_err(|e| e.to_string())?;
    let proxy = context
        .pre
        .instantiate_async(&mut store)
        .await
        .map_err(|e| e.to_string())?;
    // Drain concurrently: a streaming guest can wait for its response body to be read.
    // Both futures and the Store are dropped on deadline; no detached guest task.
    let handle = async {
        proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, request, out)
            .await
            .map_err(|e| format!("{e:#}"))
    };
    let response = async {
        let response = receiver
            .await
            .map_err(|_| "trigger did not produce a response".to_string())?
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut bytes = 0_usize;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| e.to_string())?;
            if let Some(data) = frame.data_ref() {
                bytes = bytes.saturating_add(data.len());
                if bytes > 1_048_576 {
                    return Err("trigger response exceeds 1 MiB".to_string());
                }
            }
        }
        Ok(status)
    };
    let (_, status) = tokio::try_join!(handle, response)?;
    Ok(status)
}
