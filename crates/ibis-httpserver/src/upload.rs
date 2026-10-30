//! Log upload endpoint
//! (`logUploadHandler`, kept in `ibis-httpserver` per the module map).
//!
//! Contract:
//! - 405 unless POST ("Method not allowed");
//! - 403 when `log_ingestion.http.enabled=false`;
//! - `X-API-Key` must match `log_ingestion.http.api_key` (missing/empty or
//!   wrong → 401);
//! - body cap 32 MiB ([`ibis_ingest_logs::MAX_LOG_INGEST_BYTES`]): an
//!   oversized body is rejected with a **plain-text** 413
//!   `Request body exceeds <N> byte limit` (not JSON);
//! - JSON `{project, file_name, lines[], text}` or raw body with
//!   `?project=&file_name=`; defaults `HTTPUpload` / `uploaded.log`;
//! - runs the analysis synchronously through the [`LogUploadAnalyzer`] seam
//!   (default: `ibis-ingest-logs` `LogAnalyzer`);
//! - success `{status:"processed", project, file_name, anomalies[]}`;
//!   `truncated:true` + `warning` are added **only** when the raw body was
//!   capped (success-response fields, not part of the 413 protocol).

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use http_body_util::BodyExt;
use ibis_core::config::Config;
use ibis_ingest_logs::{LogAnalyzer, Notifier, SemanticError, MAX_LOG_INGEST_BYTES};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use ibis_ai::Client;
use ibis_db::executor::Executor;

/// The synchronous-analysis seam. The default implementation builds an
/// `ibis-ingest-logs` analyzer per request (`NewLogAnalyzerWithProject` +
/// `ProcessBatchSync`); tests substitute fakes here.
#[async_trait::async_trait]
pub trait LogUploadAnalyzer: Send + Sync {
    /// Runs the analysis; `Err` carries the message used for the
    /// `Analysis failed: …` 500 body.
    async fn analyze(
        &self,
        project: &str,
        file_name: &str,
        lines: &[String],
    ) -> Result<Vec<SemanticError>, String>;
}

/// Default analyzer backed by `ibis-ingest-logs` ( wiring in
/// ).
pub struct IngestAnalyzer {
    pub cfg: Arc<Config>,
    pub db: Option<Arc<dyn Executor>>,
    pub ai: Option<Arc<Client>>,
}

impl IngestAnalyzer {
    pub fn new(cfg: Arc<Config>, db: Option<Arc<dyn Executor>>, ai: Option<Arc<Client>>) -> Self {
        IngestAnalyzer { cfg, db, ai }
    }
}

/// Replaces path separators/control characters so a hostile `project` /
/// `file_name` cannot traverse out of `logs_root` (audit finding on
/// the value is otherwise only an identifier for
/// the analyzer, but every derived path stays inside the logs root by
/// construction).
pub(crate) fn sanitize_component(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' => '_',
            other => other,
        })
        .collect();
    if sanitized == ".." || sanitized == "." {
        "_".to_string()
    } else {
        sanitized
    }
}

#[async_trait::async_trait]
impl LogUploadAnalyzer for IngestAnalyzer {
    async fn analyze(
        &self,
        project: &str,
        file_name: &str,
        lines: &[String],
    ) -> Result<Vec<SemanticError>, String> {
        let path = std::path::Path::new(self.cfg.logs_root.trim())
            .join(sanitize_component(project))
            .join(sanitize_component(file_name))
            .to_string_lossy()
            .into_owned();
        let analyzer = LogAnalyzer::new_with_project(
            &path,
            project,
            Some(self.cfg.clone()),
            self.db.clone(),
            self.ai.clone(),
        )
        .await;
        let (anomalies, err) = analyzer.process_batch_sync(lines).await;
        match err {
            Some(e) => Err(e.to_string()),
            None => Ok(anomalies),
        }
    }
}

#[derive(Clone)]
struct UploadState {
    cfg: Arc<Config>,
    analyzer: Arc<dyn LogUploadAnalyzer>,
    notifier_db: Option<Arc<dyn Executor>>,
    max_bytes: usize,
}

/// JSON upload payload.
#[derive(Debug, Default, Deserialize)]
struct LogUploadRequest {
    #[serde(default)]
    project: String,
    #[serde(rename = "file_name", default)]
    file_name: String,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    text: String,
}

/// Builds the upload router mounted at `/api/v1/logs/upload` ( wiring:
/// `mux.Handle("/", logUploadHandler(cfg, dbClient, aiClient))`).
/// The router exposes the handler at `/`; the mux nests it.
pub fn log_upload_handler(
    cfg: Arc<Config>,
    db: Option<Arc<dyn Executor>>,
    ai: Option<Arc<Client>>,
) -> Router {
    log_upload_with_analyzer(
        cfg.clone(),
        db.clone(),
        Some(Arc::new(IngestAnalyzer::new(cfg, db, ai))),
        None,
    )
}

/// Test/wiring variant: explicit analyzer seam and body cap.
pub fn log_upload_with_analyzer(
    cfg: Arc<Config>,
    db: Option<Arc<dyn Executor>>,
    analyzer: Option<Arc<dyn LogUploadAnalyzer>>,
    max_bytes: Option<usize>,
) -> Router {
    let analyzer =
        analyzer.unwrap_or_else(|| Arc::new(IngestAnalyzer::new(cfg.clone(), db.clone(), None)));
    let state = UploadState {
        notifier_db: db,
        max_bytes: max_bytes.unwrap_or(MAX_LOG_INGEST_BYTES),
        analyzer,
        cfg,
    };
    Router::new()
        .route("/", any(upload_entry))
        .with_state(state)
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    // Scenario: http.Error: text/plain, trailing newline, nosniff.
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        format!("{message}\n"),
    )
        .into_response()
}

async fn upload_entry(State(st): State<UploadState>, req: Request) -> Response {
    // `if r.Method != http.MethodPost` → 405.
    if req.method() != Method::POST {
        return plain_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    if !st.cfg.log_ingestion.http.enabled {
        return plain_error(
            StatusCode::FORBIDDEN,
            "HTTP Log Ingestion is disabled in configuration",
        );
    }
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if api_key.is_empty() || api_key != st.cfg.log_ingestion.http.api_key {
        return plain_error(
            StatusCode::UNAUTHORIZED,
            "Unauthorized: invalid or missing API key",
        );
    }

    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let mut project;
    let mut file_name;
    let lines: Vec<String>;
    let mut truncated = false;

    if content_type.contains("application/json") {
        let (body, over) = match read_body_capped(req.into_body(), st.max_bytes).await {
            Ok(pair) => pair,
            Err(e) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        };
        // the capped body makes the JSON decode fail → plain-text 413.
        if over {
            return too_large(st.max_bytes);
        }
        let req_payload: LogUploadRequest = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return plain_error(
                    StatusCode::BAD_REQUEST,
                    &format!("Failed to parse JSON body: {e}"),
                )
            }
        };
        project = req_payload.project;
        file_name = req_payload.file_name;
        if !req_payload.text.is_empty() {
            lines = req_payload.text.split('\n').map(str::to_string).collect();
        } else {
            lines = req_payload.lines;
        }
    } else {
        let query: Vec<(String, String)> = req
            .uri()
            .query()
            .map(|q| {
                q.split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(k, v)| (k.to_string(), urldecode(v)))
                    .collect()
            })
            .unwrap_or_default();
        project = query
            .iter()
            .find(|(k, _)| k == "project")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        file_name = query
            .iter()
            .find(|(k, _)| k == "file_name")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let (body, over) = match read_body_capped(req.into_body(), st.max_bytes).await {
            Ok(pair) => pair,
            Err(e) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        };
        truncated = over;
        if truncated {
            tracing::warn!(
                "HTTP log upload truncated at {} bytes (project={project} file={file_name})",
                st.max_bytes
            );
        }
        lines = String::from_utf8_lossy(&body)
            .split('\n')
            .map(str::to_string)
            .collect();
    }

    if project.is_empty() {
        project = "HTTPUpload".to_string();
    }
    if file_name.is_empty() {
        file_name = "uploaded.log".to_string();
    }

    let clean_lines: Vec<String> = lines
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if clean_lines.is_empty() {
        // success without touching the analyzer/DB
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({
                "status": "processed",
                "project": project,
                "file_name": file_name,
                "anomalies": [],
            })
            .to_string(),
        )
            .into_response();
    }

    let anomalies = match st
        .analyzer
        .analyze(&project, &file_name, &clean_lines)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("HTTP log upload analysis error: {e}");
            return plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Analysis failed: {e}"),
            );
        }
    };

    if !anomalies.is_empty() {
        let notifier = Notifier::new(st.cfg.clone(), st.notifier_db.clone());
        let mut max_severity = 0i64;
        for e in &anomalies {
            if e.severity > max_severity {
                max_severity = e.severity;
            }
        }
        let report = build_upload_report(&project, &file_name, &anomalies);
        if let Err(e) = notifier
            .queue_notification(&project, &file_name, anomalies.len(), max_severity, &report)
            .await
        {
            // Scenario: ignored notifier errors silently; this crate logs them.
            tracing::warn!("Upload notification failed: {e}");
        }
    }

    let mut resp = json!({
        "status": "processed",
        "project": project,
        "file_name": file_name,
        "anomalies": anomalies,
    });
    if truncated {
        resp["truncated"] = json!(true);
        resp["warning"] = json!(format!(
            "Body truncated at {} bytes; analysis covers the capped prefix only",
            st.max_bytes
        ));
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        resp.to_string(),
    )
        .into_response()
}

fn too_large(max_bytes: usize) -> Response {
    plain_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        &format!("Request body exceeds {max_bytes} byte limit"),
    )
}

/// The upload digest queued with the notifier.
fn build_upload_report(project: &str, file_name: &str, anomalies: &[SemanticError]) -> String {
    let mut sb = String::new();
    sb.push_str(&format!(
        "HTTP Upload Log Report for Project: {project} (File: {file_name})\r\n\r\n"
    ));
    sb.push_str(&format!(
        "Detected {} errors/anomalies:\r\n\r\n",
        anomalies.len()
    ));
    for e in anomalies {
        sb.push_str(&format!("Category: {}\r\n", e.category));
        sb.push_str(&format!("Severity: {}/10\r\n", e.severity));
        if !e.file.is_empty() {
            sb.push_str(&format!("File: {}\r\n", e.file));
        }
        if !e.cause.is_empty() {
            sb.push_str(&format!("Cause: {}\r\n", e.cause));
        }
        sb.push_str("Stack Trace:\r\n");
        sb.push_str(&format!("{}\r\n\r\n", e.stack_trace));
    }
    sb
}

/// Reads the request body capped at `max` bytes, mirroring `logs.ReadCapped
/// with `http.MaxBytesReader`: when the stream continues past the cap, the
/// returned flag is true, and the buffer holds only the capped prefix.
async fn read_body_capped(body: Body, max: usize) -> Result<(Vec<u8>, bool), String> {
    let mut body = body;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some(frame) = body.frame().await else {
            return Ok((buf, false));
        };
        let data = frame
            .map_err(|e| e.to_string())?
            .into_data()
            .map_err(|_| "unsupported body frame".to_string())?;
        let remaining = max.saturating_sub(buf.len());
        if data.len() > remaining {
            buf.extend_from_slice(&data[..remaining]);
            return Ok((buf, true));
        }
        buf.extend_from_slice(&data);
    }
}

/// Minimal percent-decoding for query values (`Query.Get semantics for
/// the shapes used here).
fn urldecode(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as AxumBody;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Inputs captured by the fake analyzer: (project, file_name, lines).
    type Captured = (String, String, Vec<String>);

    fn cfg() -> Arc<Config> {
        let mut config = Config::default();
        config.log_ingestion.http.enabled = true;
        config.log_ingestion.http.api_key = "secret-key".into();
        Arc::new(config)
    }

    fn capture() -> Arc<Mutex<Option<Captured>>> {
        Arc::new(Mutex::new(None))
    }

    /// Test-only analyzer that records its inputs and returns a canned result.
    struct FakeAnalyzer {
        result: Result<Vec<SemanticError>, String>,
        seen: Arc<Mutex<Option<Captured>>>,
    }

    #[async_trait::async_trait]
    impl LogUploadAnalyzer for FakeAnalyzer {
        async fn analyze(
            &self,
            project: &str,
            file_name: &str,
            lines: &[String],
        ) -> Result<Vec<SemanticError>, String> {
            if let Ok(mut seen) = self.seen.lock() {
                *seen = Some((project.to_string(), file_name.to_string(), lines.to_vec()));
            }
            self.result.clone()
        }
    }

    fn fake(
        result: Result<Vec<SemanticError>, String>,
        seen: Arc<Mutex<Option<Captured>>>,
    ) -> Arc<dyn LogUploadAnalyzer> {
        Arc::new(FakeAnalyzer { result, seen })
    }

    fn router(analyzer: Option<Arc<dyn LogUploadAnalyzer>>, cap: Option<usize>) -> Router {
        log_upload_with_analyzer(cfg(), None, analyzer, cap)
    }

    async fn post(router: Router, uri: &str, headers: &[(&str, &str)], body: &str) -> Response {
        let mut builder = axum::http::Request::builder().method(Method::POST).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        router
            .oneshot(builder.body(AxumBody::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// case 1: missing API key → 401.
    #[tokio::test]
    async fn missing_api_key_unauthorized() {
        let resp = post(
            router(Some(fake(Ok(vec![]), capture())), None),
            "/",
            &[],
            "log line",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_api_key_unauthorized() {
        let resp = post(
            router(Some(fake(Ok(vec![]), capture())), None),
            "/",
            &[("X-API-Key", "nope")],
            "log line",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// case 2: GET → 405.
    #[tokio::test]
    async fn get_method_not_allowed() {
        let request = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/")
            .header("X-API-Key", "secret-key")
            .body(AxumBody::empty())
            .unwrap();
        let resp = router(Some(fake(Ok(vec![]), capture())), None)
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// case 4: ingestion disabled → 403.
    #[tokio::test]
    async fn ingestion_disabled_forbidden() {
        let mut config = (*cfg()).clone();
        config.log_ingestion.http.enabled = false;
        let state = UploadState {
            notifier_db: None,
            max_bytes: MAX_LOG_INGEST_BYTES,
            analyzer: fake(Ok(vec![]), capture()),
            cfg: Arc::new(config),
        };
        let app = Router::new()
            .route("/", any(upload_entry))
            .with_state(state);
        let resp = post(app, "/", &[("X-API-Key", "secret-key")], "log line").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// case 3: JSON body with no functional AI → 500
    /// "Analysis failed: AI client is not functional" (real analyzer seam).
    #[tokio::test]
    async fn json_body_ai_not_functional() {
        let app = log_upload_with_analyzer(
            cfg(),
            None,
            // Default IngestAnalyzer without an AI client mirrors the nil aiClient.
            Some(Arc::new(IngestAnalyzer::new(cfg(), None, None))),
            None,
        );
        let body = json!({
            "project": "project-x",
            "file_name": "upload.log",
            "lines": ["error line 1", "error line 2"],
        });
        let resp = post(
            app,
            "/",
            &[
                ("X-API-Key", "secret-key"),
                ("Content-Type", "application/json"),
            ],
            &body.to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let text = body_string(resp).await;
        assert!(
            text.contains("Analysis failed: AI client is not functional"),
            "expected AI-not-functional body, got {text:?}"
        );
    }

    /// JSON success path: response shape {status, project, file_name, anomalies}.
    #[tokio::test]
    async fn json_body_success_shape() {
        let anomalies = vec![SemanticError {
            category: "timeout".into(),
            stack_trace: "boom".into(),
            severity: 7,
            ..Default::default()
        }];
        let seen = capture();
        let app = router(Some(fake(Ok(anomalies), seen.clone())), None);
        let body = json!({
            "project": "project-x",
            "file_name": "upload.log",
            "lines": ["  error line 1  ", "", "error line 2"],
        });
        let resp = post(
            app,
            "/",
            &[
                ("X-API-Key", "secret-key"),
                ("Content-Type", "application/json"),
            ],
            &body.to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(value["status"], "processed");
        assert_eq!(value["project"], "project-x");
        assert_eq!(value["file_name"], "upload.log");
        assert_eq!(value["anomalies"][0]["category"], "timeout");
        assert_eq!(value["anomalies"][0]["stack_trace"], "boom");
        assert_eq!(value["anomalies"][0]["severity"], 7);
        assert!(value.get("truncated").is_none());
        // Lines are trimmed and empties dropped before analysis (parity).
        let captured = seen.lock().unwrap().clone().unwrap();
        assert_eq!(captured.2, vec!["error line 1", "error line 2"]);
    }

    /// `text` field splits into lines when provided
    #[tokio::test]
    async fn json_text_field_splits_lines() {
        let seen = capture();
        let app = router(Some(fake(Ok(vec![]), seen.clone())), None);
        let body = json!({ "project": "p", "text": "one\ntwo\n" });
        let resp = post(
            app,
            "/",
            &[
                ("X-API-Key", "secret-key"),
                ("Content-Type", "application/json"),
            ],
            &body.to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let captured = seen.lock().unwrap().clone().unwrap();
        assert_eq!(captured.2, vec!["one", "two"]);
    }

    /// Malformed JSON → 400 with the parse message
    #[tokio::test]
    async fn malformed_json_bad_request() {
        let resp = post(
            router(Some(fake(Ok(vec![]), capture())), None),
            "/",
            &[
                ("X-API-Key", "secret-key"),
                ("Content-Type", "application/json"),
            ],
            "{not json",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(body_string(resp)
            .await
            .contains("Failed to parse JSON body"));
    }

    /// Raw body path with query params; empty lines → success with no
    /// analyzer call ( early return).
    #[tokio::test]
    async fn raw_body_empty_lines_success() {
        let seen = capture();
        let app = router(Some(fake(Ok(vec![]), seen.clone())), None);
        let resp = post(
            app,
            "/?project=raw-proj&file_name=app.log",
            &[("X-API-Key", "secret-key")],
            "\n   \n",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(value["project"], "raw-proj");
        assert_eq!(value["file_name"], "app.log");
        assert_eq!(value["anomalies"], json!([]));
        assert!(seen.lock().unwrap().is_none(), "analyzer must not run");
    }

    /// Defaults: HTTPUpload / uploaded.log when the raw request carries none.
    #[tokio::test]
    async fn raw_defaults() {
        let seen = capture();
        let app = router(Some(fake(Ok(vec![]), seen.clone())), None);
        let resp = post(
            app,
            "/",
            &[("X-API-Key", "secret-key")],
            "line one\nline two",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(value["project"], "HTTPUpload");
        assert_eq!(value["file_name"], "uploaded.log");
        let captured = seen.lock().unwrap().clone().unwrap();
        assert_eq!(captured.0, "HTTPUpload");
        assert_eq!(captured.1, "uploaded.log");
        assert_eq!(captured.2, vec!["line one", "line two"]);
    }

    /// Oversized raw body: analysis proceeds on the capped prefix and the
    /// success response carries `truncated:true` + `warning` (the cap is
    /// injected small for the test; the production default is 32 MiB).
    #[tokio::test]
    async fn raw_body_truncated_flag() {
        let seen = capture();
        let app = router(
            Some(fake(
                Ok(vec![SemanticError {
                    category: "err".into(),
                    severity: 3,
                    ..Default::default()
                }]),
                seen.clone(),
            )),
            Some(16),
        );
        let big = "x".repeat(64);
        let resp = post(app, "/", &[("X-API-Key", "secret-key")], &big).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(value["truncated"], json!(true));
        assert!(
            value["warning"]
                .as_str()
                .unwrap()
                .contains("Body truncated at 16 bytes"),
            "warning missing: {value}"
        );
        let captured = seen.lock().unwrap().clone().unwrap();
        assert_eq!(captured.2, vec!["x".repeat(16)]);
    }

    /// Oversized JSON body → plain-text 413 (not JSON), mirroring the
    /// decode-failure protocol
    #[tokio::test]
    async fn json_body_too_large_plain_text_413() {
        let app = router(Some(fake(Ok(vec![]), capture())), Some(16));
        let big = json!({ "project": "p", "lines": ["y".repeat(64)] });
        let resp = post(
            app,
            "/",
            &[
                ("X-API-Key", "secret-key"),
                ("Content-Type", "application/json"),
            ],
            &big.to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.starts_with("text/plain"));
        let text = body_string(resp).await;
        assert_eq!(
            text.trim_end(),
            "Request body exceeds 16 byte limit",
            "413 must be plain text, got {text:?}"
        );
    }

    /// A hostile file_name cannot escape the logs root in the analyzer path
    /// (audit M-04 — sanitize_component).
    #[test]
    fn hostile_names_sanitized() {
        assert_eq!(sanitize_component("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("a\\b:c"), "a_b_c");
        assert_eq!(sanitize_component("plain.log"), "plain.log");
    }

    /// Query values are percent-decoded ( Query.Get).
    #[test]
    fn query_decoding() {
        assert_eq!(urldecode("my%20app"), "my app");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("plain"), "plain");
    }
}
