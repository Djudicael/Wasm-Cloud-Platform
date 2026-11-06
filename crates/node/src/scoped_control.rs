//! Generic node-local application control. Separate from global admin authority.
//! No agent contracts, provider routing or model-specific behavior belongs here.
use crate::handlers::EventDispatcher;
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use common::{
    control::{Fence, Grant, Grants, Permission, Restart},
    types::AppId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc};
mod details;
mod nodes;
mod observations;
mod plans;

pub(crate) struct Control {
    grants: Grants,
    dispatcher: Arc<EventDispatcher>,
    gate: tokio::sync::Mutex<()>,
    ebpf: Arc<ebpf_monitor::ActionDispatcher>,
    ebpf_state: ebpf_monitor::MonitorRuntimeState,
    health: proxy::health::HealthState,
}
fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error":code}))).into_response()
}
fn audit_control(state: &Control, principal: &str, app: &str, operation: &str, kind: &str) {
    supervisor::audit::write_audit_event(
        "/var/log/wasm-node/audit.jsonl",
        &supervisor::audit::AuditEvent {
            timestamp: observations::now(),
            node_id: state.dispatcher.node_id.clone(),
            event_type: supervisor::audit::AuditEventType::AdminApiCall,
            actor: principal.into(),
            app_id: app.into(),
            details: json!({"operationId":operation,"kind":kind,"outcome":"succeeded"}),
        },
    );
}
pub(crate) fn running_binary_sha256() -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open("/proc/self/exe")?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn digest(value: &impl Serialize) -> Result<String, StatusCode> {
    let value = serde_json::to_value(value).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&value).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    )))
}
fn response(value: Value) -> Response {
    if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= 12000) {
        Json(value).into_response()
    } else {
        error(StatusCode::PAYLOAD_TOO_LARGE, "control_result_too_large")
    }
}
impl Control {
    fn authorize(
        &self,
        headers: &HeaderMap,
        app: Option<&str>,
        permission: Permission,
    ) -> Result<Grant, StatusCode> {
        let token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        let grant = token
            .and_then(|t| self.grants.authenticate(t))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if !grant.permissions.contains(&permission)
            || app.is_some_and(|app| !grant.allows(app, permission))
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(grant)
    }
    async fn snapshot(&self, app: &str) -> Result<Value, StatusCode> {
        let store = &self.dispatcher.store;
        let app_id = AppId(app.into());
        let config = store
            .load_config(&app_id)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if config.is_none()
            || store
                .is_undeployed(app)
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            if !self
                .dispatcher
                .supervisor
                .list_instances(&app_id)
                .await
                .is_empty()
            {
                return Err(StatusCode::CONFLICT);
            }
            return Ok(
                json!({"app":app,"node":self.dispatcher.node_id,"present":false,"fence":{"revision":digest(&json!({"app":app,"absent":true}))?,"instances":[]},"configuration":{},"gateway":null,"artifactSha256":null}),
            );
        }
        let config = config.expect("checked config presence");
        let gateway = store
            .load_gateway_config(app)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let artifact = store
            .get_artifact_sha256(&app_id)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let routes = store
            .list_routes_for_app(app)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let paused = details::paused_triggers(store, app, &config)?;
        let desired = store
            .load_meta(&format!("control-desired-instances:{app}"))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let circuit = self
            .dispatcher
            .gateway
            .as_ref()
            .map(|g| g.circuit_breaker.snapshot(app));
        let revision = digest(
            &json!({"config":config,"gateway":gateway,"artifact":artifact,"routes":routes,"pausedTriggers":paused,"circuit":circuit,"desiredInstances":desired}),
        )?;
        let instances: BTreeSet<_> = self
            .dispatcher
            .supervisor
            .list_instances(&app_id)
            .await
            .into_iter()
            .map(|i| i.0.to_string())
            .collect();
        let fence = Fence {
            revision,
            instances,
        };
        let environment_names: BTreeSet<_> = config.env_vars.keys().cloned().collect();
        // Explicit allowlist: never serialize environment values, credentials,
        // API key records, OIDC client secrets or proxy upstream addresses.
        let raw_gateway =
            serde_json::to_value(gateway).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let safe_gateway: serde_json::Map<_, _> = [
            "host",
            "path_prefix",
            "allowed_callers",
            "auth",
            "cors",
            "rate_limit",
            "circuit_breaker",
        ]
        .into_iter()
        .filter_map(|k| raw_gateway.get(k).map(|v| (k.into(), v.clone())))
        .collect();
        Ok(
            json!({"app":app,"node":self.dispatcher.node_id,"present":true,"artifactSha256":artifact,"fence":fence,
            "configuration":{"fuelQuota":config.fuel_quota.0,"memoryPages":config.memory_limit.0,"maxInstances":config.max_instances,
            "idleTimeoutSeconds":config.idle_timeout_secs,"policy":config.policy,"environmentNames":environment_names,
            "secretNames":config.secret_keys,"triggers":config.triggers,"placement":config.placement,"localDependencies":config.local_dependencies},
            "gateway":safe_gateway}),
        )
    }
}
pub(crate) fn router(
    grants: Grants,
    dispatcher: Arc<EventDispatcher>,
    ebpf: Arc<ebpf_monitor::ActionDispatcher>,
    ebpf_state: ebpf_monitor::MonitorRuntimeState,
    health: proxy::health::HealthState,
) -> Router {
    Router::new()
        .route("/control/v1/nodes", get(nodes::list))
        .route("/control/v1/nodes/{node}", get(nodes::get))
        .route("/control/v1/nodes/{node}/changes", post(nodes::change))
        .route(
            "/control/v1/nodes/{node}/operations/{operation}",
            get(nodes::operation),
        )
        .route(
            "/control/v1/nodes/{node}/storage",
            get(observations::storage),
        )
        .route("/control/v1/release", get(observations::release))
        .route("/control/v1/billing", get(observations::billing))
        .route("/control/v1/apps/{app}/flows", get(observations::flows))
        .route(
            "/control/v1/apps/{app}/timeline",
            get(observations::timeline),
        )
        .route("/control/v1/apps", get(list))
        .route("/control/v1/apps/{app}", get(inspect))
        .route("/control/v1/apps/{app}/restart", post(restart))
        .route("/control/v1/apps/{app}/details", get(details::get))
        .route("/control/v1/apps/{app}/history", get(details::history))
        .route("/control/v1/apps/{app}/diff", get(details::diff))
        .route(
            "/control/v1/apps/{app}/plans",
            post(plans::create).layer(DefaultBodyLimit::max(16384)),
        )
        .route("/control/v1/apps/{app}/plans/{plan}", get(plans::get))
        .route("/control/v1/apps/{app}/apply", post(plans::apply))
        .route(
            "/control/v1/apps/{app}/operations/{operation}",
            get(operation),
        )
        .with_state(Arc::new(Control {
            grants,
            dispatcher,
            gate: tokio::sync::Mutex::new(()),
            ebpf,
            ebpf_state,
            health,
        }))
        .layer(DefaultBodyLimit::max(8192))
}
async fn list(State(state): State<Arc<Control>>, headers: HeaderMap) -> Response {
    let grant = match state.authorize(&headers, None, Permission::Observe) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let deployed = match state.dispatcher.store.list_deployed_apps() {
        Ok(v) => v,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "control_unavailable"),
    };
    let apps: Result<Vec<_>, _> = deployed
        .into_iter()
        .filter(|app| grant.applications.contains(&app.0))
        .map(|app| {
            let config = state.dispatcher.store.load_config(&app)?;
            let artifact = state.dispatcher.store.get_artifact_sha256(&app)?;
            Ok::<_, common::error::PlatformError>(json!({"app":app.0,"namespace":app.namespace(),"version":app.0.rsplit_once(':').map(|(_,v)|v),"placement":config.as_ref().map(|c|&c.placement),"artifactSha256":artifact}))
        })
        .collect();
    let apps = match apps {
        Ok(apps) => apps,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "control_unavailable"),
    };
    response(json!({"apps":apps,"node":state.dispatcher.node_id}))
}
async fn inspect(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = state.authorize(&headers, Some(&app), Permission::Observe) {
        return error(status, "control_access_denied");
    }
    match state.snapshot(&app).await {
        Ok(v) if v["present"] == false => {
            error(StatusCode::NOT_FOUND, "control_snapshot_unavailable")
        }
        Ok(v) => response(v),
        Err(s) => error(s, "control_snapshot_unavailable"),
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    request_sha256: String,
    status: String,
    output: Value,
    #[serde(default)]
    permission: Option<Permission>,
    #[serde(default)]
    timestamp_ms: u64,
}
async fn operation(
    State(state): State<Arc<Control>>,
    Path((app, operation)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let grant = match plans::effect_grant(&state, &headers, &app) {
        Ok(grant) => grant,
        Err(status) => return error(status, "control_access_denied"),
    };
    if !common::control::valid_operation_key(&operation) {
        return error(StatusCode::BAD_REQUEST, "invalid_operation");
    }
    let key = match digest(&json!({"principal":grant.principal,"app":app,"operationId":operation}))
    {
        Ok(key) => format!("control-operation:{key}"),
        Err(status) => return error(status, "control_unavailable"),
    };
    match state.dispatcher.store.load_meta(&key) {
        Ok(Some(raw)) => match serde_json::from_str::<Record>(&raw) {
            Ok(record)
                if !grant
                    .permissions
                    .contains(&record.permission.unwrap_or(Permission::Restart)) =>
            {
                error(StatusCode::FORBIDDEN, "control_access_denied")
            }
            Ok(record) if record.status == "succeeded" => response(record.output),
            Ok(_) => error(StatusCode::CONFLICT, "control_outcome_unknown"),
            Err(_) => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "control_journal_unavailable",
            ),
        },
        Ok(None) => error(StatusCode::NOT_FOUND, "control_operation_not_found"),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "control_journal_unavailable",
        ),
    }
}
async fn restart(
    State(state): State<Arc<Control>>,
    Path(app): Path<String>,
    headers: HeaderMap,
    input: Result<Json<Restart>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let grant = match state.authorize(&headers, Some(&app), Permission::Restart) {
        Ok(g) => g,
        Err(s) => return error(s, "control_access_denied"),
    };
    let request = match input {
        Ok(Json(r)) if r.valid() => r,
        _ => return error(StatusCode::BAD_REQUEST, "invalid_restart"),
    };
    // Serializes operation reservation/recovery locally; deployment fencing uses
    // the same lifecycle gate as control-plane events. Supervisor fences spawns.
    let _operation_guard = match state.gate.try_lock() {
        Ok(g) => g,
        Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "control_busy"),
    };
    let _deployment_guard = state.dispatcher.lifecycle_gate.lock().await;
    let identity =
        json!({"principal":grant.principal,"app":app,"operationId":request.operation_id});
    let key = match digest(&identity) {
        Ok(s) => format!("control-operation:{s}"),
        Err(s) => return error(s, "control_unavailable"),
    };
    let request_sha256 = match digest(&request) {
        Ok(s) => s,
        Err(s) => return error(s, "control_unavailable"),
    };
    let store = &state.dispatcher.store;
    match store.load_meta(&key) {
        Ok(Some(raw)) => {
            let saved: Record = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "control_journal_unavailable",
                    )
                }
            };
            if saved.request_sha256 != request_sha256 {
                return error(StatusCode::CONFLICT, "control_identity_conflict");
            }
            return if saved.status == "succeeded" {
                response(saved.output)
            } else {
                error(StatusCode::CONFLICT, "control_outcome_unknown")
            };
        }
        Ok(None) => {}
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "control_journal_unavailable",
            )
        }
    }
    let snapshot = match state.snapshot(&app).await {
        Ok(v) => v,
        Err(s) => return error(s, "control_snapshot_unavailable"),
    };
    if snapshot["fence"] != json!(request.expected) {
        return error(StatusCode::CONFLICT, "control_precondition_changed");
    }
    let mut record = Record {
        request_sha256,
        status: "dispatched".into(),
        output: Value::Null,
        permission: Some(Permission::Restart),
        timestamp_ms: observations::now(),
    };
    let persist = |r: &Record| {
        serde_json::to_string(r)
            .ok()
            .is_some_and(|raw| store.save_meta(&key, &raw).is_ok())
    };
    if !persist(&record) {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "control_journal_unavailable",
        );
    }
    match state
        .dispatcher
        .supervisor
        .evict_instances_if(&AppId(app.clone()), &request.expected.instances)
        .await
    {
        Ok(true) => {
            record.status = "succeeded".into();
            record.output = json!({"app":app,"node":state.dispatcher.node_id,"operationId":request.operation_id,"evictedInstances":request.expected.instances,"revision":request.expected.revision,"restartMode":"on-next-request"});
            if !persist(&record) {
                return error(StatusCode::CONFLICT, "control_outcome_unknown");
            }
            audit_control(
                &state,
                &grant.principal,
                &app,
                &request.operation_id,
                "conditional_restart",
            );
            tracing::info!(principal=%grant.principal,app=%app,operation=%request.operation_id,"scoped application eviction completed");
            response(record.output)
        }
        Ok(false) => error(StatusCode::CONFLICT, "control_precondition_changed"),
        Err(_) => error(StatusCode::CONFLICT, "control_outcome_unknown"),
    }
}

pub(crate) fn load_grants(path: &str) -> anyhow::Result<Grants> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    let before = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "control grants must be a regular private file"
    );
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    let owner = std::fs::metadata("/proc/self")?.uid();
    anyhow::ensure!(
        meta.is_file()
            && meta.uid() == owner
            && meta.ino() == before.ino()
            && meta.dev() == before.dev()
            && meta.permissions().mode() & 0o777 == 0o600
            && meta.len() <= 65536,
        "control grants must be a stable owned private 0600 file <=64KiB"
    );
    let mut raw = Vec::new();
    file.take(65537).read_to_end(&mut raw)?;
    anyhow::ensure!(raw.len() <= 65536, "control grants too large");
    let grants: Grants =
        serde_json::from_slice(&raw).map_err(|_| anyhow::anyhow!("invalid control grants"))?;
    anyhow::ensure!(
        grants.valid(),
        "invalid control grant permissions or applications"
    );
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::fs::PermissionsExt};
    #[test]
    fn retained_v1_journal_records_keep_their_receipt_and_restart_permission() {
        let old = json!({"requestSha256":"a".repeat(64),"status":"succeeded","output":{"revision":"b".repeat(64),"restartMode":"on-next-request"}});
        let record: Record = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            record.permission.unwrap_or(Permission::Restart),
            Permission::Restart
        );
        assert_eq!(record.timestamp_ms, 0);
        assert_eq!(record.output, old["output"]);
    }
    #[test]
    fn service_grants_refuse_public_files_symlinks_and_ambiguous_scopes() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let policy = json!({"grants":[{"principal":"test-observer","token":"a".repeat(64),"applications":["test/example:v1"],"permissions":["observe"]}]});
        file.write_all(&serde_json::to_vec(&policy).unwrap())
            .unwrap();
        let path = file.path().to_str().unwrap();
        assert!(load_grants(path).is_ok());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_grants(path).is_err());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("grant.json");
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert!(load_grants(link.to_str().unwrap()).is_err());
        file.as_file().set_len(65537).unwrap();
        assert!(load_grants(path).is_err());
    }
}
