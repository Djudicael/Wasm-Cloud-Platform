use super::*;
use axum::extract::Query;
use common::types::AppConfig;
pub(super) fn pause_key(app: &str, id: &str) -> String {
    format!("control-trigger-paused:{app}|{id}")
}
pub(super) fn paused_triggers(
    store: &storage::Store,
    app: &str,
    config: &AppConfig,
) -> Result<Value, StatusCode> {
    let mut out = serde_json::Map::new();
    for trigger in &config.triggers {
        let id = &trigger.schedule().id;
        out.insert(
            id.clone(),
            json!(
                store
                    .load_meta(&pause_key(app, id))
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                    .as_deref()
                    == Some("true")
            ),
        );
    }
    Ok(Value::Object(out))
}
pub(super) fn config_projection(c: &AppConfig) -> Value {
    let names: BTreeSet<_> = c.env_vars.keys().collect();
    json!({"fuelQuota":c.fuel_quota.0,"memoryPages":c.memory_limit.0,"maxInstances":c.max_instances,"idleTimeoutSeconds":c.idle_timeout_secs,"policy":c.policy,"environmentNames":names,"secretNames":c.secret_keys,"triggers":c.triggers,"placement":c.placement,"localDependencies":c.local_dependencies,"healthCheckPath":c.health_check_path,"rateLimit":c.rate_limit})
}
pub(super) async fn get(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(s) = state.authorize(&headers, Some(&app), Permission::Observe) {
        return error(s, "control_access_denied");
    }
    match detail(&state, &app).await {
        Ok(v) => response(v),
        Err(s) => error(s, "control_details_unavailable"),
    }
}
async fn detail(state: &Control, app: &str) -> Result<Value, StatusCode> {
    let snapshot = state.snapshot(app).await?;
    if snapshot["present"] != true {
        return Ok(snapshot);
    }
    let id = AppId(app.into());
    let store = &state.dispatcher.store;
    let c = store
        .load_config(&id)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let keys: BTreeSet<_> = state
        .dispatcher
        .secret_provider
        .list_keys(&id)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .into_iter()
        .collect();
    let bindings: Vec<_> = c
        .secret_keys
        .iter()
        .map(|name| json!({"name":name,"present":keys.contains(name)}))
        .collect();
    let declared: BTreeSet<_> = c.secret_keys.iter().cloned().collect();
    let binding_audit = json!({"missing":declared.difference(&keys).collect::<Vec<_>>(),"unboundStoredNames":keys.difference(&declared).collect::<Vec<_>>(),"declaredCount":declared.len(),"storedCount":keys.len(),"valuesReturned":false});
    let paused = paused_triggers(store, app, &c)?;
    let mut triggers = Vec::new();
    for t in &c.triggers {
        let schedule = t.schedule();
        let cursor = store
            .load_meta(&storage::triggers::cursor_key(
                &state.dispatcher.node_id,
                &id,
                &schedule.id,
            ))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let stats = store
            .load_meta(&storage::triggers::stats_key(
                &state.dispatcher.node_id,
                &id,
                &schedule.id,
            ))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        triggers.push(json!({"declaration":t,"paused":paused[&schedule.id],"cursor":cursor,"statistics":stats}));
    }
    let runtime = state.dispatcher.supervisor.execution_snapshot(&id).await;
    let routes = store
        .list_routes_for_app(app)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut gateway = state
        .dispatcher
        .gateway
        .as_ref()
        .map(|g| g.circuit_breaker.snapshot(app));
    if let Some(value) = gateway.as_mut() {
        value["rateLimitRejections"] = state.dispatcher.rate_limiter.rejection_snapshot(app);
    }
    let history = store
        .deployment_history(app, 9)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let desired = store
        .load_meta(&format!("control-desired-instances:{app}"))
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .map(|s| s.parse::<u32>())
        .transpose()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let effective_policy = state
        .dispatcher
        .supervisor
        .effective_network_policy(&c)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let probe_health = state
        .health
        .instance_count_provider
        .app_health_summaries()
        .into_iter()
        .find(|h| h.app_id == app);
    Ok(
        json!({"app":app,"node":state.dispatcher.node_id,"snapshot":snapshot,"runtime":runtime,"probeHealth":probe_health,"effectivePolicy":effective_policy,"routes":routes,"triggers":triggers,"secretBindings":bindings,"bindingAudit":binding_audit,"gatewayState":gateway,"desiredInstances":desired,"deploymentHistory":history.iter().take(8).map(project_revision).collect::<Vec<_>>(),"historyTruncated":history.len()>8}),
    )
}
pub(super) fn project_revision(r: &storage::deployment_history::Revision) -> Value {
    json!({"app":r.config.id.0,"version":r.config.id.0.rsplit(':').next(),"revision":r.revision,"artifactSha256":r.artifact_sha256,"recordedAtMs":r.recorded_at_ms,"sequence":r.sequence})
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct HistoryQuery {
    #[serde(default = "history_limit")]
    limit: usize,
    before_sequence: Option<u64>,
}
fn history_limit() -> usize {
    16
}
pub(super) async fn history(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Err(s) = state.authorize(&headers, Some(&app), Permission::Observe) {
        return error(s, "control_access_denied");
    }
    if q.limit == 0 || q.limit > 32 {
        return error(StatusCode::BAD_REQUEST, "invalid_history_query");
    }
    match state
        .dispatcher
        .store
        .deployment_history_before(&app, q.before_sequence, q.limit + 1)
    {
        Ok(rows) => response(
            json!({"app":app,"node":state.dispatcher.node_id,"revisions":rows.iter().take(q.limit).map(project_revision).collect::<Vec<_>>(),"truncated":rows.len()>q.limit,"nextBeforeSequence":if rows.len()>q.limit{rows.get(q.limit-1).map(|r|r.sequence)}else{None},"coverage":"atomic-deployment-history-since-feature-enabled"}),
        ),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "control_history_unavailable",
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Diff {
    from: String,
    to: String,
}
pub(super) fn resolve_revision(
    state: &Control,
    grant: &Grant,
    app: &str,
    revision: &str,
) -> Result<(AppConfig, String), StatusCode> {
    if revision.len() == 64 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        let r = state
            .dispatcher
            .store
            .deployment_revision(app, revision)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .ok_or(StatusCode::NOT_FOUND)?;
        return Ok((r.config, r.artifact_sha256));
    }
    if revision.is_empty()
        || revision.len() > 128
        || !revision
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (base, _) = app.rsplit_once(':').ok_or(StatusCode::BAD_REQUEST)?;
    let source = format!("{base}:{revision}");
    if !grant.applications.contains(&source) {
        return Err(StatusCode::FORBIDDEN);
    }
    let id = AppId(source);
    let c = state
        .dispatcher
        .store
        .load_config(&id)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let hash = state
        .dispatcher
        .store
        .get_artifact_sha256(&id)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok((c, hash))
}
pub(super) async fn diff(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    Query(q): Query<Diff>,
) -> Response {
    let grant = match state.authorize(&headers, Some(&app), Permission::Observe) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let result = (|| {
        let (a, ah) = resolve_revision(&state, &grant, &app, &q.from)?;
        let (b, bh) = resolve_revision(&state, &grant, &app, &q.to)?;
        let av = config_projection(&a);
        let bv = config_projection(&b);
        let changed: Vec<_> = av
            .as_object()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
            .keys()
            .filter(|k| av[*k] != bv[*k])
            .cloned()
            .collect();
        Ok::<_, StatusCode>(
            json!({"app":app,"node":state.dispatcher.node_id,"from":q.from,"to":q.to,"changedFields":changed,"artifactChanged":ah!=bh,"environmentValuesChanged":a.env_vars!=b.env_vars,"before":av,"after":bv}),
        )
    })();
    match result {
        Ok(v) => response(v),
        Err(s) => error(s, "control_diff_unavailable"),
    }
}
