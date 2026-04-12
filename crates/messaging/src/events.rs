use common::types::{AppId, InstanceId};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    InstanceReady {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },
    InstanceDead {
        app_id: AppId,
        instance_id: InstanceId,
        node_id: String,
    },
}
