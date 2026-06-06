use std::io::Write;

/// Configuration for log file rotation.
#[derive(Debug, Clone)]
pub struct LogRotationConfig {
    pub max_file_size_bytes: u64,
    pub max_files: u32,
    pub max_age: std::time::Duration,
    pub compress: bool,
}

impl Default for LogRotationConfig {
    fn default() -> Self {
        LogRotationConfig {
            max_file_size_bytes: 100 * 1024 * 1024,
            max_files: 10,
            max_age: std::time::Duration::from_secs(24 * 3600),
            compress: true,
        }
    }
}

struct RotatingFileState {
    current_size: u64,
    current_file: Option<std::fs::File>,
}

pub struct RotatingFileWriter {
    path: std::path::PathBuf,
    config: LogRotationConfig,
    state: std::sync::Mutex<RotatingFileState>,
}

impl RotatingFileWriter {
    pub fn new(path: std::path::PathBuf, config: LogRotationConfig) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let current_size = file.metadata()?.len();

        Ok(RotatingFileWriter {
            path,
            config,
            state: std::sync::Mutex::new(RotatingFileState {
                current_size,
                current_file: Some(file),
            }),
        })
    }

    pub fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.current_size > self.config.max_file_size_bytes {
            self.rotate_with_state(&mut state)?;
        }

        if let Some(ref mut file) = state.current_file {
            let bytes = line.as_bytes();
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            state.current_size += bytes.len() as u64 + 1;
        }

        Ok(())
    }

    fn rotate_with_state(
        &self,
        state: &mut std::sync::MutexGuard<'_, RotatingFileState>,
    ) -> std::io::Result<()> {
        state.current_file = None;

        let oldest = format!("{}.{}", self.path.display(), self.config.max_files);
        let _ = std::fs::remove_file(&oldest);

        for i in (1..self.config.max_files).rev() {
            let from = format!("{}.{}", self.path.display(), i);
            let to = format!("{}.{}", self.path.display(), i + 1);
            let _ = std::fs::rename(&from, &to);
        }

        let rotated = format!("{}.1", self.path.display());
        let _ = std::fs::rename(&self.path, &rotated);

        if self.config.compress {
            self.compress_file(&rotated);
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        state.current_file = Some(file);
        state.current_size = 0;
        Ok(())
    }

    fn compress_file(&self, path: &str) {
        let gz_path = format!("{}.gz", path);
        let mut input = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("failed to open log file for compression '{}': {}", path, e);
                return;
            }
        };
        let mut output = match std::fs::File::create(&gz_path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("failed to create compressed file '{}': {}", gz_path, e);
                return;
            }
        };
        let mut encoder = flate2::write::GzEncoder::new(&mut output, flate2::Compression::fast());
        if let Err(e) = std::io::copy(&mut input, &mut encoder) {
            tracing::warn!("failed to compress log file '{}': {}", path, e);
            return;
        }
        if let Err(e) = encoder.finish() {
            tracing::warn!("failed to finish gzip encoding for '{}': {}", gz_path, e);
        }
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!("failed to remove uncompressed log file '{}': {}", path, e);
        }
    }
}
