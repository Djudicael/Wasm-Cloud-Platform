use std::io::Write as IoWrite;
use std::sync::Arc;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

use super::{LogFormat, LogOutput, LoggingConfig, NodeJsonFormatter, SamplingLayer};

pub enum LogWriter {
    Stdout,
    Stderr,
    File(Arc<std::sync::Mutex<std::fs::File>>),
}

pub fn build_log_writer(output: &LogOutput) -> Result<LogWriter, String> {
    match output {
        LogOutput::Stdout => Ok(LogWriter::Stdout),
        LogOutput::Stderr => Ok(LogWriter::Stderr),
        LogOutput::File { path } => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("failed to open log file {}: {}", path.display(), e))?;
            Ok(LogWriter::File(Arc::new(std::sync::Mutex::new(file))))
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        match self {
            LogWriter::Stdout => LogWriterGuard::Stdout(std::io::stdout()),
            LogWriter::Stderr => LogWriterGuard::Stderr(std::io::stderr()),
            LogWriter::File(file) => LogWriterGuard::File(FileWriterGuard {
                guard: file.lock().unwrap_or_else(|e| e.into_inner()),
            }),
        }
    }
}

pub enum LogWriterGuard<'a> {
    Stdout(std::io::Stdout),
    Stderr(std::io::Stderr),
    File(FileWriterGuard<'a>),
}

pub struct FileWriterGuard<'a> {
    guard: std::sync::MutexGuard<'a, std::fs::File>,
}

impl<'a> IoWrite for FileWriterGuard<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.guard.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.guard.flush()
    }
}

impl<'a> IoWrite for LogWriterGuard<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            LogWriterGuard::Stdout(w) => w.write(buf),
            LogWriterGuard::Stderr(w) => w.write(buf),
            LogWriterGuard::File(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            LogWriterGuard::Stdout(w) => w.flush(),
            LogWriterGuard::Stderr(w) => w.flush(),
            LogWriterGuard::File(w) => w.flush(),
        }
    }
}

/// Handle for hot-reloading log levels at runtime.
pub struct LogReloadHandle {
    reload: tracing_subscriber::reload::Handle<EnvFilter, Registry>,
}

impl LogReloadHandle {
    pub fn new(reload: tracing_subscriber::reload::Handle<EnvFilter, Registry>) -> Self {
        Self { reload }
    }

    pub fn update_levels(&self, directives: &str) -> Result<(), String> {
        let new_filter = EnvFilter::new(directives);
        self.reload.reload(new_filter).map_err(|e| e.to_string())
    }

    pub fn set_module_level(&self, module: &str, level: &str) -> Result<(), String> {
        let directive = format!("{}={}", module, level);
        let new_filter = self
            .reload
            .with_current(|current| {
                let current_str = current.to_string();
                let combined = if current_str.is_empty() {
                    directive.clone()
                } else {
                    format!("{},{}", current_str, directive)
                };
                EnvFilter::new(combined)
            })
            .map_err(|e| e.to_string())?;
        self.reload.reload(new_filter).map_err(|e| e.to_string())
    }
}

impl Clone for LogReloadHandle {
    fn clone(&self) -> Self {
        LogReloadHandle {
            reload: self.reload.clone(),
        }
    }
}

/// Initialize the structured logging subsystem.
pub fn init_logging(config: &LoggingConfig) -> LogReloadHandle {
    let mut directives = String::new();
    directives.push_str(&config.default_level);

    for (module, level) in &config.module_levels {
        directives.push_str(&format!(",{}={}", module, level));
    }

    let env_filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(&directives)
    };

    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);
    let writer = build_log_writer(&config.output).unwrap_or_else(|e| {
        eprintln!("{e}; exiting");
        std::process::exit(1);
    });

    match config.format {
        LogFormat::Json => {
            let formatter = NodeJsonFormatter::new(config.node_id.clone(), config.include_source);
            let fmt_layer = tracing_subscriber::fmt::layer()
                .event_format(formatter)
                .with_writer(writer);

            let sampling_layer = if config.sampling_enabled {
                Some(SamplingLayer::new(
                    config.info_sample_rate,
                    config.debug_sample_rate,
                    config.trace_sample_rate,
                ))
            } else {
                None
            };

            Registry::default()
                .with(filter_layer)
                .with(fmt_layer)
                .with(sampling_layer)
                .init();
        }
        LogFormat::Text => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_writer(writer);

            let sampling_layer = if config.sampling_enabled {
                Some(SamplingLayer::new(
                    config.info_sample_rate,
                    config.debug_sample_rate,
                    config.trace_sample_rate,
                ))
            } else {
                None
            };

            Registry::default()
                .with(filter_layer)
                .with(fmt_layer)
                .with(sampling_layer)
                .init();
        }
    }

    LogReloadHandle {
        reload: reload_handle,
    }
}
