//! Generic, node-local periodic invocation contracts. No application workflow state.
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};

pub const TRIGGER_ROUTE_PREFIX: &str = "/_platform/triggers/";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    Schedule(ScheduleTrigger),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTrigger {
    pub id: String,
    pub every: String,
    pub route: String,
    pub method: String,
    pub max_concurrency: u32,
    pub timeout: String,
}

impl Trigger {
    pub fn schedule(&self) -> &ScheduleTrigger {
        match self {
            Self::Schedule(schedule) => schedule,
        }
    }
}

/// Integer durations only: no implicit units, fractions, cron, or zero intervals.
pub fn duration(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(n) = value.strip_suffix("ms") {
        (n, 1_u64)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1_000)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60_000)
    } else {
        return Err("trigger duration requires ms, s, or m".into());
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err("trigger duration must be a positive integer".into());
    }
    let millis = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n > 0)
        .ok_or("invalid trigger duration")?;
    Ok(Duration::from_millis(millis))
}

pub fn validate(triggers: &[Trigger]) -> Result<(), String> {
    if triggers.len() > 8 {
        return Err("at most 8 triggers per application".into());
    }
    let mut ids = HashSet::new();
    let mut routes = HashSet::new();
    for trigger in triggers {
        let s = trigger.schedule();
        if s.id.is_empty()
            || s.id.len() > 64
            || !s
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !ids.insert(&s.id)
        {
            return Err("trigger IDs must be unique ASCII identifiers (1..64 characters)".into());
        }
        if s.route != format!("{TRIGGER_ROUTE_PREFIX}{}", s.id) || !routes.insert(&s.route) {
            return Err("trigger route must be /_platform/triggers/<id>".into());
        }
        if s.method != "POST" {
            return Err("schedule triggers currently require POST".into());
        }
        if !(1..=32).contains(&s.max_concurrency) {
            return Err("trigger concurrency must be 1..32".into());
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(86_400))
            .contains(&duration(&s.every)?)
        {
            return Err("trigger interval must be 100ms..24h".into());
        }
        if !(Duration::from_millis(10)..=Duration::from_secs(300)).contains(&duration(&s.timeout)?)
        {
            return Err("trigger timeout must be 10ms..5m".into());
        }
    }
    Ok(())
}

/// The whole platform path space is inaccessible through network requests.
/// Reject ambiguous encodings too; a framework must never decode a second route.
pub fn reserved_network_path(path: &str) -> bool {
    let mut path = path.as_bytes().to_vec();
    for _ in 0..4 {
        if !path.contains(&b'%') {
            break;
        }
        let mut decoded = Vec::with_capacity(path.len());
        let mut changed = false;
        let mut i = 0;
        while i < path.len() {
            if path[i] == b'%' {
                let byte = path
                    .get(i + 1..i + 3)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .and_then(|s| u8::from_str_radix(s, 16).ok());
                if let Some(byte) = byte {
                    decoded.push(byte);
                    changed = true;
                    i += 3;
                } else {
                    decoded.push(path[i]);
                    i += 1;
                }
            } else {
                decoded.push(path[i]);
                i += 1;
            }
        }
        if !changed {
            break;
        }
        path = decoded;
    }
    if path
        .windows(3)
        .any(|s| s[0] == b'%' && s[1].is_ascii_hexdigit() && s[2].is_ascii_hexdigit())
    {
        return true;
    }
    let path = String::from_utf8_lossy(&path)
        .to_ascii_lowercase()
        .replace('\\', "/");
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    format!("/{}", segments.join("/")).starts_with("/_platform/triggers")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations_are_bounded_and_explicit() {
        assert_eq!(duration("2s").unwrap(), Duration::from_secs(2));
        for invalid in ["0s", "-1s", "1.5s", "1", "18446744073709551615m", "+2s"] {
            assert!(duration(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn declarations_bound_identifiers_capacity_and_intervals() {
        let s = ScheduleTrigger {
            id: "tick".into(),
            every: "100ms".into(),
            route: "/_platform/triggers/tick".into(),
            method: "POST".into(),
            max_concurrency: 32,
            timeout: "5m".into(),
        };
        validate(&[Trigger::Schedule(s.clone())]).unwrap();
        assert!(validate(&vec![Trigger::Schedule(s.clone()); 9]).is_err());
        assert!(validate(&vec![Trigger::Schedule(s.clone()); 2]).is_err());
        for (interval, timeout, capacity) in [
            ("99ms", "1s", 1),
            ("1441m", "1s", 1),
            ("1s", "301s", 1),
            ("1s", "9ms", 1),
            ("1s", "1s", 33),
        ] {
            let mut invalid = s.clone();
            invalid.every = interval.into();
            invalid.timeout = timeout.into();
            invalid.max_concurrency = capacity;
            assert!(validate(&[Trigger::Schedule(invalid)]).is_err());
        }
    }
    #[test]
    fn network_paths_cannot_alias_trigger_routes() {
        for path in [
            "/_platform/triggers/tick",
            "/_PLATFORM/triggers/tick",
            "/%5fplatform/triggers/tick",
            "//_platform/triggers/tick",
            "/public/../_platform/triggers/tick",
        ] {
            assert!(reserved_network_path(path));
        }
        for path in [
            "/health",
            "/public/a%20b",
            "/public/100%25",
            "/public/100%2525",
        ] {
            assert!(!reserved_network_path(path), "{path}");
        }
    }
}
