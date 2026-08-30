/// Platform callbacks invoked by the `ActionDispatcher`.
pub trait EventCallbacks: Send + Sync {
    fn activate_backpressure(&self, reason: &str);
    fn deactivate_backpressure(&self);
    fn mark_nats_disconnected(&self);
    fn publish_node_under_pressure(&self, node_id: &str, pressure_level: u32);
    fn publish_node_pressure_recovered(&self, node_id: &str);
    fn publish_security_incident(&self, node_id: &str, pid: u32, syscall_nr: u64, category: &str);
    fn kill_instance(&self, pid: u32, reason: &str);
    fn prune_idle_instances(&self);
    fn remove_from_upstream(&self, pid: u32);
    fn kill_instance_by_tid(&self, tid: u32, reason: &str);
    /// Report a kernel-observed TCP close so runtime connection-policy
    /// reservations can be released for the attributed workload.
    fn tcp_connection_closed(&self, _tid: u32, _src_port: u16, _dst_port: u16) {}
}

/// A no-op implementation used in tests and safe defaults.
pub struct NoopCallbacks;

impl EventCallbacks for NoopCallbacks {
    fn activate_backpressure(&self, _reason: &str) {}
    fn deactivate_backpressure(&self) {}
    fn mark_nats_disconnected(&self) {}
    fn publish_node_under_pressure(&self, _node_id: &str, _pressure_level: u32) {}
    fn publish_node_pressure_recovered(&self, _node_id: &str) {}
    fn publish_security_incident(
        &self,
        _node_id: &str,
        _pid: u32,
        _syscall_nr: u64,
        _category: &str,
    ) {
    }
    fn kill_instance(&self, _pid: u32, _reason: &str) {}
    fn prune_idle_instances(&self) {}
    fn remove_from_upstream(&self, _pid: u32) {}
    fn kill_instance_by_tid(&self, _tid: u32, _reason: &str) {}
}
