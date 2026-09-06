use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{propagation::TraceContextPropagator, trace::SdkTracerProvider, Resource};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

/// Keeps the provider alive and flushes pending spans during controlled
/// shutdown. Dropping the guard is intentionally best-effort; callers may use
/// [`shutdown`](Self::shutdown) when they need the result.
pub struct TracingGuard {
    provider: Option<SdkTracerProvider>,
}

impl TracingGuard {
    pub fn shutdown(mut self) -> Result<(), String> {
        let Some(provider) = self.provider.take() else {
            return Ok(());
        };
        provider.shutdown().map_err(|error| error.to_string())
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Initialize the node's structured logger and OTLP trace exporter as one
/// subscriber. This preserves JSON/text output, sampling, and the reload
/// handle while adding distributed tracing.
pub fn init_tracing(
    service_name: &str,
    service_instance_id: &str,
    otlp_endpoint: &str,
    config: &common::logging::LoggingConfig,
) -> Result<(common::logging::LogReloadHandle, TracingGuard), String> {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otlp_endpoint.to_string())
        .build()
        .map_err(|error| format!("OTLP span exporter initialization failed: {error}"))?;

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(service_name.to_string())
                .with_attributes([
                    KeyValue::new("service.instance.id", service_instance_id.to_string()),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                ])
                .build(),
        )
        .build();

    let tracer = tracer_provider.tracer(service_name.to_string());
    global::set_tracer_provider(tracer_provider.clone());
    global::set_text_map_propagator(TraceContextPropagator::new());

    let mut directives = config.default_level.clone();
    for (module, level) in &config.module_levels {
        directives.push_str(&format!(",{module}={level}"));
    }
    let env_filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(directives)
    };
    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);
    let writer = common::logging::build_log_writer(&config.output)?;
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let sampling = config.sampling_enabled.then(|| {
        common::logging::SamplingLayer::new(
            config.info_sample_rate,
            config.debug_sample_rate,
            config.trace_sample_rate,
        )
    });

    match config.format {
        common::logging::LogFormat::Json => {
            let formatter = common::logging::NodeJsonFormatter::new(
                config.node_id.clone(),
                config.include_source,
            );
            let format = tracing_subscriber::fmt::layer()
                .event_format(formatter)
                .with_writer(writer);
            Registry::default()
                .with(filter_layer)
                .with(telemetry)
                .with(format)
                .with(sampling)
                .try_init()
                .map_err(|error| format!("tracing subscriber initialization failed: {error}"))?;
        }
        common::logging::LogFormat::Text => {
            let format = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_writer(writer);
            Registry::default()
                .with(filter_layer)
                .with(telemetry)
                .with(format)
                .with(sampling)
                .try_init()
                .map_err(|error| format!("tracing subscriber initialization failed: {error}"))?;
        }
    }

    Ok((
        common::logging::LogReloadHandle::new(reload_handle),
        TracingGuard {
            provider: Some(tracer_provider),
        },
    ))
}
