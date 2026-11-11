//! Leveled logging with stdout-quiet mode for stdio plugin children
//! Drivers write to stderr and/or the app log file —
//! stdout is never touched so the MCP stdio framing stays clean.
//!
//! F17 adds an opt-in OpenTelemetry exporter behind the `otel` Cargo
//! feature. The feature is off by default: the standard build does not link
//! the OTel stack, and `init` keeps the same `fmt` output either way.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{IsTerminal, Write as _};
#[cfg(not(feature = "otel"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing_subscriber::layer::SubscriberExt;

pub const DEFAULT_OTLP_ENDPOINT: &str = "http://127.0.0.1:4318";
pub const DEFAULT_OTLP_PROTOCOL: &str = "http/protobuf";
pub const DEFAULT_OTLP_SERVICE_NAME: &str = "ibis";
pub const DEFAULT_OTLP_SAMPLE_RATIO: f64 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Mirrors the original SetLevel switch: unknown strings fall back to INFO.
    pub fn parse(level_str: &str) -> Level {
        match level_str.to_uppercase().as_str() {
            "DEBUG" => Level::Debug,
            "INFO" => Level::Info,
            "WARN" => Level::Warn,
            "ERROR" => Level::Error,
            _ => Level::Info,
        }
    }
}

/// F17 — root `telemetry` block. Only OTLP span export is supported in v1;
/// metrics and log export are deliberately out of scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TelemetryConfig {
    pub otlp: OtlpConfig,
}

/// F17 — `telemetry.otlp`. Raw values as written in the config file; the
/// documented normalization (endpoint fallback, protocol fallback, ratio
/// clamp, service-name trim/cap) happens in [`OtlpConfig::resolved`].
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OtlpConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub protocol: String,
    pub service_name: String,
    pub sample_ratio: f64,
    pub headers: HashMap<String, String>,
}

/// `headers` may carry collector credentials: `Debug` redacts the values so
/// a config dump can never leak them.
impl std::fmt::Debug for OtlpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtlpConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field("protocol", &self.protocol)
            .field("service_name", &self.service_name)
            .field("sample_ratio", &self.sample_ratio)
            .field(
                "headers",
                &format_args!("<{} redacted>", self.headers.len()),
            )
            .finish()
    }
}

impl Default for OtlpConfig {
    fn default() -> Self {
        OtlpConfig {
            enabled: false,
            endpoint: DEFAULT_OTLP_ENDPOINT.to_string(),
            protocol: DEFAULT_OTLP_PROTOCOL.to_string(),
            service_name: DEFAULT_OTLP_SERVICE_NAME.to_string(),
            sample_ratio: DEFAULT_OTLP_SAMPLE_RATIO,
            headers: HashMap::new(),
        }
    }
}

#[cfg_attr(not(feature = "otel"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OtlpProtocol {
    HttpProtobuf,
    Grpc,
}

#[cfg_attr(not(feature = "otel"), allow(dead_code))]
impl OtlpProtocol {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "http/protobuf" => Some(OtlpProtocol::HttpProtobuf),
            "grpc" => Some(OtlpProtocol::Grpc),
            _ => None,
        }
    }
}

/// Normalized exporter settings consumed by the `otel` module.
#[cfg_attr(not(feature = "otel"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedOtlp {
    pub(crate) endpoint: String,
    pub(crate) protocol: OtlpProtocol,
    pub(crate) service_name: String,
    pub(crate) sample_ratio: f64,
    pub(crate) headers: HashMap<String, String>,
}

#[cfg_attr(not(feature = "otel"), allow(dead_code))]
impl OtlpConfig {
    /// Applies the documented normalization. Anomalies are warned once per
    /// `init` (the subscriber is already installed when this runs).
    pub(crate) fn resolved(&self) -> ResolvedOtlp {
        let protocol = match OtlpProtocol::parse(&self.protocol) {
            Some(protocol) => protocol,
            None => {
                tracing::warn!(
                    "telemetry.otlp.protocol '{}' is unknown; using '{}'",
                    self.protocol,
                    DEFAULT_OTLP_PROTOCOL
                );
                OtlpProtocol::HttpProtobuf
            }
        };

        let sample_ratio = if self.sample_ratio.is_nan() {
            tracing::warn!(
                "telemetry.otlp.sample_ratio is NaN; using {}",
                DEFAULT_OTLP_SAMPLE_RATIO
            );
            DEFAULT_OTLP_SAMPLE_RATIO
        } else if self.sample_ratio < 0.0 {
            tracing::warn!("telemetry.otlp.sample_ratio < 0; clamping to 0");
            0.0
        } else if self.sample_ratio > 1.0 {
            tracing::warn!("telemetry.otlp.sample_ratio > 1; clamping to 1");
            1.0
        } else {
            self.sample_ratio
        };

        let endpoint = self.endpoint.trim();
        let endpoint = if endpoint.is_empty() {
            DEFAULT_OTLP_ENDPOINT.to_string()
        } else {
            endpoint.to_string()
        };

        let service_name = self.service_name.trim();
        let service_name: String = service_name.chars().take(64).collect();
        let service_name = if service_name.is_empty() {
            DEFAULT_OTLP_SERVICE_NAME.to_string()
        } else {
            service_name
        };

        ResolvedOtlp {
            endpoint,
            protocol,
            service_name,
            sample_ratio,
            headers: self.headers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
enum Sink {
    Discard,
    Stderr,
    File(Arc<Mutex<Option<std::fs::File>>>),
    Multi(Arc<Mutex<Option<std::fs::File>>>),
}

impl Sink {
    fn write_line(&self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Discard => Ok(()),
            Sink::Stderr => std::io::stderr().write_all(buf),
            Sink::File(f) => {
                let mut guard = f.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(file) = guard.as_mut() {
                    return file.write_all(buf);
                }
                Ok(())
            }
            Sink::Multi(f) => {
                let mut guard = f.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(file) = guard.as_mut() {
                    file.write_all(buf)?;
                }
                std::io::stderr().write_all(buf)
            }
        }
    }
}

struct SinkWriter(Sink);

impl std::io::Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_line(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Sink::File(f) | Sink::Multi(f) = &self.0 {
            let mut guard = f.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(file) = guard.as_mut() {
                return file.flush();
            }
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = SinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter(clone_sink(self))
    }
}

fn clone_sink(sink: &Sink) -> Sink {
    match sink {
        Sink::Discard => Sink::Discard,
        Sink::Stderr => Sink::Stderr,
        Sink::File(f) => Sink::File(Arc::clone(f)),
        Sink::Multi(f) => Sink::Multi(Arc::clone(f)),
    }
}

fn open_log_file(log_file: &str) -> std::io::Result<std::fs::File> {
    OpenOptions::new().append(true).create(true).open(log_file)
}

/// Selects the writer sink mirroring the original implementation:
/// - `file_only` with no `log_file` → discard everything;
/// - `log_file` set → file only when `file_only` or the process is not
///   attached to a terminal (service mode), else stderr + file;
/// - otherwise stderr.
fn build_sink(log_file: &str, file_only: bool) -> std::io::Result<Sink> {
    if log_file.is_empty() {
        return Ok(if file_only {
            Sink::Discard
        } else {
            Sink::Stderr
        });
    }
    let file = open_log_file(log_file)?;
    let shared = Arc::new(Mutex::new(Some(file)));
    if file_only || !std::io::stderr().is_terminal() {
        Ok(Sink::File(shared))
    } else {
        Ok(Sink::Multi(shared))
    }
}

/// The `fmt` layer keeps the pre-F17 configuration byte for byte: no ANSI,
/// no target, custom sink. Kept as a function so the fixture test exercises
/// the same layer the runtime installs.
fn build_fmt_layer<S, W>(writer: W) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(writer)
}

/// Warns once when the config asks for OTLP but the binary was built without
/// the `otel` feature. Returns `true` only for the first call.
#[cfg(not(feature = "otel"))]
fn warn_missing_otel_support_once() -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return false;
    }
    tracing::warn!(
        "telemetry.otlp.enabled is true but this build has no otel support \
         (feature 'otel'); telemetry stays off"
    );
    true
}

/// Initializes the global tracing subscriber using `build_sink`'s writer
/// selection. Idempotent: the first call wins (process-global default).
///
/// F17: `telemetry.otlp.enabled` mounts the OTLP layer when the `otel`
/// feature is compiled in; otherwise it logs a single explicit warning.
pub fn init(
    log_file: &str,
    level_str: &str,
    file_only: bool,
    telemetry: &TelemetryConfig,
) -> std::io::Result<()> {
    let level = Level::parse(level_str);
    let max = match level {
        Level::Debug => tracing::Level::DEBUG,
        Level::Info => tracing::Level::INFO,
        Level::Warn => tracing::Level::WARN,
        Level::Error => tracing::Level::ERROR,
    };

    let sink = build_sink(log_file, file_only)?;

    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(max))
        .with(build_fmt_layer(sink));

    #[cfg(feature = "otel")]
    {
        crate::otel::install(registry, telemetry);
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = tracing::subscriber::set_global_default(registry);
        if telemetry.otlp.enabled {
            warn_missing_otel_support_once();
        }
    }
    Ok(())
}

/// F17 — flushes pending spans and shuts the OTLP provider down. Bounded by
/// `timeout`; a failed flush is a warning, never a fatal error. Without a
/// provider (feature off, exporter disabled, or init not called) it is a
/// no-op.
pub fn shutdown_telemetry(timeout: Duration) {
    #[cfg(feature = "otel")]
    {
        crate::otel::shutdown(timeout);
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = timeout;
    }
}

#[cfg(feature = "otel")]
mod otel;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parsing_defaults_to_info() {
        assert_eq!(Level::parse("DEBUG"), Level::Debug);
        assert_eq!(Level::parse("info"), Level::Info);
        assert_eq!(Level::parse("WARN"), Level::Warn);
        assert_eq!(Level::parse("ERROR"), Level::Error);
        assert_eq!(Level::parse("whatever"), Level::Info);
    }

    #[test]
    fn init_discards_when_file_only_without_file() {
        init("", "INFO", true, &TelemetryConfig::default()).expect("init discard mode");
    }

    #[test]
    fn file_only_sink_writes_to_file_only() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("app.log");
        let sink = build_sink(log_path.to_str().unwrap(), true).unwrap();
        sink.write_line(b"[INFO] file mode message\n").unwrap();
        // Quiet mode: the file carries the line (stdout/stderr untouched).
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            contents.contains("file mode message"),
            "log file missing message: {contents:?}"
        );
    }

    #[test]
    fn multi_sink_writes_file_and_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("app.log");
        let sink = build_sink(log_path.to_str().unwrap(), false).unwrap();
        sink.write_line(b"[INFO] multi mode message\n").unwrap();
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("multi mode message"));
    }

    /// stdio framing: with the discard sink a log line reaches no stream.
    #[test]
    fn stdio_stdout_stays_empty() {
        let sink = build_sink("", true).unwrap();
        sink.write_line(b"[INFO] dropped\n").unwrap();
        assert!(matches!(sink, Sink::Discard));
    }

    #[test]
    fn config_defaults_off() {
        let cfg = TelemetryConfig::default();
        assert!(!cfg.otlp.enabled);
        assert_eq!(cfg.otlp.endpoint, DEFAULT_OTLP_ENDPOINT);
        assert_eq!(cfg.otlp.protocol, DEFAULT_OTLP_PROTOCOL);
        assert_eq!(cfg.otlp.service_name, DEFAULT_OTLP_SERVICE_NAME);
        assert!((cfg.otlp.sample_ratio - DEFAULT_OTLP_SAMPLE_RATIO).abs() < f64::EPSILON);
        assert!(cfg.otlp.headers.is_empty());
    }

    #[test]
    fn protocol_unknown_falls_back() {
        let unknown = OtlpConfig {
            protocol: "carrier-pigeon".into(),
            ..Default::default()
        };
        assert_eq!(unknown.resolved().protocol, OtlpProtocol::HttpProtobuf);

        let grpc = OtlpConfig {
            protocol: " GRPC ".into(),
            ..Default::default()
        };
        assert_eq!(grpc.resolved().protocol, OtlpProtocol::Grpc);

        let http = OtlpConfig {
            protocol: "HTTP/Protobuf".into(),
            ..Default::default()
        };
        assert_eq!(http.resolved().protocol, OtlpProtocol::HttpProtobuf);
    }

    #[test]
    fn sample_ratio_clamped() {
        let low = OtlpConfig {
            sample_ratio: -1.0,
            ..Default::default()
        };
        assert_eq!(low.resolved().sample_ratio, 0.0);

        let high = OtlpConfig {
            sample_ratio: 2.0,
            ..Default::default()
        };
        assert_eq!(high.resolved().sample_ratio, 1.0);

        let nan = OtlpConfig {
            sample_ratio: f64::NAN,
            ..Default::default()
        };
        assert_eq!(nan.resolved().sample_ratio, DEFAULT_OTLP_SAMPLE_RATIO);
    }

    #[test]
    fn endpoint_and_service_name_normalized() {
        let cfg = OtlpConfig {
            endpoint: "   ".into(),
            service_name: format!("  {}  ", "s".repeat(80)),
            ..Default::default()
        };
        let resolved = cfg.resolved();
        assert_eq!(resolved.endpoint, DEFAULT_OTLP_ENDPOINT);
        assert_eq!(resolved.service_name.chars().count(), 64);
        assert_eq!(resolved.service_name, "s".repeat(64));

        let empty = OtlpConfig {
            service_name: "   ".into(),
            ..Default::default()
        };
        assert_eq!(empty.resolved().service_name, DEFAULT_OTLP_SERVICE_NAME);
    }

    #[test]
    fn telemetry_config_deserializes_partial_otlp_block() {
        let cfg: TelemetryConfig =
            serde_json::from_str(r#"{"otlp":{"enabled":true,"headers":{"x-api-key":"secret"}}}"#)
                .unwrap();
        assert!(cfg.otlp.enabled);
        assert_eq!(cfg.otlp.protocol, DEFAULT_OTLP_PROTOCOL);
        assert_eq!(
            cfg.otlp.headers.get("x-api-key").map(String::as_str),
            Some("secret")
        );
    }

    #[cfg(not(feature = "otel"))]
    #[test]
    fn feature_off_warns_once() {
        assert!(warn_missing_otel_support_once());
        assert!(!warn_missing_otel_support_once());
    }

    #[test]
    fn shutdown_without_provider_is_noop() {
        shutdown_telemetry(Duration::from_millis(10));
    }

    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The `fmt` layer keeps the pre-F17 shape: no ANSI, no target.
    #[test]
    fn fmt_output_is_unchanged() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::INFO)
            .with(build_fmt_layer(CaptureWriter(Arc::clone(&buffer))));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("fmt fixture line");
        });
        let output = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(output.contains(" INFO fmt fixture line"), "{output:?}");
        assert!(!output.contains('\u{1b}'), "ANSI escape leaked: {output:?}");
        assert!(!output.contains("ibis_logger"), "target leaked: {output:?}");
    }
}
