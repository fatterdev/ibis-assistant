//! Opt-in OTLP span export (Cargo feature `otel`).
//!
//! The provider lives in a process-wide `OnceLock` so `shutdown_telemetry`
//! can flush it from `main`. The exporter never writes to stdout: transport
//! errors are reported through `tracing` (the `internal-logs` feature of the
//! OpenTelemetry crates routes them to the same subscriber).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{
    SpanExporter as OtlpSpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Sampler, SdkTracer, SdkTracerProvider, SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::filter::FilterFn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer as _;

use crate::{
    OtlpProtocol, ResolvedOtlp, TelemetryConfig, SPAN_AI_EMBED, SPAN_AI_GENERATE, SPAN_MCP_TOOL,
    SPAN_MEMORY_CONSOLIDATE,
};

static GUARD: OnceLock<TelemetryGuard> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// True once the OTLP layer is installed in this process.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

struct TelemetryGuard {
    provider: SdkTracerProvider,
}

/// Wraps the OTLP exporter so an unreachable collector produces one warning
/// instead of an ERROR per batch (the SDK's internal log is filtered by the
/// `fmt` layer target list).
#[derive(Debug)]
struct WarnOnceExporter<E> {
    inner: E,
    warned: AtomicBool,
}

impl<E: SpanExporter> SpanExporter for WarnOnceExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        if let Err(err) = &result {
            if !self.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "OpenTelemetry export failed (further export errors suppressed): {err}"
                );
            }
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }
}

fn is_allowed_span(name: &str) -> bool {
    matches!(
        name,
        SPAN_MCP_TOOL | SPAN_AI_GENERATE | SPAN_AI_EMBED | SPAN_MEMORY_CONSOLIDATE
    )
}

/// Composes the OTLP layer onto `registry` (when enabled) and installs the
/// global default. `None` from the builder means the exporter could not be
/// constructed: the plain `fmt` registry is installed instead.
pub(crate) fn install<S>(registry: S, telemetry: &TelemetryConfig)
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
{
    let mut build_error: Option<String> = None;
    let mut header_warnings: Vec<String> = Vec::new();
    let mut guard: Option<TelemetryGuard> = None;
    let layer = if telemetry.otlp.enabled && GUARD.get().is_none() {
        match build_layer::<S>(telemetry) {
            Ok(BuiltLayer {
                layer,
                guard: built_guard,
                warnings,
            }) => {
                guard = Some(built_guard);
                header_warnings = warnings;
                Some(layer)
            }
            Err(err) => {
                build_error = Some(err.to_string());
                None
            }
        }
    } else {
        None
    };

    // §4.4: only the four declared spans leave the process. `tracing` events
    // inside a span would otherwise be exported as OTel span events carrying
    // their fields (potentially content), and foreign INFO+ spans from
    // dependencies would add unbounded names/attributes.
    let layer = layer.map(|layer| {
        layer.with_filter(FilterFn::new(|meta| {
            meta.is_span() && is_allowed_span(meta.name())
        }))
    });
    let layer_built = layer.is_some();
    let installed = tracing::subscriber::set_global_default(registry.with(layer)).is_ok();
    if installed {
        if let Some(guard) = guard {
            let _ = GUARD.set(guard);
        }
        if layer_built {
            ACTIVE.store(true, Ordering::Relaxed);
        }
    }

    // The subscriber is installed now, so warnings reach the configured sink.
    if let Some(err) = build_error {
        tracing::warn!("OpenTelemetry exporter could not be built: {err}; telemetry stays off");
    }
    // The exporter build runs before the subscriber exists: its warnings are
    // buffered and emitted now, or they would be lost (same pattern as the
    // build error above).
    for warning in header_warnings {
        tracing::warn!("{warning}");
    }
    if installed && layer_built {
        // Re-run normalization only to surface its warnings; the values used
        // by the layer were resolved before installation.
        let resolved = telemetry.otlp.resolved();
        tracing::info!(
            "OpenTelemetry exporter enabled: endpoint={} protocol={} sample_ratio={}",
            resolved.endpoint,
            resolved.protocol.as_str(),
            resolved.sample_ratio
        );
    }
}

/// Result of a successful layer build: the OTLP layer, its provider guard,
/// and the warnings buffered until the subscriber is installed.
struct BuiltLayer<S> {
    layer: OpenTelemetryLayer<S, SdkTracer>,
    guard: TelemetryGuard,
    warnings: Vec<String>,
}

fn build_layer<S>(
    telemetry: &TelemetryConfig,
) -> Result<BuiltLayer<S>, opentelemetry_otlp::ExporterBuildError>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let resolved = telemetry.otlp.resolved();
    let (exporter, warnings) = build_exporter(&resolved)?;
    let exporter = WarnOnceExporter {
        inner: exporter,
        warned: AtomicBool::new(false),
    };

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
    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        // §4.3/§4.4: only the declared attributes leave the process. The
        // layer defaults would add code.* (absolute paths), thread.*, target
        // and busy/idle counters.
        .with_location(false)
        .with_threads(false)
        .with_target(false)
        .with_tracked_inactivity(false);
    Ok(BuiltLayer {
        layer,
        guard: TelemetryGuard { provider },
        warnings,
    })
}

fn build_exporter(
    resolved: &ResolvedOtlp,
) -> Result<(OtlpSpanExporter, Vec<String>), opentelemetry_otlp::ExporterBuildError> {
    let builder = OtlpSpanExporter::builder();
    match resolved.protocol {
        OtlpProtocol::HttpProtobuf => Ok((
            builder
                .with_http()
                .with_endpoint(http_traces_endpoint(&resolved.endpoint))
                .with_headers(resolved.headers.clone())
                .with_timeout(Duration::from_secs(5))
                .build()?,
            Vec::new(),
        )),
        OtlpProtocol::Grpc => {
            let (metadata, warnings) = tonic_metadata(&resolved.headers);
            let exporter = builder
                .with_tonic()
                .with_endpoint(resolved.endpoint.clone())
                .with_metadata(metadata)
                .with_timeout(Duration::from_secs(5))
                .build()?;
            Ok((exporter, warnings))
        }
    }
}

/// The HTTP exporter uses a programmatic endpoint verbatim (only the generic
/// env var gets the signal path appended), so add `/v1/traces` ourselves.
fn http_traces_endpoint(endpoint: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if endpoint.ends_with("/v1/traces") {
        endpoint.to_string()
    } else {
        format!("{endpoint}/v1/traces")
    }
}

/// Converts the static config headers into tonic metadata. Invalid keys or
/// values are skipped and returned as warning strings (they would otherwise
/// fail the export silently at the first batch); the caller buffers the
/// warnings until the subscriber is installed, so they are not lost.
fn tonic_metadata(
    headers: &std::collections::HashMap<String, String>,
) -> (tonic::metadata::MetadataMap, Vec<String>) {
    use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};

    let mut metadata = MetadataMap::new();
    let mut warnings = Vec::new();
    for (name, raw) in headers {
        let key = match MetadataKey::from_bytes(name.as_bytes()) {
            Ok(key) => key,
            Err(_) => {
                warnings.push(format!(
                    "telemetry.otlp.headers key '{name}' is not valid gRPC metadata; skipped"
                ));
                continue;
            }
        };
        let value = match MetadataValue::try_from(raw.as_str()) {
            Ok(value) => value,
            Err(_) => {
                warnings.push(format!(
                    "telemetry.otlp.headers value for '{name}' is not valid gRPC metadata; skipped"
                ));
                continue;
            }
        };
        metadata.insert(key, value);
    }
    (metadata, warnings)
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
                .with_filter(FilterFn::new(|meta| {
                    meta.is_span() && is_allowed_span(meta.name())
                })),
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
                .with_filter(FilterFn::new(|meta| {
                    meta.is_span() && is_allowed_span(meta.name())
                })),
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

    #[test]
    fn foreign_spans_are_not_exported() {
        let exporter = VecExporter::default();
        let provider = provider_with(exporter.clone(), 1.0);
        let tracer = provider.tracer("test");
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(FilterFn::new(|meta| {
                    meta.is_span() && is_allowed_span(meta.name())
                })),
        );
        tracing::subscriber::with_default(subscriber, || {
            let foreign = tracing::info_span!("foreign.span", secret = "x");
            let _guard = foreign.enter();
            let allowed = tracing::info_span!("mcp.tool");
            let _guard = allowed.enter();
        });
        let spans = exporter.spans.lock().unwrap().clone();
        assert_eq!(spans.len(), 1, "only the whitelisted span is exported");
        assert_eq!(spans[0].name, "mcp.tool");
        provider.shutdown().unwrap();
    }

    #[test]
    fn http_endpoint_appends_traces_path() {
        assert_eq!(
            http_traces_endpoint("http://127.0.0.1:4318"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            http_traces_endpoint("http://127.0.0.1:4318/"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            http_traces_endpoint("http://collector/v1/traces"),
            "http://collector/v1/traces"
        );
    }

    /// F17: invalid gRPC metadata is skipped with a warning string instead of
    /// a `tracing::warn!` that would fire before the subscriber exists.
    #[test]
    fn grpc_metadata_reports_invalid_headers_without_panicking() {
        let headers = std::collections::HashMap::from([
            ("valid-key".to_string(), "ok".to_string()),
            // Header names normalize their case but must stay a token: a space
            // is invalid.
            ("Bad Key".to_string(), "ok".to_string()),
            // Control bytes are not valid header values.
            ("also-valid".to_string(), "bad\nvalue".to_string()),
        ]);
        let (metadata, warnings) = tonic_metadata(&headers);
        assert_eq!(metadata.len(), 1, "only the valid pair lands in the map");
        assert_eq!(
            warnings.len(),
            2,
            "one warning per skipped pair: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("'Bad Key'")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("'also-valid'")),
            "{warnings:?}"
        );
    }

    /// The buffered warnings must travel with the built layer so `install`
    /// can emit them after the subscriber is mounted. The tonic client needs
    /// a Tokio reactor at build time.
    #[tokio::test]
    async fn build_layer_surfaces_header_warnings() {
        let telemetry = TelemetryConfig {
            otlp: crate::OtlpConfig {
                enabled: true,
                endpoint: "http://127.0.0.1:4317".into(),
                protocol: "grpc".into(),
                headers: std::collections::HashMap::from([("Bad Key".into(), "v".into())]),
                ..Default::default()
            },
        };
        let built =
            build_layer::<tracing_subscriber::Registry>(&telemetry).expect("grpc layer builds");
        assert_eq!(built.warnings.len(), 1, "{:?}", built.warnings);
        assert!(
            built.warnings[0].contains("'Bad Key'"),
            "{:?}",
            built.warnings
        );
    }

    #[test]
    fn build_layer_works_without_tokio_runtime() {
        let telemetry = TelemetryConfig {
            otlp: crate::OtlpConfig {
                enabled: true,
                endpoint: "http://127.0.0.1:1".into(),
                ..Default::default()
            },
        };
        let result = build_layer::<tracing_subscriber::Registry>(&telemetry);
        assert!(result.is_ok(), "build_layer: {:?}", result.err());
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
