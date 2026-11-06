//! Bounded observations from actual node data; missing sources remain explicit.
use super::*;
use axum::extract::Query;
pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Window {
    start_ms: u64,
    end_ms: u64,
}
impl Window {
    fn valid(&self) -> bool {
        self.start_ms < self.end_ms
            && self.end_ms.saturating_sub(self.start_ms) <= 86_400_000
            && self.end_ms <= now().saturating_add(60_000)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct BillingQuery {
    namespace: String,
    start_ms: u64,
    end_ms: u64,
}
pub(super) async fn billing(
    State(state): State<Arc<Control>>,
    headers: HeaderMap,
    Query(q): Query<BillingQuery>,
) -> Response {
    let grant = match state.authorize(&headers, None, Permission::Observe) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let w = Window {
        start_ms: q.start_ms,
        end_ms: q.end_ms,
    };
    if !w.valid() {
        return error(StatusCode::BAD_REQUEST, "invalid_billing_window");
    }
    let apps: BTreeSet<_> = grant
        .applications
        .iter()
        .filter(|a| AppId((*a).clone()).namespace() == q.namespace)
        .cloned()
        .collect();
    if apps.is_empty() {
        return error(StatusCode::FORBIDDEN, "control_access_denied");
    }
    let result = (|| {
        let rows = state
            .dispatcher
            .store
            .scoped_billing_window(&apps, w.start_ms, w.end_ms)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let fuel: u128 = rows.iter().map(|r| u128::from(r.fuel_consumed)).sum();
        let wall: u128 = rows.iter().map(|r| u128::from(r.wall_clock_ms)).sum();
        let ram_time: u128 = rows
            .iter()
            .map(|r| u128::from(r.ram_bytes) * u128::from(r.wall_clock_ms))
            .sum();
        let cost=state.grants.pricing.as_ref().map(|p|{
            let micros=fuel.checked_mul(u128::from(p.fuel_micros_per_million)).and_then(|n|n.checked_div(1_000_000)).and_then(|n|ram_time.checked_mul(u128::from(p.memory_micros_per_gib_second)).and_then(|m|m.checked_div(1_073_741_824_000)).and_then(|m|n.checked_add(m))).ok_or(StatusCode::PAYLOAD_TOO_LARGE)?;
            Ok::<_,StatusCode>(json!({"currency":p.currency,"estimatedCostMicros":u64::try_from(micros).map_err(|_|StatusCode::PAYLOAD_TOO_LARGE)?,"pricingSha256":digest(p)?,"basis":"completed-execution-fuel-and-reserved-ram-time","rounding":"floor-per-resource-to-microcurrency"}))
        }).transpose()?;
        let refs:Vec<_>=rows.iter().take(32).map(|r|json!({"node":r.node_id,"sequence":r.seq,"recordHash":r.record_hash,"app":r.app_id,"timestampMs":r.timestamp_ms,"trap":r.is_trap})).collect();
        Ok::<_, StatusCode>(
            json!({"namespace":q.namespace,"node":state.dispatcher.node_id,"startMs":w.start_ms,"endMs":w.end_ms,"applications":apps,"completedRecords":rows.len(),"fuelConsumed":u64::try_from(fuel).map_err(|_|StatusCode::PAYLOAD_TOO_LARGE)?,"wallClockMs":u64::try_from(wall).map_err(|_|StatusCode::PAYLOAD_TOO_LARGE)?,"reservedRamByteMilliseconds":ram_time.to_string(),"trapCount":rows.iter().filter(|r|r.is_trap).count(),"pricingConfigured":cost.is_some(),"cost":cost,"recordReferences":refs,"referencesTruncated":rows.len()>32,"completeTotals":true,"source":"persisted-billing-records"}),
        )
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_billing_unavailable"),
    }
}
pub(super) async fn storage(
    State(state): State<Arc<Control>>,
    Path(node): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(s) = nodes::authorize(&state, &headers, &node, Permission::NodeObserve) {
        return error(s, "control_access_denied");
    }
    if node != state.dispatcher.node_id {
        return error(StatusCode::NOT_FOUND, "control_node_not_local");
    }
    let result = (|| {
        let path = state.dispatcher.store.db_path();
        let bytes = std::fs::metadata(&path)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .len();
        let available = storage::gc::get_available_disk_space(&path)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let gc = state
            .dispatcher
            .store
            .load_meta("gc-last-observation")
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        Ok::<_, StatusCode>(
            json!({"node":node,"databaseFileBytes":bytes,"availableFilesystemBytes":available,"artifactCount":state.dispatcher.store.count_artifacts().map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?,"garbageCollection":gc,"measuredAtMs":now()}),
        )
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_storage_unavailable"),
    }
}
pub(super) async fn release(State(state): State<Arc<Control>>, headers: HeaderMap) -> Response {
    let grant = match state.authorize(&headers, None, Permission::NodeObserve) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let result = (|| {
        let mut nodes = Vec::new();
        for n in state
            .dispatcher
            .store
            .list_cluster_nodes()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            if grant.nodes.contains(&n.node_id) && n.node_id != state.dispatcher.node_id {
                nodes.push(json!({"node":n.node_id,"binaryVersion":n.binary_version,"protocolVersion":n.protocol_version,"lastSeenUnixSecs":n.last_seen_unix_secs,"stale":n.is_stale(state.dispatcher.cluster_node_stale_after_secs),"provenance":null}));
            }
        }
        if grant.nodes.contains(&state.dispatcher.node_id) {
            nodes.push(json!({"node":state.dispatcher.node_id,"binaryVersion":common::protocol::BINARY_VERSION,"protocolVersion":common::protocol::PROTOCOL_VERSION,"stale":false,"provenance":{"kind":"running-elf-sha256","binarySha256":state.dispatcher.store.load_meta("running-binary-sha256").map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?}}));
        }
        Ok::<_, StatusCode>(
            json!({"nodes":nodes,"sourceNode":state.dispatcher.node_id,"remoteProvenanceVerified":false}),
        )
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_release_unavailable"),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FlowQuery {
    since_ms: u64,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    32
}
pub(super) async fn flows(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    Query(q): Query<FlowQuery>,
) -> Response {
    if let Err(s) = state.authorize(&headers, Some(&app), Permission::Observe) {
        return error(s, "control_access_denied");
    }
    if q.limit == 0 || q.limit > 128 || q.since_ms > now().saturating_add(60_000) {
        return error(StatusCode::BAD_REQUEST, "invalid_flow_query");
    }
    let monitor = state.ebpf_state.snapshot();
    if !monitor.ebpf_active || monitor.monitoring_degraded {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "control_kernel_flow_source_unavailable",
        );
    }
    let mut value = state.ebpf.observed_connections(&app, q.since_ms, q.limit);
    value["app"] = json!(app);
    value["node"] = json!(state.dispatcher.node_id);
    value["ebpfActive"] = json!(true);
    response(value)
}
pub(super) async fn timeline(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    Query(w): Query<Window>,
) -> Response {
    if let Err(s) = state.authorize(&headers, Some(&app), Permission::Observe) {
        return error(s, "control_access_denied");
    }
    if !w.valid() {
        return error(StatusCode::BAD_REQUEST, "invalid_timeline_window");
    }
    let result = (|| {
        let mut events = Vec::new();
        let history = state
            .dispatcher
            .store
            .deployment_history(&app, 256)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut source_truncated = history.len() == 256;
        for r in history {
            if r.recorded_at_ms >= w.start_ms && r.recorded_at_ms < w.end_ms {
                events.push(json!({"timestampMs":r.recorded_at_ms,"kind":"deployment","revision":r.revision,"artifactSha256":r.artifact_sha256,"sequence":r.sequence}));
            }
        }
        let operations = state
            .dispatcher
            .store
            .meta_prefix("control-operation:", 1024)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        source_truncated |= operations.len() == 1024;
        for (_, raw) in operations {
            let r: Record =
                serde_json::from_str(&raw).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            if r.output["app"] == app && r.timestamp_ms >= w.start_ms && r.timestamp_ms < w.end_ms {
                events.push(json!({"timestampMs":r.timestamp_ms,"kind":"control_operation","status":r.status,"receipt":r.output}));
            }
        }
        let apps = [app.clone()].into();
        for r in state
            .dispatcher
            .store
            .scoped_billing_window(&apps, w.start_ms, w.end_ms)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            if r.is_trap || r.status_code >= 500 {
                events.push(json!({"timestampMs":r.timestamp_ms,"kind":"execution_error","trap":r.is_trap,"statusCode":r.status_code,"recordHash":r.record_hash,"instance":r.instance_id}));
            }
        }
        events.sort_by_key(|e| e["timestampMs"].as_u64().unwrap_or(0));
        let truncated = events.len() > 64 || source_truncated;
        events.truncate(64);
        Ok::<_, StatusCode>(
            json!({"app":app,"node":state.dispatcher.node_id,"startMs":w.start_ms,"endMs":w.end_ms,"events":events,"truncated":truncated,"sourceTruncated":source_truncated,"sources":["deployment-history","control-operation-journal","billing-records"],"requiresExternalAlertCorrelation":true}),
        )
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_timeline_unavailable"),
    }
}
