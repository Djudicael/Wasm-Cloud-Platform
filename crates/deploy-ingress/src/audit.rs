use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::warn;

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_audit(path: &Path, payload: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            warn!(error = %err, path = %parent.display(), "failed to create audit directory");
            return;
        }
    }
    match serde_json::to_string(payload) {
        Ok(line) => {
            if let Err(err) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| {
                    use std::io::Write;
                    writeln!(file, "{line}")
                })
            {
                warn!(error = %err, path = %path.display(), "failed to append deploy ingress audit record");
            }
        }
        Err(err) => warn!(error = %err, "failed to serialize deploy ingress audit record"),
    }
}
