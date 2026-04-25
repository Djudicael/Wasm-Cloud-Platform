use std::collections::HashMap;
use std::net::IpAddr;

/// Per-app virtual DNS resolver.
/// The Supervisor builds one of these for each spawned instance.
/// It resolves `*.internal` hostnames within the caller's namespace
/// to a placeholder loopback address. The actual target port is
/// determined at connect time by the network interceptor.
#[derive(Debug, Clone)]
pub struct VirtualDns {
    /// The namespace of the app that owns this resolver.
    namespace: String,

    /// Map from hostname → placeholder IP.
    /// All *.internal names resolve to 127.0.0.1.
    records: HashMap<String, IpAddr>,
}

impl VirtualDns {
    pub fn new(namespace: String) -> Self {
        VirtualDns {
            namespace,
            records: HashMap::new(),
        }
    }

    /// Register a known internal service.
    pub fn register_service(&mut self, bare_name: &str) {
        self.records.insert(
            format!("{bare_name}.internal"),
            IpAddr::from([127, 0, 0, 1]),
        );
    }

    /// Resolve a hostname. Returns Some([127.0.0.1]) for known *.internal
    /// names in this namespace, None otherwise (falls through to real DNS).
    pub fn resolve(&self, name: &str) -> Option<Vec<IpAddr>> {
        if name.ends_with(".internal") {
            self.records.get(name).map(|ip| vec![*ip])
        } else {
            None
        }
    }

    /// Get the namespace this resolver is scoped to.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Register multiple services at once.
    pub fn register_services(&mut self, names: &[&str]) {
        for name in names {
            self.register_service(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_dns_resolve_known() {
        let mut dns = VirtualDns::new("production".to_string());
        dns.register_service("api-b");

        let result = dns.resolve("api-b.internal");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), vec![IpAddr::from([127, 0, 0, 1])]);
    }

    #[test]
    fn test_virtual_dns_resolve_unknown() {
        let dns = VirtualDns::new("production".to_string());

        let result = dns.resolve("unknown.internal");
        assert!(result.is_none());
    }

    #[test]
    fn test_virtual_dns_external_falls_through() {
        let mut dns = VirtualDns::new("production".to_string());
        dns.register_service("api-b");

        let result = dns.resolve("google.com");
        assert!(result.is_none());
    }

    #[test]
    fn test_virtual_dns_namespace_scoped() {
        let mut dns = VirtualDns::new("tenant-a".to_string());
        dns.register_service("api-users");

        assert_eq!(dns.namespace(), "tenant-a");
        assert!(dns.resolve("api-users.internal").is_some());
    }

    #[test]
    fn test_register_multiple() {
        let mut dns = VirtualDns::new("default".to_string());
        dns.register_services(&["svc-a", "svc-b", "svc-c"]);

        assert!(dns.resolve("svc-a.internal").is_some());
        assert!(dns.resolve("svc-b.internal").is_some());
        assert!(dns.resolve("svc-c.internal").is_some());
        assert!(dns.resolve("svc-d.internal").is_none());
    }
}
