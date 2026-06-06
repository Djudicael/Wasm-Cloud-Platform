use pingora_proxy::Session;

pub(super) fn strip_uri_prefix(path: &str, query: Option<&str>, prefix: &str) -> Option<String> {
    let stripped = path.strip_prefix(prefix)?;
    let new_path = if stripped.starts_with('/') || stripped.is_empty() {
        stripped.to_string()
    } else {
        format!("/{}", stripped)
    };

    Some(match query {
        Some(query) => format!("{new_path}?{query}"),
        None => new_path,
    })
}

pub(super) fn canonical_host(host: &str) -> &str {
    if let Some(stripped) = host.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            return &stripped[..end];
        }
    }

    if let Some((name, port)) = host.rsplit_once(':') {
        if !name.contains(':') && !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) {
            return name;
        }
    }

    host
}

pub(super) fn extract_request_host(session: &Session) -> String {
    let raw_host = session
        .req_header()
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            session
                .req_header()
                .uri
                .authority()
                .map(|authority| authority.as_str().to_string())
        })
        .unwrap_or_default();

    canonical_host(&raw_host).to_string()
}
