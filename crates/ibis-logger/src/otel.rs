//! Opt-in OTLP span export (Cargo feature `otel`).
//!
//! The provider lives in a process-wide `OnceLock` so `shutdown_telemetry`
//! can flush it from `main`. The exporter never writes to stdout: transport
//! errors are reported through `tracing` (the `internal-logs` feature of the
//! OpenTelemetry crates routes them to the same subscriber).

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig};
use opentelemetry_sdk::trace::{Sampler, SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::filter::FilterFn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer as _;

use crate::{OtlpProtocol, ResolvedOtlp, TelemetryConfig};

static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();

struct TelemetryGuard {
    provider: SdkTracerProvider,
}

/// Composes the OTLP layer onto `registry` (when enabled) and installs the
/// global default. `None` from the builder means the exporter could not be
/// constructed: the plain `fmt` registry is installed instead.
pub(crate) fn install<S>(registry: S, telemetry: &TelemetryConfig)
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
{
    let mut build_error: Option<String> = None;
    let layer = if telemetry.otlp.enabled && GUARD.get().is_none() {
        if tokio::runtime::Handle::try_current().is_err() {
            build_error = Some("the OTLP exporter requires a Tokio runtime".to_string());
            None
        } else {
            match build_layer::<S>(telemetry) {
                Ok((layer, guard)) => {
                    let _ = GUARD.set(guard);
                    Some(layer)
                }
                Err(err) => {
                    build_error = Some(err.to_string());
                    None
                }
            }
        }
    } else {
        None
    };

    // §4.4: only span attributes leave the process. `tracing` events inside
    // a span would otherwise be exported as OTel span events carrying their
    // fields (potentially content), so the OTel layer filters events out.
    let layer = layer.map(|layer| layer.with_filter(FilterFn::new(|meta| meta.is_span())));
    let installed = tracing::subscriber::set_global_default(registry.with(layer)).is_ok();

    // The subscriber is installed now, so warnings reach the configured sink.
    if let Some(err) = build_error {
        tracing::warn!("OpenTelemetry exporter could not be built: {err}; telemetry stays off");
    }
    if installed && telemetry.otlp.enabled {
        // Re-run normalization only to surface its warnings; the values used
        // by the layer were resolved before installation.
        let _ = telemetry.otlp.resolved();
    }
}

fn build_layer<S>(
    telemetry: &TelemetryConfig,
) -> Result<
    (OpenTelemetryLayer<S, SdkTracer>, TelemetryGuard),
    opentelemetry_otlp::ExporterBuildError,
>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let resolved = telemetry.otlp.resolved();
    let exporter = build_exporter(&resolved)?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            resolved.sample_ratio,
        ))))
        .with_resource(
            Resource::builder()
                .with_service_name(resolved.service_name.clone())
                .build(),
        )
        .build();
    let tracer = provider.tracer("ibis");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Ok((layer, TelemetryGuard { provider }))
}

fn build_exporter(
    resolved: &ResolvedOtlp,
) -> Result<SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    let builder = SpanExporter::builder();
    match resolved.protocol {
        OtlpProtocol::HttpProtobuf => builder
            .with_http()
            .with_endpoint(resolved.endpoint.clone())
            .with_headers(resolved.headers.clone())
            .with_timeout(Duration::from_secs(5))
            .build(),
        OtlpProtocol::Grpc => builder
            .with_tonic()
            .with_endpoint(resolved.endpoint.clone())
            .with_metadata(tonic_metadata(&resolved.headers))
            .with_timeout(Duration::from_secs(5))
            .build(),
    }
}

/// Converts the static config headers into tonic metadata. Invalid keys or
/// values are skipped with a warning (they would otherwise fail the export
/// silently at the first batch).
fn tonic_metadata(
    headers: &std::collections::HashMap<String, String>,
) -> tonic::metadata::MetadataMap {
    use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};

    let mut metadata = MetadataMap::new();
    for (name, raw) in headers {
        let key = match MetadataKey::from_bytes(name.as_bytes()) {
            Ok(key) => key,
            Err(_) => {
                tracing::warn!(
                    "telemetry.otlp.headers key '{name}' is not valid gRPC metadata; skipped"
                );
                continue;
            }
        };
        let value = match MetadataValue::try_from(raw.as_str()) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(
                    "telemetry.otlp.headers value for '{name}' is not valid gRPC metadata; skipped"
                );
                continue;
            }
        };
        metadata.insert(key, value);
    }
    metadata
}

/// Flushes pending spans, bounded by `timeout`. No provider → no-op.
pub(crate) fn shutdown(timeout: Duration) {
    if let Some(guard) = GUARD.get() {
        if let Err(err) = guard.provider.shutdown_with_timeout(timeout) {
            tracing::warn!("OpenTelemetry shutdown failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};

    use super::*;

    /// Minimal in-memory exporter: the `testing` feature of the SDK would
    /// pull metrics/logs into the opt-in build, so the test implements the
    /// two required methods itself.
    #[derive(Debug, Clone, Default)]
    struct VecExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl SpanExporter for VecExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.spans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(batch);
            Ok(())
        }
    }

    fn provider_with(exporter: VecExporter, ratio: f64) -> SdkTracerProvider {
        SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                ratio,
            ))))
            .build()
    }

    fn record_one_span(provider: &SdkTracerProvider) {
        let tracer = provider.tracer("test");
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(FilterFn::new(|meta| meta.is_span())),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("mcp.tool", "mcp.tool.name" = "memory_search");
            let _guard = span.enter();
        });
    }

    #[test]
    fn in_memory_exporter_receives_one_span() {
        let exporter = VecExporter::default();
        let provider = provider_with(exporter.clone(), 1.0);
        record_one_span(&provider);

        let spans = exporter.spans.lock().unwrap().clone();
        assert_eq!(spans.len(), 1, "expected exactly one exported span");
        assert_eq!(spans[0].name, "mcp.tool");
        let name = spans[0]
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "mcp.tool.name")
            .expect("mcp.tool.name attribute");
        assert_eq!(name.value.as_str().as_ref(), "memory_search");
        provider.shutdown().unwrap();
    }

    #[test]
    fn sampled_out_spans_are_not_exported() {
        let exporter = VecExporter::default();
        let provider = provider_with(exporter.clone(), 0.0);
        record_one_span(&provider);
        assert!(exporter.spans.lock().unwrap().is_empty());
        provider.shutdown().unwrap();
    }

    /// §4.4: a `tracing` event inside a span must not become an OTel span
    /// event (its fields could carry content).
    #[test]
    fn span_events_are_not_exported() {
        let exporter = VecExporter::default();
        let provider = provider_with(exporter.clone(), 1.0);
        let tracer = provider.tracer("test");
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(FilterFn::new(|meta| meta.is_span())),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("mcp.tool", "mcp.tool.name" = "memory_search");
            let _guard = span.enter();
            tracing::info!(content = "secret prompt content", "event inside span");
        });

        let spans = exporter.spans.lock().unwrap().clone();
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0].events.is_empty(),
            "span events leaked: {:?}",
            spans[0].events
        );
        provider.shutdown().unwrap();
    }

    #[tokio::test]
    async fn exporter_builds_for_both_protocols() {
        let base = ResolvedOtlp {
            endpoint: "http://127.0.0.1:4318".into(),
            protocol: OtlpProtocol::HttpProtobuf,
            service_name: "ibis".into(),
            sample_ratio: 1.0,
            headers: std::collections::HashMap::new(),
        };
        assert!(build_exporter(&base).is_ok(), "http/protobuf exporter");
        let grpc = ResolvedOtlp {
            protocol: OtlpProtocol::Grpc,
            ..base
        };
        assert!(build_exporter(&grpc).is_ok(), "grpc exporter");
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_spans() {
        use opentelemetry::trace::{Span as _, Tracer as _};

        let exporter = VecExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter.clone())
            .with_sampler(Sampler::AlwaysOn)
            .build();
        let tracer = provider.tracer("test");
        tracer.start("pending").end();
        provider
            .shutdown_with_timeout(Duration::from_secs(5))
            .expect("flush pending spans");
        assert_eq!(exporter.spans.lock().unwrap().len(), 1);
    }
}
