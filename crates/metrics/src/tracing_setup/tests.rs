//! Exercise the actual SDK visitor/export pipeline, not just a numeric cast.
use opentelemetry::{trace::TracerProvider as _, Value};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{SdkTracerProvider, SpanData, SpanExporter},
};
use std::sync::{Arc, Mutex};
use tracing_subscriber::prelude::*;

#[derive(Clone, Debug, Default)]
struct Capture {
    spans: Arc<Mutex<Vec<SpanData>>>,
}

impl SpanExporter for Capture {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.spans.lock().unwrap().extend(batch);
        Ok(())
    }
}

#[test]
fn production_http_status_exports_as_integer_and_absent_response_has_no_attribute() {
    let capture = Capture::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(capture.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer().with_tracer(provider.tracer("http-status-regression")),
    );
    tracing::subscriber::with_default(subscriber, || {
        for status in [200_u16, 429, 0] {
            let span = tracing::info_span!(
                "http.server.request",
                http.response.status_code = tracing::field::Empty,
                app_id = "agents/status-test:v1",
            );
            // This is the same production function called by proxy logging.
            common::logging::record_http_response_status(&span, status);
        }
    });
    provider.force_flush().unwrap();
    let spans = capture.spans.lock().unwrap();
    assert_eq!(spans.len(), 3);
    for (span, expected) in spans.iter().zip([Some(200_i64), Some(429), None]) {
        let actual = span
            .attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == "http.response.status_code")
            .map(|attribute| &attribute.value);
        match expected {
            Some(expected) => assert_eq!(
                actual,
                Some(&Value::I64(expected)),
                "status must reach the exporter as an OTLP integer, never a string"
            ),
            None => assert!(
                actual.is_none(),
                "no response must not invent HTTP status zero"
            ),
        }
    }
    drop(spans);
    provider.shutdown().unwrap();
}
