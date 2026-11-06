//! Explicit node-wide privileges are separate from application grants. No
//! remote-node control forwarding or mesh identity fallback exists here.
use super::*;
use common::control::valid_operation_key;
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct NodeChange {
    operation_id: String,
    expected_revision: String,
    change: Action,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Cordon { enabled: bool },
    Drain { enabled: bool },
}
pub(super) fn authorize(
    state: &Control,
    headers: &HeaderMap,
    node: &str,
    permission: Permission,
) -> Result<Grant, StatusCode> {
    let grant = state.authorize(headers, None, permission)?;
    if !grant.nodes.contains(node) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(grant)
}
async fn snapshot(state: &Control) -> Result<Value, StatusCode> {
    let d = &state.dispatcher;
    let store = &d.store;
    let cordoned = store
        .load_meta("control-node-cordoned")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .as_deref()
        == Some("true");
    let draining = store
        .load_meta("control-node-draining")
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .as_deref()
        == Some("true");
    let execution = d.supervisor.node_execution_fence().await;
    let mut deployments = std::collections::BTreeMap::new();
    for app in store
        .list_deployed_apps()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    {
        let c = store
            .load_config(&app)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let hash = store
            .get_artifact_sha256(&app)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let desired = store
            .load_meta(&format!("control-desired-instances:{}", app.0))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        deployments.insert(
            app.0,
            digest(&json!({"config":c,"artifact":hash,"desiredInstances":desired}))?,
        );
    }
    let revision = digest(
        &json!({"node":d.node_id,"cordoned":cordoned,"draining":draining,"execution":execution,"deployments":deployments}),
    )?;
    let health = state.health.clone();
    let dependencies = if d.backpressure.is_accepting() {
        tokio::task::spawn_blocking(move || {
            health
                .dependency_checkers
                .iter()
                .map(|c| c.check())
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    } else {
        Vec::new()
    };
    let dependency_status = if dependencies.is_empty() {
        None
    } else {
        Some(proxy::health::compute_status_for_probe(
            &dependencies,
            common::health::ProbeType::Readiness,
        ))
    };
    let health = json!({"status":dependency_status,"dependencies":dependencies.iter().map(|d|json!({"name":d.name,"status":d.status,"latencyMs":d.latency_ms,"lastCheck":d.last_check})).collect::<Vec<_>>(),"startupComplete":state.health.startup_complete.load(std::sync::atomic::Ordering::Relaxed),"uptimeSecs":state.health.started_at.elapsed().as_secs()});
    Ok(
        json!({"node":d.node_id,"revision":revision,"cordoned":cordoned,"draining":draining,"acceptingRequests":d.backpressure.is_accepting(),"health":health,"capacity":{"configuredMemoryBudgetBytes":d.supervisor.configured_memory_budget_bytes()},"activeInstances":execution.values().map(BTreeSet::len).sum::<usize>(),"deployedApplications":deployments.len(),"applications":execution.iter().map(|(app,ids)|json!({"app":app,"instances":ids.len()})).collect::<Vec<_>>(),"binaryVersion":common::protocol::BINARY_VERSION,"protocolVersion":common::protocol::PROTOCOL_VERSION}),
    )
}
pub(super) async fn list(State(state): State<Arc<Control>>, headers: HeaderMap) -> Response {
    let grant = match state.authorize(&headers, None, Permission::NodeObserve) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let result=async{
        let mut nodes=Vec::new();
        for n in state.dispatcher.store.list_cluster_nodes().map_err(|_|StatusCode::SERVICE_UNAVAILABLE)? {
            if grant.nodes.contains(&n.node_id) && n.node_id!=state.dispatcher.node_id {nodes.push(json!({"node":n.node_id,"lastSeenUnixSecs":n.last_seen_unix_secs,"health":n.health_status,"stale":n.is_stale(state.dispatcher.cluster_node_stale_after_secs),"activeInstances":n.active_instances,"deployedApplications":n.deployed_apps,"acceptingRequests":n.accepting_requests,"binaryVersion":n.binary_version,"protocolVersion":n.protocol_version}));}
        }
        if grant.nodes.contains(&state.dispatcher.node_id){nodes.push(snapshot(&state).await?);}
        Ok::<_,StatusCode>(json!({"nodes":nodes,"sourceNode":state.dispatcher.node_id}))
    }.await;
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_node_unavailable"),
    }
}
pub(super) async fn get(
    State(state): State<Arc<Control>>,
    Path(node): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(s) = authorize(&state, &headers, &node, Permission::NodeObserve) {
        return error(s, "control_access_denied");
    }
    if node != state.dispatcher.node_id {
        return error(StatusCode::NOT_FOUND, "control_node_not_local");
    }
    match snapshot(&state).await {
        Ok(v) => response(v),
        Err(s) => error(s, "control_node_unavailable"),
    }
}
pub(super) async fn change(
    State(state): State<Arc<Control>>,
    Path(node): Path<String>,
    headers: HeaderMap,
    input: Result<Json<NodeChange>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let grant = match authorize(&state, &headers, &node, Permission::NodeManage) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    if node != state.dispatcher.node_id {
        return error(StatusCode::NOT_FOUND, "control_node_not_local");
    }
    let r = match input {
        Ok(Json(r))
            if valid_operation_key(&r.operation_id)
                && r.expected_revision.len() == 64
                && r.expected_revision.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            r
        }
        _ => return error(StatusCode::BAD_REQUEST, "invalid_node_change"),
    };
    let _lane = match state.gate.try_lock() {
        Ok(g) => g,
        Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "control_busy"),
    };
    let _lifecycle = state.dispatcher.lifecycle_gate.lock().await;
    let result=async{
        let key=format!("control-node-operation:{}",digest(&json!({"principal":grant.principal,"node":node,"operationId":r.operation_id})).map_err(|s|(s,"control_unavailable"))?);
        let sha=digest(&r).map_err(|s|(s,"control_unavailable"))?;
        if let Some(raw)=state.dispatcher.store.load_meta(&key).map_err(|_|(StatusCode::SERVICE_UNAVAILABLE,"control_journal_unavailable"))? {
            let saved:Record=serde_json::from_str(&raw).map_err(|_|(StatusCode::SERVICE_UNAVAILABLE,"control_journal_unavailable"))?;
            if saved.request_sha256!=sha{return Err((StatusCode::CONFLICT,"control_identity_conflict"));}
            return if saved.status=="succeeded"{Ok(saved.output)}else{Err((StatusCode::CONFLICT,"control_outcome_unknown"))};
        }
        if snapshot(&state).await.map_err(|s|(s,"control_node_unavailable"))?["revision"]!=r.expected_revision{return Err((StatusCode::CONFLICT,"control_precondition_changed"));}
        let execution=state.dispatcher.supervisor.node_execution_fence().await;
        let mut record=Record{request_sha256:sha,status:"dispatched".into(),output:Value::Null,permission:Some(Permission::NodeManage),timestamp_ms:observations::now()};
        let persist=|r:&Record|state.dispatcher.store.save_meta(&key,&serde_json::to_string(r).map_err(|_|(StatusCode::INTERNAL_SERVER_ERROR,"control_journal_unavailable"))?).map_err(|_|(StatusCode::SERVICE_UNAVAILABLE,"control_journal_unavailable"));
        persist(&record)?;
        let(draining,cordoned)=match r.change{Action::Drain{enabled}=>(Some(enabled),None),Action::Cordon{enabled}=>(None,Some(enabled))};
        if !state.dispatcher.supervisor.change_node_admission_if(&execution,draining,cordoned,&state.dispatcher.backpressure).await.map_err(|_|(StatusCode::CONFLICT,"control_outcome_unknown"))? {return Err((StatusCode::CONFLICT,"control_precondition_changed"));}
        record.status="succeeded".into();record.output=json!({"node":node,"operationId":r.operation_id,"expectedRevision":r.expected_revision,"applied":true,"state":snapshot(&state).await.map_err(|s|(s,"control_outcome_unknown"))?});persist(&record)?;
        audit_control(&state,&grant.principal,"",&r.operation_id,"node_admission_change");
        Ok(record.output)
    }.await;
    match result {
        Ok(v) => response(v),
        Err((s, c)) => error(s, c),
    }
}
pub(super) async fn operation(
    State(state): State<Arc<Control>>,
    Path((node, operation)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let grant = match authorize(&state, &headers, &node, Permission::NodeManage) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    if node != state.dispatcher.node_id {
        return error(StatusCode::NOT_FOUND, "control_node_not_local");
    }
    if !valid_operation_key(&operation) {
        return error(StatusCode::BAD_REQUEST, "invalid_operation");
    }
    let result = (|| {
        let key = format!(
            "control-node-operation:{}",
            digest(&json!({"principal":grant.principal,"node":node,"operationId":operation}))?
        );
        let raw = state
            .dispatcher
            .store
            .load_meta(&key)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .ok_or(StatusCode::NOT_FOUND)?;
        let r: Record = serde_json::from_str(&raw).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if r.status != "succeeded" {
            return Err(StatusCode::CONFLICT);
        }
        Ok(r.output)
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_node_receipt_unavailable"),
    }
}
