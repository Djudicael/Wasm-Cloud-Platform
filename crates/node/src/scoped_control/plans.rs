//! Persisted proposals contain no execution authority. Apply checks both the
//! effect-specific grant and the exact current execution/resource fence.
use super::*;
use axum::extract::rejection::JsonRejection;
use common::{
    control::{Apply, Change, PlanRequest},
    types::AppConfig,
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Plan {
    app: String,
    node: String,
    creator: String,
    expected: Fence,
    change: Change,
    change_sha256: String,
    expires_at_ms: u64,
    circuit: Option<Value>,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
// Existing public AppConfig accepts legacy unknown keys. The scoped proposal
// boundary is stricter: no ignored nested field may be approved and dropped.
fn known_fields(raw: &Value, typed: &Value) -> bool {
    match (raw, typed) {
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .all(|(k, v)| b.get(k).is_some_and(|t| known_fields(v, t))),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| known_fields(a, b))
        }
        _ => true,
    }
}

fn plan_key(app: &str, hash: &str) -> Result<String, StatusCode> {
    Ok(format!("control-plan:{}:{hash}", digest(&app)?))
}
fn read_plan(state: &Control, app: &str, hash: &str) -> Result<Plan, StatusCode> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let raw = state
        .dispatcher
        .store
        .load_meta(&plan_key(app, hash)?)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let plan: Plan = serde_json::from_str(&raw).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if digest(&plan)? != hash || plan.app != app || plan.node != state.dispatcher.node_id {
        return Err(StatusCode::CONFLICT);
    }
    Ok(plan)
}
fn projection(plan: &Plan, hash: &str) -> Result<Value, StatusCode> {
    let mut change =
        serde_json::to_value(&plan.change).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Change::Deploy {
        config: Some(c), ..
    } = &plan.change
    {
        change["config"] = details::config_projection(c);
    }
    if let Change::Gateway { config: c } = &plan.change {
        let raw = serde_json::to_value(c).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let safe: serde_json::Map<_, _> = [
            "auth",
            "cors",
            "rate_limit",
            "circuit_breaker",
            "allowed_callers",
        ]
        .into_iter()
        .filter_map(|k| raw.get(k).map(|v| (k.into(), v.clone())))
        .collect();
        change["config"] = Value::Object(safe);
    }
    Ok(
        json!({"app":plan.app,"node":plan.node,"planDigest":hash,"expected":plan.expected,"change":change,"changeSha256":plan.change_sha256,"expiresAtMs":plan.expires_at_ms,"executionAuthorized":false}),
    )
}
pub(super) fn effect_grant(
    state: &Control,
    headers: &HeaderMap,
    app: &str,
) -> Result<Grant, StatusCode> {
    for permission in [
        Permission::Restart,
        Permission::Configure,
        Permission::Deploy,
    ] {
        match state.authorize(headers, Some(app), permission) {
            Ok(g) => return Ok(g),
            Err(StatusCode::UNAUTHORIZED) => return Err(StatusCode::UNAUTHORIZED),
            Err(_) => {}
        }
    }
    Err(StatusCode::FORBIDDEN)
}
pub(super) async fn create(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    input: Result<Json<Value>, JsonRejection>,
) -> Response {
    let grant = match state
        .authorize(&headers, Some(&app), Permission::Observe)
        .or_else(|_| effect_grant(&state, &headers, &app))
    {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let raw = match input {
        Ok(Json(v)) => v,
        _ => return error(StatusCode::BAD_REQUEST, "invalid_control_plan"),
    };
    let request: PlanRequest = match serde_json::from_value(raw.clone()) {
        Ok(r) => r,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_control_plan"),
    };
    if !request.expected.valid()
        || !request.change.valid()
        || !serde_json::to_value(&request).is_ok_and(|typed| known_fields(&raw, &typed))
    {
        return error(StatusCode::BAD_REQUEST, "invalid_control_plan");
    }
    let _lane = match state.gate.try_lock() {
        Ok(g) => g,
        Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "control_busy"),
    };
    let _lifecycle = state.dispatcher.lifecycle_gate.lock().await;
    let result = async {
        if state.snapshot(&app).await?["fence"] != json!(request.expected) {
            return Err(StatusCode::CONFLICT);
        }
        validate_change(&state, &grant, &app, &request.change)?;
        let plan = Plan {
            app: app.clone(),
            node: state.dispatcher.node_id.clone(),
            creator: grant.principal,
            expected: request.expected,
            change: request.change,
            change_sha256: digest(&raw["change"])?,
            expires_at_ms: now().saturating_add(300_000),
            circuit: state
                .dispatcher
                .gateway
                .as_ref()
                .map(|g| g.circuit_breaker.snapshot(&app)),
        };
        let hash = digest(&plan)?;
        let key = plan_key(&app, &hash)?;
        let prefix = format!("control-plan:{}:", digest(&app)?);
        let mut live = 0;
        for (k, v) in state
            .dispatcher
            .store
            .meta_prefix(&prefix, 65)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            let old: Plan =
                serde_json::from_str(&v).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            if old.expires_at_ms < now() {
                state
                    .dispatcher
                    .store
                    .delete_meta(&k)
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            } else {
                live += 1;
            }
        }
        if live >= 64 {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        state
            .dispatcher
            .store
            .save_meta(
                &key,
                &serde_json::to_string(&plan).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            )
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        projection(&plan, &hash)
    }
    .await;
    match result {
        Ok(v) => response(v),
        Err(StatusCode::CONFLICT) => error(StatusCode::CONFLICT, "control_precondition_changed"),
        Err(s) => error(s, "control_plan_rejected"),
    }
}
pub(super) async fn get(
    State(state): State<Arc<Control>>,
    Path((app, hash)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let grant = match state
        .authorize(&headers, Some(&app), Permission::Observe)
        .or_else(|_| effect_grant(&state, &headers, &app))
    {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let result = (|| {
        let p = read_plan(&state, &app, &hash)?;
        if !grant.permissions.contains(&Permission::Observe)
            && !grant.permissions.contains(&p.change.permission())
        {
            return Err(StatusCode::FORBIDDEN);
        }
        projection(&p, &hash)
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_plan_unavailable"),
    }
}
fn configuration(
    state: &Control,
    grant: &Grant,
    app: &str,
    change: &Change,
) -> Result<AppConfig, StatusCode> {
    let id = AppId(app.into());
    let existing = state
        .dispatcher
        .store
        .load_config(&id)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut c = match change {
        Change::Deploy {
            config: Some(c), ..
        } => c.as_ref().clone(),
        Change::Rollback { revision } => {
            let (mut c, _) = details::resolve_revision(state, grant, app, revision)?;
            c.id = id.clone();
            c.namespace = id.namespace().into();
            c
        }
        _ => existing.clone().ok_or(StatusCode::NOT_FOUND)?,
    };
    match change {
        Change::Configure { key, value } => match key.as_str() {
            "fuel_quota" => c.fuel_quota.0 = *value,
            "memory_pages" => c.memory_limit.0 = *value as u32,
            "max_instances" => c.max_instances = *value as u32,
            "idle_timeout_secs" => c.idle_timeout_secs = *value,
            _ => return Err(StatusCode::BAD_REQUEST),
        },
        Change::Scale { replicas } if *replicas > 0 => c.max_instances = *replicas,
        _ => {}
    }
    if c.id != id || c.namespace != id.namespace() || !common::control::valid_application(&c.id.0) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if c.fuel_quota.0 == 0 || c.memory_limit.0 == 0 || c.max_instances == 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let permitted: BTreeSet<_> = existing
        .iter()
        .flat_map(|c| c.secret_keys.iter().cloned())
        .chain(grant.secret_keys.iter().cloned())
        .collect();
    if c.secret_keys.iter().any(|key| !permitted.contains(key)) {
        return Err(StatusCode::FORBIDDEN);
    }
    let permitted_dependencies: BTreeSet<_> = existing
        .iter()
        .flat_map(|c| c.local_dependencies.iter().map(|a| a.0.clone()))
        .chain(grant.applications.iter().cloned())
        .collect();
    if c.local_dependencies
        .iter()
        .any(|a| !permitted_dependencies.contains(&a.0))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    state
        .dispatcher
        .supervisor
        .check_resource_limits(&c)
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    state
        .dispatcher
        .validate_local_placement(&c)
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    Ok(c)
}
fn validate_change(
    state: &Control,
    grant: &Grant,
    app: &str,
    change: &Change,
) -> Result<(), StatusCode> {
    if !change.valid() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let c = configuration(state, grant, app, change)?;
    match change {
        Change::Deploy {
            artifact_sha256, ..
        } => {
            if !state
                .dispatcher
                .store
                .raw_wasm_exists(artifact_sha256)
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            {
                return Err(StatusCode::NOT_FOUND);
            }
        }
        Change::Rollback { revision } => {
            let (_, hash) = details::resolve_revision(state, grant, app, revision)?;
            if !state
                .dispatcher
                .store
                .raw_wasm_exists(&hash)
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            {
                return Err(StatusCode::NOT_FOUND);
            }
        }
        Change::Route { host, .. } => {
            if !grant.route_hosts.contains(host) {
                return Err(StatusCode::FORBIDDEN);
            }
            if state
                .dispatcher
                .store
                .load_route(host)
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                .is_some_and(|r| r.app_id.0 != app)
            {
                return Err(StatusCode::CONFLICT);
            }
        }
        Change::Trigger { id, .. } => {
            if !c.triggers.iter().any(|t| t.schedule().id == *id) {
                return Err(StatusCode::NOT_FOUND);
            }
        }
        Change::Gateway { config } => {
            // Distributed limiters have their own operator-managed NATS KV
            // lifecycle. A scoped configuration must not silently promise a
            // new distributed limiter or leave its old immutable rate active.
            if config.rate_limit.as_ref().is_some_and(|r| r.distributed)
                || config
                    .endpoints
                    .iter()
                    .any(|e| e.rate_limit.as_ref().is_some_and(|r| r.distributed))
            {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            if config.circuit_breaker.as_ref().is_some_and(|c| {
                c.failure_threshold == 0
                    || c.failure_threshold > 1000
                    || c.reset_timeout_secs == 0
                    || c.reset_timeout_secs > 86400
            }) {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        Change::GatewayReset if state.dispatcher.gateway.is_none() => {
            return Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        _ => {}
    }
    Ok(())
}
pub(super) async fn apply(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    input: Result<Json<Apply>, JsonRejection>,
) -> Response {
    let grant = match effect_grant(&state, &headers, &app) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let request = match input {
        Ok(Json(r)) if common::control::valid_operation_key(&r.operation_id) => r,
        _ => return error(StatusCode::BAD_REQUEST, "invalid_control_apply"),
    };
    let _lane = match state.gate.try_lock() {
        Ok(g) => g,
        Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "control_busy"),
    };
    let _lifecycle = state.dispatcher.lifecycle_gate.lock().await;
    let result=async{
        let key=format!("control-operation:{}",digest(&json!({"principal":grant.principal,"app":app,"operationId":request.operation_id})).map_err(|s|(s,"control_unavailable"))?);
        let sha=digest(&request).map_err(|s|(s,"control_unavailable"))?;
        if let Some(raw)=state.dispatcher.store.load_meta(&key).map_err(|_|(StatusCode::SERVICE_UNAVAILABLE,"control_journal_unavailable"))? {
            let record:Record=serde_json::from_str(&raw).map_err(|_|(StatusCode::SERVICE_UNAVAILABLE,"control_journal_unavailable"))?;
            if !grant.permissions.contains(&record.permission.unwrap_or(Permission::Restart)){return Err((StatusCode::FORBIDDEN,"control_access_denied"));}
            if record.request_sha256!=sha{return Err((StatusCode::CONFLICT,"control_identity_conflict"));}
            return if record.status=="succeeded" {Ok(record.output)}else{Err((StatusCode::CONFLICT,"control_outcome_unknown"))};
        }
        let plan=read_plan(&state,&app,&request.plan_digest).map_err(|s|(s,"control_plan_unavailable"))?;
        if !grant.permissions.contains(&plan.change.permission()){return Err((StatusCode::FORBIDDEN,"control_access_denied"));}
        if plan.expires_at_ms<now(){return Err((StatusCode::CONFLICT,"control_plan_expired"));}
        if state.snapshot(&app).await.map_err(|s|(s,"control_snapshot_unavailable"))?["fence"]!=json!(plan.expected){return Err((StatusCode::CONFLICT,"control_precondition_changed"));}
        validate_change(&state,&grant,&app,&plan.change).map_err(|s|(s,"control_plan_rejected"))?;
        let mut record=Record{request_sha256:sha,status:"dispatched".into(),output:Value::Null,permission:Some(plan.change.permission()),timestamp_ms:observations::now()};
        let persist=|r:&Record|->Result<(),(StatusCode,&'static str)>{state.dispatcher.store.save_meta(&key,&serde_json::to_string(r).map_err(|_|(StatusCode::INTERNAL_SERVER_ERROR,"control_journal_unavailable"))?).map_err(|_|(StatusCode::SERVICE_UNAVAILABLE,"control_journal_unavailable"))};
        persist(&record)?;
        execute(&state,&grant,&app,&plan).await.map_err(|_|(StatusCode::CONFLICT,"control_outcome_unknown"))?;
        record.status="succeeded".into();
        record.output=json!({"app":app,"node":state.dispatcher.node_id,"operationId":request.operation_id,"planDigest":request.plan_digest,"changeSha256":plan.change_sha256,"applied":true,"resultingFence":state.snapshot(&app).await.map_err(|s|(s,"control_outcome_unknown"))?["fence"]});
        persist(&record)?;
        audit_control(&state,&grant.principal,&app,&request.operation_id,"apply_exact_plan");
        Ok(record.output)
    }.await;
    match result {
        Ok(v) => response(v),
        Err((s, code)) => error(s, code),
    }
}
async fn execute(state: &Control, grant: &Grant, app: &str, plan: &Plan) -> Result<(), StatusCode> {
    let id = AppId(app.into());
    let store = &state.dispatcher.store;
    match &plan.change {
        Change::Configure { .. }
        | Change::Scale { .. }
        | Change::Deploy { .. }
        | Change::Rollback { .. } => {
            let c = configuration(state, grant, app, &plan.change)?;
            let artifact = match &plan.change {
                Change::Deploy {
                    artifact_sha256, ..
                } => Some(artifact_sha256.clone()),
                Change::Rollback { revision } => {
                    Some(details::resolve_revision(state, grant, app, revision)?.1)
                }
                _ => None,
            };
            let compiled = if let Some(hash) = artifact {
                let bytes = store
                    .load_raw_wasm(&hash)
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                    .ok_or(StatusCode::NOT_FOUND)?;
                if bytes.len() > 64 * 1024 * 1024 || hex::encode(Sha256::digest(&bytes)) != hash {
                    return Err(StatusCode::UNPROCESSABLE_ENTITY);
                }
                let runtime = state.dispatcher.runtime.clone();
                let compiled = tokio::task::spawn_blocking(move || runtime.compile(&bytes))
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                    .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
                Some((compiled, hash))
            } else {
                None
            };
            if !state
                .dispatcher
                .supervisor
                .replace_execution_if(
                    &id,
                    &plan.expected.instances,
                    c.clone(),
                    compiled,
                    if let Change::Scale { replicas } = &plan.change {
                        Some(*replicas)
                    } else {
                        None
                    },
                )
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            {
                return Err(StatusCode::CONFLICT);
            }
            state.dispatcher.apply_rate_limit(&id, &c);
            if let Change::Scale { replicas } = &plan.change {
                state
                    .dispatcher
                    .supervisor
                    .reconcile_desired_instances(&id, *replicas)
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            }
        }
        Change::Route {
            host,
            path_prefix,
            strip_prefix,
        } => {
            let old = store
                .load_route(host)
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let route = common::types::Route {
                host: host.clone(),
                app_id: id,
                path_prefix: path_prefix.clone(),
                strip_prefix: *strip_prefix,
                created_at: old.as_ref().map(|r| r.created_at).unwrap_or(now() / 1000),
                updated_at: now() / 1000,
            };
            if let Some(old) = old {
                if old.path_prefix != *path_prefix {
                    state
                        .dispatcher
                        .host_router
                        .remove_route(host, &old.path_prefix)
                        .await;
                }
            }
            state
                .dispatcher
                .handle(messaging::events::Event::RouteAdd { route })
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
        Change::Gateway { config } => state
            .dispatcher
            .handle(messaging::events::Event::GatewayConfigUpdate {
                app_id: id,
                config: config.as_ref().clone(),
            })
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?,
        Change::GatewayReset => {
            let g = state
                .dispatcher
                .gateway
                .as_ref()
                .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
            if !g
                .circuit_breaker
                .reset_if(app, plan.circuit.as_ref().ok_or(StatusCode::CONFLICT)?)
            {
                return Err(StatusCode::CONFLICT);
            }
        }
        Change::Trigger { id, paused } => store
            .save_meta(
                &details::pause_key(app, id),
                if *paused { "true" } else { "false" },
            )
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?,
    }
    Ok(())
}

#[cfg(test)]
mod field_tests {
    use super::*;
    #[test]
    fn nested_ignored_fields_are_rejected_at_the_scoped_boundary() {
        let raw = json!({"kind":"deploy","artifactSha256":"a".repeat(64),"config":{"id":"test/app:v1","namespace":"test","unknownIgnored":true}});
        let mut typed = raw.clone();
        typed["config"]
            .as_object_mut()
            .unwrap()
            .remove("unknownIgnored");
        assert!(!known_fields(&raw, &typed));
        assert!(known_fields(&typed, &raw));
        assert!(!known_fields(
            &json!({"values":[{"unknown":1}]}),
            &json!({"values":[{}]})
        ));
    }
}
