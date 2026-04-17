use crate::backpressure::BackpressureSignal;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use messaging::reconnect::NatsHealth;
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub node_id: String,
    pub nats_connected: bool,
    pub active_instances: u32,
    pub accepting_requests: bool,
}

#[derive(Clone)]
pub struct HealthState {
    pub node_id: String,
    pub nats_health: Arc<NatsHealth>,
    pub backpressure: Arc<BackpressureSignal>,
}

pub fn health_router(
    node_id: String,
    nats_health: Arc<NatsHealth>,
    backpressure: Arc<BackpressureSignal>,
) -> Router {
    let state = HealthState {
        node_id,
        nats_health,
        backpressure,
    };
    Router::new()
        .route("/health", get(health_check))
        .with_state(Arc::new(state))
}

pub async fn health_check(State(state): State<Arc<HealthState>>) -> Response {
    let healthy = state.nats_health.is_connected() && state.backpressure.is_accepting();

    let response = HealthResponse {
        status: if healthy {
            "ok".to_string()
        } else {
            "degraded".to_string()
        },
        node_id: state.node_id.clone(),
        nats_connected: state.nats_health.is_connected(),
        active_instances: 0,
        accepting_requests: state.backpressure.is_accepting(),
    };

    if healthy {
        (StatusCode::OK, Json(response)).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(response)).into_response()
    }
}
