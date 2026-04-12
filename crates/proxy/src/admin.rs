use super::upstream::UpstreamRegistry;
use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;

#[derive(Clone)]
struct AdminState {
    upstream: Arc<UpstreamRegistry>,
}

pub fn admin_router(upstream: Arc<UpstreamRegistry>) -> Router {
    let state = AdminState { upstream };
    Router::new()
        .route("/upstreams", get(list_upstreams))
        .with_state(state)
}

async fn list_upstreams(State(s): State<AdminState>) -> Json<serde_json::Value> {
    let map = s.upstream.inner.read().await;
    let out: serde_json::Map<_, _> = map
        .iter()
        .map(|(k, (_, addrs))| {
            let addrs_str: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
            (k.clone(), serde_json::json!(addrs_str))
        })
        .collect();
    Json(serde_json::Value::Object(out))
}
