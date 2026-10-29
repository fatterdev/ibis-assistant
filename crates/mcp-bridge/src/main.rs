//! mcp-bridge — stdio ↔ Streamable HTTP `/mcp` bridge for Claude Desktop.
//!
//! **Rewritten for Streamable HTTP**
//! per D11 (the legacy `/sse` + `/message` transport is gone from the
//! server, so the bridge speaks `/mcp`).
//!
//! # Framing and session behavior
//!
//! The bridge delegated nothing to the MCP SDK — it was a transparent
//! NDJSON/SSE proxy (bufio.Scanner over stdin, raw POSTs, SSE line parsing).
//! The port mirrors that transparency: Claude Desktop owns the MCP handshake,
//! the bridge only shuttles JSON-RPC frames, so any protocol method (today's
//! and future) passes through without bridge changes. An rmcp typed client
//! would terminate the handshake (its own `initialize`, its own server
//! identity) and break that transparency — hence a plain HTTP client.
//!
//! - **Egress** (Claude → server): one stdin line = one JSON-RPC message,
//!   POSTed to the endpoint (`scanner.Scan` → `POST`), 10 MiB line cap
//!
//! - **Ingress** (server → Claude): JSON response bodies and
//!   `text/event-stream` payloads are forwarded to stdout as NDJSON lines
//!   (SSE `data: ` payloads → `fmt.Println`). Every write is flushed —
//!   Rust buffers piped stdout, did not.
//! - **Session**: the `Mcp-Session-Id` response header is captured after the
//!   client's `initialize` and replayed on later POSTs and on the GET event
//!   stream (replaces the original SSE `endpoint` event that carried the POST URL —
//!   that mechanism is SSE-only and is dead under Streamable HTTP).
//!
//! # CLI (`flag-compatible for the two flags)
//!
//! `text
//! -url <endpoint> Streamable HTTP MCP endpoint.
//! Default http://localhost:3030/mcp (D11 deviation: the
//! default was the legacy http://localhost:3030/sse).
//! -key <api-key> Optional Redmine API key, forwarded as X-Redmine-API-Key
//! on every request (unchanged).
//! `
//!
//! Both `-flag value` and `-flag=value` (and `--flag`) spellings are accepted,
//! mirroring the `flag.Parse`.
//!
//! # Behavioral deltas (documented audit rows)
//!
//! - The 30-second total HTTP timeout on POSTs is dropped : under
//!   Streamable HTTP the tool response arrives in the POST body, so a
//!   long-running tool would break the bridge; only a 10s connect timeout
//!   remains.
//! - The `-url`→`/message` POST-URL guess and the SSE `endpoint` event
//!   handling are removed (single endpoint + session header now).
//! - Empty stdin lines are skipped instead of POSTed as empty bodies.
//! - The 1s reconnect cadence for the server→client stream is kept (parity).

use std::io::Write as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::StreamExt;
use tokio::io::AsyncReadExt;

/// `scanner.Buffer(buf, 10*1024*1024) — stdin/SSE line cap.
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

const DEFAULT_URL: &str = "http://localhost:3030/mcp";

#[derive(Debug, Clone, PartialEq)]
struct Args {
    url: String,
    key: String,
}

/// Shared session-id slot ( stored the POST URL; Streamable HTTP keeps one
/// endpoint and carries the session in a header).
type SessionSlot = Arc<RwLock<Option<String>>>;

/// `flag.Parse-compatible parsing for `-url` / `-key` (also accepts the
/// `--flag` spelling and `-flag=value`). `argv[0]` is stripped.
fn parse_args<I: IntoIterator<Item = String>>(argv: I) -> Result<Args, String> {
    let mut items: Vec<String> = argv.into_iter().collect();
    if let Some(first) = items.first() {
        if !first.starts_with('-') {
            items.remove(0); // program name
        }
    }
    let mut args = Args {
        url: DEFAULT_URL.to_string(),
        key: String::new(),
    };
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        let flag = item.strip_prefix('-').unwrap_or(item);
        let flag = flag.strip_prefix('-').unwrap_or(flag);
        let (name, inline_value) = match flag.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (flag.to_string(), None),
        };
        let value = match inline_value {
            Some(v) => v,
            None => {
                i += 1;
                match items.get(i) {
                    Some(v) => v.clone(),
                    None => return Err(format!("flag needs an argument: -{name}")),
                }
            }
        };
        match name.as_str() {
            "url" => args.url = value,
            "key" => args.key = value,
            other => return Err(format!("flag provided but not defined: -{other}")),
        }
        i += 1;
    }
    Ok(args)
}

/// Writes one NDJSON line to stdout, flushed (`fmt.Println — unbuffered).
fn emit_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn log(message: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{message}");
    let _ = err.flush();
}

#[tokio::main]
async fn main() {
    let args = match parse_args(std::env::args()) {
        Ok(args) => args,
        Err(e) => {
            log(&format!("Error: {e}"));
            std::process::exit(1);
        }
    };
    if args.url.is_empty() {
        log("Error: -url is required");
        std::process::exit(1);
    }

    let session: SessionSlot = Arc::new(RwLock::new(None));

    log(&format!("MCP Bridge starting...\nConnect: {}", args.url));

    // 1. Server → client stream (replaces the SSE listener goroutine).
    {
        let url = args.url.clone();
        let key = args.key.clone();
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            listen_server_stream(url, key, session).await;
        });
    }

    // 2. Claude → server loop over stdin (blocks until stdin closes, likewise).
    if let Err(e) = stdin_loop(&args, Arc::clone(&session)).await {
        log(&format!("Stdin error: {e}"));
    }
}

/// Reads NDJSON lines from stdin and POSTs each to the endpoint, forwarding
/// the response to stdout (`main loop). Line assembly is byte-buffered
/// with the original scanner's 10 MiB cap, so multi-byte UTF-8 survives chunk
/// boundaries and oversized lines abort the loop like `scanner.Err`.
async fn stdin_loop(args: &Args, session: SessionSlot) -> std::io::Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // No total timeout: tool responses arrive in the POST body and may be
        // slow (audit M-02 — the original bridge's 30s cap broke long tool calls).
        .build()
        .expect("reqwest client");
    let mut reader = tokio::io::stdin();
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 && buffer.is_empty() {
            return Ok(()); // stdin closed (Go: scanner.Scan() == false)
        }
        buffer.extend_from_slice(&chunk[..n]);
        // Dispatch every complete line; keep partial frames buffered.
        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=pos).collect();
            let message = line_without_eol(&line);
            if is_blank(message) {
                // Scenario: POSTed empty bodies (and got 400s); skipping is
                // lossless for real MCP traffic.
                continue;
            }
            let message = String::from_utf8_lossy(message).into_owned();
            forward_message(&client, args, &session, &message).await;
        }
        if buffer.len() > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stdin line too long",
            ));
        }
        if n == 0 && !buffer.is_empty() {
            // Final line without a trailing newline.
            let line = std::mem::take(&mut buffer);
            let message = line_without_eol(&line);
            if !is_blank(message) {
                let message = String::from_utf8_lossy(message).into_owned();
                forward_message(&client, args, &session, &message).await;
            }
            return Ok(());
        }
    }
}

/// Trimmed-empty check over raw bytes (whitespace per strings.TrimSpace).
fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| b.is_ascii_whitespace())
}

/// Strips the trailing `\n`/`\r\n` (`bufio.ScanLines semantics).
fn line_without_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

/// POSTs one JSON-RPC message and forwards the response body to stdout.
async fn forward_message(
    client: &reqwest::Client,
    args: &Args,
    session: &SessionSlot,
    message: &str,
) {
    let mut request = client
        .post(&args.url)
        .header("Content-Type", "application/json")
        // Streamable HTTP requires both MIME types in Accept.
        .header("Accept", "application/json, text/event-stream");
    if !args.key.is_empty() {
        request = request.header("X-Redmine-API-Key", &args.key);
    }
    if let Some(id) = session.read().ok().and_then(|g| g.clone()) {
        request = request.header("Mcp-Session-Id", id);
    }
    let response = match request.body(message.to_string()).send().await {
        Ok(response) => response,
        Err(e) => {
            log(&format!("Post failed: {e}"));
            return;
        }
    };
    let status = response.status();
    if status.as_u16() >= 400 {
        log(&format!(
            "Post error status: {} for URL {}",
            status.as_u16(),
            args.url
        ));
        if status.as_u16() == 404 {
            // Stale/expired session: drop it so the next initialize re-mints.
            if let Ok(mut guard) = session.write() {
                *guard = None;
            }
        }
    }
    if let Some(id) = response.headers().get("Mcp-Session-Id") {
        if let Ok(id) = id.to_str() {
            if let Ok(mut guard) = session.write() {
                *guard = Some(id.to_string());
            }
        }
    }
    if status == reqwest::StatusCode::ACCEPTED {
        return; // notification accepted; nothing to forward
    }
    let content_type = response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        pump_sse_stream(response, None).await;
    } else {
        // Single JSON-RPC message (or HTTP error body).
        match response.bytes().await {
            Ok(bytes) => {
                for line in String::from_utf8_lossy(&bytes).lines() {
                    if !line.trim().is_empty() {
                        emit_line(line);
                    }
                }
            }
            Err(e) => log(&format!("Read response failed: {e}")),
        }
    }
}

/// Holds the GET event stream open and forwards server→client messages to
/// stdout (replaces `listenSSE, same 1s reconnect cadence).
async fn listen_server_stream(url: String, key: String, session: SessionSlot) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            log(&format!("HTTP client error: {e}"));
            return;
        }
    };
    loop {
        let mut request = client
            .get(&url)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache");
        if !key.is_empty() {
            request = request.header("X-Redmine-API-Key", &key);
        }
        if let Some(id) = session.read().ok().and_then(|g| g.clone()) {
            request = request.header("Mcp-Session-Id", id);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                log(&format!("Stream disconnected: {e}. Reconnecting in 1s..."));
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            // Modern (2026-07-28) servers reject GET; POST responses carry
            // everything. Stop reconnecting instead of hammering the server.
            log("GET stream not supported by the server (405); relying on POST responses");
            return;
        }
        if status.as_u16() >= 400 {
            log(&format!(
                "Stream error status: {}. Reconnecting in 1s...",
                status.as_u16()
            ));
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        pump_sse_stream(response, Some(Duration::from_secs(1))).await;
    }
}

/// Parses an SSE response incrementally and forwards every `data: ` payload
/// line to stdout (`listenSSE line loop). When `reconnect` is present, a
/// natural stream end sleeps before returning (drives the reconnect loop).
async fn pump_sse_stream(response: reqwest::Response, reconnect: Option<Duration>) {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
            Some(Err(e)) => {
                log(&format!("Stream disconnected: {e}. Reconnecting in 1s..."));
                break;
            }
            None => break, // stream closed
        }
        // Forward complete lines; keep the tail buffered (partial frames with
        // multi-byte UTF-8 preserved — byte-counted, per.
        while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=pos).collect();
            let mut line = &line[..line.len() - 1]; // strip '\n'
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line);
            if let Some(payload) = line.strip_prefix("data: ") {
                emit_line(payload);
            }
            // `event:`/`id:`/`retry:` lines and comments are ignored, likewise.
        }
        if buffer.len() > MAX_LINE_BYTES {
            log("Stream line too long; dropping buffer");
            buffer.clear();
        }
    }
    if let Some(delay) = reconnect {
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults() {
        let args = parse_args(argv(&["mcp-bridge"])).unwrap();
        assert_eq!(args.url, DEFAULT_URL);
        assert_eq!(args.key, "");
    }

    #[test]
    fn single_dash_flags() {
        // Scenario: flag spelling: -url value / -key value.
        let args =
            parse_args(argv(&["mcp-bridge", "-url", "http://x:1/mcp", "-key", "k"])).unwrap();
        assert_eq!(args.url, "http://x:1/mcp");
        assert_eq!(args.key, "k");
    }

    #[test]
    fn inline_values_and_double_dash() {
        let args = parse_args(argv(&["mcp-bridge", "--url=http://y:2/mcp", "-key=z"])).unwrap();
        assert_eq!(args.url, "http://y:2/mcp");
        assert_eq!(args.key, "z");
    }

    #[test]
    fn errors() {
        assert!(parse_args(argv(&["mcp-bridge", "-url"])).is_err());
        assert!(parse_args(argv(&["mcp-bridge", "-bogus", "x"])).is_err());
        assert!(parse_args(argv(&["mcp-bridge", "positional"])).is_err());
    }

    #[test]
    fn stdin_line_cap_matches_reference() {
        assert_eq!(MAX_LINE_BYTES, 10 * 1024 * 1024);
    }

    /// SSE payload extraction mirrors only `data: ` lines are forwarded,
    /// CRLF tolerated.
    #[test]
    fn sse_lines_forward_data_only() {
        let chunked = b"event: message\ndata: {\"a\":1}\n\ndata: {\"b\":2}\r\nevent: endpoint\r\ndata: should-not-carry\r\n".to_vec();
        let mut payloads = Vec::new();
        for line in String::from_utf8_lossy(&chunked).lines() {
            if let Some(payload) = line.strip_prefix("data: ") {
                payloads.push(payload.to_string());
            }
        }
        assert_eq!(payloads, vec!["{\"a\":1}", "{\"b\":2}", "should-not-carry"]);
    }

    /// A local axum server drives the three POST response paths (JSON + 202 +
    /// SSE) and the session capture, mirroring the original bridge contract.
    #[tokio::test]
    async fn forward_message_response_paths() {
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|headers: axum::http::HeaderMap, body: String| async move {
                if body.contains("initialize") {
                    let mut resp = (
                        axum::http::StatusCode::OK,
                        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}",
                    )
                        .into_response();
                    resp.headers_mut().insert(
                        "Mcp-Session-Id",
                        axum::http::HeaderValue::from_static("sess-1"),
                    );
                    return resp;
                }
                if body.contains("notify") {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                }
                if body.contains("stream") {
                    assert_eq!(
                        headers.get("Mcp-Session-Id"),
                        Some(&axum::http::HeaderValue::from_static("sess-1")),
                        "captured session must be replayed"
                    );
                    let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":9}\n\n";
                    return (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        sse,
                    )
                        .into_response();
                }
                (axum::http::StatusCode::BAD_REQUEST, "nope").into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let args = Args {
            url: format!("http://{addr}/mcp"),
            key: "k".to_string(),
        };
        let session: SessionSlot = Arc::new(RwLock::new(None));
        let client = reqwest::Client::new();

        // JSON response path + session capture.
        forward_message(&client, &args, &session, "{\"initialize\":1}").await;
        assert_eq!(
            session.read().unwrap().as_deref(),
            Some("sess-1"),
            "session id must be captured"
        );

        // 202 path: accepted, nothing to forward.
        forward_message(&client, &args, &session, "{\"notify\":1}").await;

        // SSE response path with the session replayed (asserted server-side).
        forward_message(&client, &args, &session, "{\"stream\":1}").await;
    }

    /// A 404 clears a stale session (re-initialize on the next request).
    #[tokio::test]
    async fn not_found_clears_session() {
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let args = Args {
            url: format!("http://{addr}/mcp"),
            key: String::new(),
        };
        let session: SessionSlot = Arc::new(RwLock::new(Some("stale".to_string())));
        let client = reqwest::Client::new();
        forward_message(&client, &args, &session, "{\"x\":1}").await;
        assert_eq!(session.read().unwrap().as_deref(), None);
    }
}
