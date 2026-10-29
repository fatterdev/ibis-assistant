//! Discovery.
//!
//! `GET /` is content-negotiated:
//! `Accept: application/json` → JSON document, `text/markdown` → the guide,
//! anything else → an HTML page wrapping the guide.
//!
//! F-005/D11: the endpoints map no longer contains `sse_legacy` and the guide
//! no longer advertises `/sse`. The `tools[]` list is an injected provider
//! ([`ToolsProvider`]) — the 56-tool registry arrives with wave 4
//! (`threski-server`); the default provider returns an empty list.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use threski_core::config::Config;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

/// MCP resource URI for the guide.
pub const GUIDE_URI: &str = "threski://guide";
/// Discovery `name` field.
pub const SERVER_NAME: &str = "Threski Studio";
/// Discovery `version` field.
pub const SERVER_VERSION: &str = "1.1.0";

/// One row of the discovery `tools[]` list: sorted by name.
#[derive(Debug, Clone, Serialize)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    #[serde(rename = "input_schema")]
    pub input_schema: serde_json::Value,
}

/// Injected tool-list provider ( read `mcpServer.ListTools` directly;
/// the rmcp registry arrives with wave 4). Returns tools sorted by name.
pub type ToolsProvider = Arc<dyn Fn() -> Vec<ToolMeta> + Send + Sync>;

/// Empty default provider (wave-4 registry not wired yet).
pub fn no_tools() -> ToolsProvider {
    Arc::new(Vec::new)
}

/// MCP protocol versions known to this SDK, newest first (
/// `protocolVersions` copied `mcp.ValidProtocolVersions`).
pub fn protocol_versions() -> Vec<String> {
    use rmcp::model::ProtocolVersion;
    let mut versions: Vec<String> = ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .map(|v| v.as_str().to_string())
        .collect();
    versions.reverse();
    versions
}

/// Shared handler state.
#[derive(Clone)]
pub struct DiscoveryState {
    pub cfg: Arc<Config>,
    pub tools: ToolsProvider,
}

impl DiscoveryState {
    pub fn new(cfg: Arc<Config>, tools: ToolsProvider) -> Self {
        DiscoveryState { cfg, tools }
    }
}

/// The human install/usage guide, also served as the MCP resource
/// `threski://guide` (`GuideMarkdown, updated for D11 and the Rust build).
pub fn guide_markdown(cfg: &Config, tools: &[ToolMeta]) -> String {
    let base = cfg.public_base_url();
    let mut mode = cfg.runtime_mode.trim().to_string();
    if mode.is_empty() {
        mode = "(unset — listener binds all interfaces)".to_string();
    }
    let mut b = String::new();
    b.push_str("# Threski Studio\n\n");
    b.push_str(
        "Living project memory over MCP. One binary: personal, server, or Cursor plugin (stdio child).\n\n",
    );
    b.push_str(&format!("- **Runtime:** `{mode}`\n"));
    b.push_str(&format!("- **Listen:** `{}`\n", cfg.listen_addr()));
    b.push_str(&format!("- **Streamable HTTP (default):** `{base}/mcp`\n"));
    b.push_str(&format!(
        "- **Human/agent guide:** `GET {base}/` (`Accept: text/html` | `application/json` | `text/markdown`) and MCP resource `{GUIDE_URI}`\n\n",
    ));

    b.push_str("## Install\n\n");
    b.push_str("1. Build the binary: `cargo build --release -p threski-server` (Windows: `threski.exe`), or unpack a release distribution (exe + `mcp-bridge` + `rules/` + `sgconfig.yml` + `config.json`).\n");
    b.push_str("2. First run with no `config.json` launches **`config fast`** (hardware probe + hybrid AI + keys). Or run `threski config` / `config full`. Prefer the wizard/Settings UI over hand-editing; never commit `config.json`.\n");
    b.push_str("3. Set `runtime_mode` to `personal` (same PC as your repos) or `server` (dedicated host).\n");
    b.push_str("4. For personal mode, set `projects[].working_repo_path` (or `git_repos`) to the live checkout. Do not point `discovery_root/dynamic` at a developer tree.\n");
    b.push_str("5. Run `threski run`, or `start`/`stop` for a daemon, or `/install` as a Windows service. Cursor plugin: `threski -mode stdio` with `RUNTIME_MODE=plugin`.\n\n");
    b.push_str("Personal mode binds `127.0.0.1` unless `bind_address` is set. Server mode binds `0.0.0.0`.\n\n");

    b.push_str("## AI and Settings\n\n");
    b.push_str("Default reasoning is **hybrid**: local Ollama chat (hardware auto-tier) plus a multi-cloud pool (Gemini, OpenAI-compatible, Claude). Embeddings stay on Ollama `nomic-embed-text`.\n\n");
    b.push_str(&format!(
        "- **Settings UI:** `{base}/settings/` (same app as Cursor command **Threski: Open Settings** / MCP `settings_open_ui`)\n",
    ));
    b.push_str("- **API:** `GET|POST /api/v1/settings` (localhost)\n");
    b.push_str("- **MCP:** `settings_get`, `settings_apply`, `settings_open_ui`\n");
    b.push_str("- **Keys:** enter in wizard or Settings UI into `config.json` (ENV is optional override only)\n");
    b.push_str("- **CLI:** `threski config show` / `config recommend`\n\n");
    b.push_str("Operator guide in the repo: `docs/ai_and_settings.md`.\n\n");

    b.push_str("## Agent autoconfig\n\n");
    b.push_str("HTTP clients: `GET /` with `Accept: application/json`. MCP 2026-07-28 clients: `server/discover` on `/mcp` (legacy clients still `initialize`). Then `resources/read` on `threski://guide` to show this page to a human.\n\n");
    b.push_str("Cursor / VS Code MCP (Streamable HTTP):\n\n");
    b.push_str("```json\n");
    b.push_str(&format!(
        "{{\n  \"mcpServers\": {{\n    \"threski\": {{\n      \"url\": \"{base}/mcp\"\n    }}\n  }}\n}}\n",
    ));
    b.push_str("```\n");
    b.push_str(&format!(
        "Claude Desktop: run the bundled `mcp-bridge` binary (stdio → Streamable HTTP `{base}/mcp`).\n\n",
    ));
    b.push_str(&format!(
        "Inspector: `npx @modelcontextprotocol/inspector {base}/mcp`\n\n",
    ));

    b.push_str("## CLI\n\n");
    b.push_str(
        "Setup: `threski config` | `config fast` | `config full` | `config show`.\n\n",
    );
    b.push_str("The same binary is an MCP client of a running server (`POST /mcp`):\n\n");
    b.push_str("```bash\n");
    b.push_str("threski ask \"Explain the auth flow\" --branch main\n");
    b.push_str("threski ingest code --path ./path/to/project\n");
    b.push_str("threski logs analyze --project MyApp --text \"ERROR timeout\"\n");
    b.push_str("```\n\n");

    b.push_str("## Main tools\n\n");
    b.push_str("- `init_project` — sync git workspace and **wait** for ingest (progress notifications if the client sends `progressToken`).\n");
    b.push_str("- `ask_project` — hybrid search over the knowledge graph.\n");
    b.push_str("- `analyze_logs` — synchronous log analysis.\n");
    b.push_str("- `ingest_code` / `ingest` CLI — vectorize a tree.\n");
    b.push_str("- `sync_local_patch` — server clones only, when `init_project` returns `requires_patch`.\n");
    b.push_str(
        "- `settings_get` / `settings_apply` / `settings_open_ui` — hybrid AI and cloud keys.\n\n",
    );

    b.push_str("### Auth headers (optional per request)\n\n");
    b.push_str("`X-Git-Token` / `X-Git-PAT`, `X-Redmine-API-Key`, `X-Jira-Email` + `X-Jira-API-Token`, `X-Azure-DevOps-PAT`.\n\n");

    if cfg.log_ingestion.http.enabled {
        b.push_str("Log HTTP upload is enabled at `POST /api/v1/logs/upload` (`X-API-Key`).\n\n");
    }

    if !tools.is_empty() {
        b.push_str("## Registered MCP tools\n\n");
        for t in tools {
            b.push_str(&format!("- `{}` — {}\n", t.name, t.description));
        }
    }
    b
}

/// The discovery handler: negotiates on `Accept` and
/// sorts the provider's tool list by name.
pub(crate) async fn discovery_handler(
    State(state): State<DiscoveryState>,
    headers: HeaderMap,
) -> Response {
    let mut tools = (state.tools)();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if accept.contains("application/json") {
        json_discovery_response(&state.cfg, &tools)
    } else if accept.contains("text/markdown") {
        (
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            guide_markdown(&state.cfg, &tools),
        )
            .into_response()
    } else {
        html_discovery(&state.cfg, &tools)
    }
}

/// The JSON discovery document (`writeJSONDiscovery, D11: no
/// `sse_legacy` endpoint entry).
fn json_discovery(cfg: &Config, tools: &[ToolMeta]) -> serde_json::Value {
    let base = cfg.public_base_url();
    json!({
        "status": "online",
        "name": SERVER_NAME,
        "version": SERVER_VERSION,
        "runtime_mode": cfg.runtime_mode,
        "listen_addr": cfg.listen_addr(),
        "protocol_versions": protocol_versions(),
        "endpoints": {
            "streamable_http": format!("{base}/mcp"),
            "log_push_upload": format!("{base}/api/v1/logs/upload"),
            "settings_ui": format!("{base}/settings/"),
            "settings_api": format!("{base}/api/v1/settings"),
            "guide_markdown": base.clone(),
            "guide_resource": GUIDE_URI,
            "server_discover": "server/discover",
        },
        "auth_headers": [
            "X-Git-Token", "X-Git-PAT",
            "X-Redmine-API-Key",
            "X-Jira-Email", "X-Jira-API-Token",
            "X-Azure-DevOps-PAT", "X-Azure-DevOps-Org", "X-Azure-DevOps-Project", "X-Azure-DevOps-Repo",
        ],
        "documentation": {
            "guide_resource": GUIDE_URI,
            "cursor_mcp": {
                "mcpServers": {
                    "threski": {
                        "url": format!("{base}/mcp"),
                    },
                },
            },
        },
        "guide_markdown": guide_markdown(cfg, tools),
        "tools": tools,
    })
}

fn json_discovery_response(cfg: &Config, tools: &[ToolMeta]) -> Response {
    let body = json_discovery(cfg, tools).to_string();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// The HTML discovery page: the guide in a `<pre>`
/// plus one tool-schema card per registered tool.
fn html_discovery(cfg: &Config, tools: &[ToolMeta]) -> Response {
    let md = guide_markdown(cfg, tools);
    let mut tools_html = String::new();
    for t in tools {
        let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
        tools_html.push_str(&format!(
            "<div class=\"tool-item\"><div class=\"tool-name\">{}</div><div class=\"tool-desc\">{}</div><pre><code>{}</code></pre></div>",
            html_escape(&t.name),
            html_escape(&t.description),
            html_escape(&schema),
        ));
    }
    let markdown = markdown_as_pre(&md);
    let page = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\"/>\n<title>Threski Studio</title>\n<style>\nbody{{font-family:system-ui,sans-serif;background:#0f1419;color:#e7ecf3;margin:0;padding:2rem;}}\nmain{{max-width:52rem;margin:0 auto;}}\npre{{background:#1b222c;padding:1rem;overflow:auto;border-radius:8px;}}\ncode{{font-family:ui-monospace,monospace;}}\n.tool-item{{margin:1rem 0;padding:1rem;background:#1b222c;border-radius:8px;}}\n.tool-name{{font-weight:700;color:#7dd3fc;}}\na{{color:#7dd3fc;}}\n</style>\n</head>\n<body><main>\n{markdown}\n<h2>Tool schemas</h2>\n{tools}\n</main></body></html>",
        markdown = markdown,
        tools = tools_html,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response()
}

fn markdown_as_pre(md: &str) -> String {
    format!("<pre>{}</pre>", html_escape(md))
}

/// `html.EscapeString subset: `& < > ' "`.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            other => out.push(other),
        }
    }
    out
}

// JSON/HTML bodies are only compared through the axum handler in tests; the
// two render helpers above are exercised via `discovery_handler`.
#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Arc<Config> {
        Arc::new(Config {
            port: 3030,
            runtime_mode: "personal".into(),
            ..Default::default()
        })
    }

    async fn respond(accept: &str) -> Response {
        let state = DiscoveryState::new(cfg(), no_tools());
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, accept.parse().unwrap());
        discovery_handler(State(state), headers).await
    }

    /// Scenario: TestDiscoveryJSONAndMarkdown (JSON half, updated for D11:
    /// `sse_legacy` must be absent).
    #[tokio::test]
    async fn discovery_json_shape() {
        let resp = respond("application/json").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["status"], "online");
        assert_eq!(body["name"], SERVER_NAME);
        assert_eq!(body["version"], SERVER_VERSION);
        assert_eq!(body["listen_addr"], "127.0.0.1:3030");
        let eps = &body["endpoints"];
        assert_eq!(eps["streamable_http"], "http://127.0.0.1:3030/mcp");
        assert_eq!(eps["settings_ui"], "http://127.0.0.1:3030/settings/");
        assert_eq!(eps["settings_api"], "http://127.0.0.1:3030/api/v1/settings");
        assert_eq!(
            eps["log_push_upload"],
            "http://127.0.0.1:3030/api/v1/logs/upload"
        );
        assert_eq!(eps["guide_resource"], GUIDE_URI);
        assert_eq!(eps["server_discover"], "server/discover");
        assert!(
            eps.get("sse_legacy").is_none(),
            "D11: sse_legacy endpoint must be gone: {eps}"
        );
        let guide = body["guide_markdown"].as_str().unwrap();
        assert!(guide.contains("/mcp"), "guide missing /mcp");
        assert!(
            guide.contains("AI and Settings") && guide.contains("config fast"),
            "guide missing AI/Settings section"
        );
        assert!(
            !guide.contains("/sse"),
            "D11: guide must not advertise SSE: {guide}"
        );
        let vers = body["protocol_versions"].as_array().unwrap();
        assert!(
            vers.iter().any(|v| v == "2026-07-28"),
            "protocol_versions missing latest: {vers:?}"
        );
        let auth = body["auth_headers"].as_array().unwrap();
        assert!(!auth.iter().any(|v| v == "X-Threski-Actor"));
        assert!(auth.iter().any(|v| v == "X-Git-Token"));
        assert!(auth.iter().any(|v| v == "X-Azure-DevOps-Repo"));
        assert_eq!(
            body["documentation"]["cursor_mcp"]["mcpServers"]["threski"]["url"],
            "http://127.0.0.1:3030/mcp"
        );
    }

    /// Scenario: TestDiscoveryJSONAndMarkdown (markdown half).
    #[tokio::test]
    async fn discovery_markdown() {
        let resp = respond("text/markdown").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.starts_with("text/markdown"));
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("Threski Studio"));
        assert!(body.contains("## AI and Settings"));
        assert!(body.contains("## Install"));
    }

    #[tokio::test]
    async fn discovery_html_default() {
        let resp = respond("text/html").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.starts_with("text/html"));
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("<!DOCTYPE html>"));
        assert!(body.contains("<pre>"));
    }

    #[tokio::test]
    async fn tools_listed_sorted() {
        let provider: ToolsProvider = Arc::new(|| {
            vec![
                ToolMeta {
                    name: "zebra".into(),
                    description: "last".into(),
                    input_schema: json!({"type": "object"}),
                },
                ToolMeta {
                    name: "alpha".into(),
                    description: "first".into(),
                    input_schema: json!({"type": "object", "properties": {}}),
                },
            ]
        });
        let state = DiscoveryState::new(cfg(), provider);
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse().unwrap());
        let resp = discovery_handler(State(state), headers).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "alpha");
        assert_eq!(tools[1]["name"], "zebra");
        assert_eq!(tools[0]["input_schema"]["properties"], json!({}));
    }

    #[tokio::test]
    async fn empty_tools_serialize_as_array() {
        // Scenario: marshalled a nil slice as `null`; this crate always emits `[]`.
        let resp = respond("application/json").await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn guide_lists_log_upload_when_enabled() {
        let mut config = (*cfg()).clone();
        config.log_ingestion.http.enabled = true;
        let state = DiscoveryState::new(Arc::new(config), no_tools());
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/markdown".parse().unwrap());
        let resp = discovery_handler(State(state), headers).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("POST /api/v1/logs/upload"));
    }
}
