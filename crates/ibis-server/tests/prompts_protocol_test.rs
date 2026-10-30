//! F0.3 protocol gate: the real [`IbisServer`] over the rmcp in-memory
//! duplex (the same NDJSON framing stdio uses) serves the `/ibis.*`
//! prompt surface — capability advertisement (`prompts` + `completions`),
//! `prompts/list` (the 5-prompt table with argument definitions),
//! `prompts/get` (single user message, argument interpolation, unknown-name
//! error) and `completion/complete` (static kind/visibility values,
//! config-backed project names, unknown refs degrade to empty).

use std::sync::{Arc, Mutex, RwLock};

use rmcp::model::{
    ClientCapabilities, ClientConfig, CompleteRequestParams, GetPromptRequestParams,
    Implementation, Reference,
};
use rmcp::service::{Peer, RoleClient, RunningService, ServiceExt};
use serde_json::{json, Value};
use ibis_ai::Client;
use ibis_core::config::{Config, ProjectConfig};
use ibis_server::state::AppState;
use ibis_server::{assembly, mcp_server, prompts};

/// A minimal-but-real [`AppState`]: no DB (degraded clone-only), no AI
/// providers, no background workers — enough state for the prompt surface
/// (the only stateful bit is `cfg.projects` for `project_name` completions).
async fn test_state(projects: &[String]) -> Arc<AppState> {
    let mut cfg = Config {
        port: 3030,
        runtime_mode: "server".into(),
        ..Default::default()
    };
    for name in projects {
        cfg.projects.push(ProjectConfig {
            name: (*name).to_string(),
            working_repo_path: format!("C:/repos/{name}"),
            ..Default::default()
        });
    }
    let cfg = Arc::new(cfg);
    let db: Option<Arc<dyn ibis_db::executor::Executor>> = None;
    let ticket = assembly::assemble_ticketing(db.clone()).await.0;
    let repopr = assembly::assemble_repopr(&ticket.config().clone());
    Arc::new(AppState {
        cfg: Arc::new(RwLock::new((*cfg).clone())),
        db: Arc::new(RwLock::new(db)),
        search: Arc::new(RwLock::new(Arc::new(
            ibis_search::search::SearchService::new(None, None),
        ))),
        ai: Arc::new(Client::new(None, None, None)),
        ticket,
        repopr,
        sessions: Mutex::new(std::collections::HashMap::new()),
        ingestion: Arc::new(ibis_ingest_dynamic::queue::ProjectIngestionManager::new(
            cfg.clone(),
            None,
        )),
        active_repos: vec![],
        db_process: Mutex::new(None),
        resource_subs: ibis_server::resources::ResourceSubscriptions::default(),
        sampling: Arc::new(ibis_server::sampling::SamplingSessionRegistry::new()),
        tool_discovery: ibis_server::tool_discovery::ToolDiscovery::new(false),
    })
}

/// The real server over an in-memory duplex, plus the connected client.
/// The service stays alive for the whole test (the peer borrows from it);
/// [`TestClient::shutdown`] tears both sides down.
struct TestClient {
    service: RunningService<RoleClient, ClientConfig>,
    server: tokio::task::JoinHandle<Result<(), String>>,
}

impl TestClient {
    async fn connect(projects: &[&str]) -> TestClient {
        let projects: Vec<String> = projects.iter().map(|s| s.to_string()).collect();
        let state = test_state(&projects).await;
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let service = mcp_server::IbisServer::new(state, false);
            match service.serve(server_stream).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        });
        let service = match ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("prompts-protocol-test", "0.0.0"),
        )
        .serve(client_stream)
        .await
        {
            Ok(service) => service,
            Err(e) => {
                let server_result = server.await;
                panic!("client init failed ({e}); server task: {server_result:?}")
            }
        };
        TestClient { service, server }
    }

    fn peer(&self) -> &Peer<RoleClient> {
        self.service.peer()
    }

    async fn shutdown(self) {
        let _ = self.service.cancel().await;
        match self.server.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("server serve error: {e}"),
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        }
    }
}

#[tokio::test]
async fn prompts_capability_advertised_and_list_serves_the_table() {
    let client = TestClient::connect(&[]).await;
    let peer = client.peer();

    // Capability advertisement: prompts + completions on the initialize
    // result the client negotiated.
    let info = peer
        .peer_info()
        .expect("initialize result cached by the client");
    assert!(
        info.capabilities.prompts.is_some(),
        "prompts capability: {info:?}"
    );
    assert!(
        info.capabilities.completions.is_some(),
        "completions capability: {info:?}"
    );

    let listed = peer
        .list_prompts(Default::default())
        .await
        .expect("prompts/list");
    let mut names: Vec<&str> = listed.prompts.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "blast_radius",
            "readiness",
            "remember",
            "status",
            "why_changed"
        ],
        "the F0.3 prompt table"
    );
    for prompt in &listed.prompts {
        assert!(prompt.title.is_some(), "{} title", prompt.name);
        assert!(
            prompt.description.as_deref().unwrap_or_default().len() > 20,
            "{} description",
            prompt.name
        );
        let args = prompt.arguments.as_ref().expect("arguments declared");
        assert_eq!(
            args,
            &prompts::PROMPT_SPECS
                .iter()
                .find(|s| s.name == prompt.name)
                .expect("listed prompt is a spec")
                .arguments
                .iter()
                .map(|a| {
                    rmcp::model::PromptArgument::new(a.name)
                        .with_title(a.title)
                        .with_description(a.description)
                        .with_required(a.required)
                })
                .collect::<Vec<_>>(),
            "{} argument definitions match the spec table",
            prompt.name
        );
    }
    client.shutdown().await;
}

#[tokio::test]
async fn prompts_get_renders_the_session_contract_with_arguments() {
    let client = TestClient::connect(&[]).await;
    let peer = client.peer();

    // readiness interpolates the project and pins the checklist + table.
    let readiness = peer
        .get_prompt(
            GetPromptRequestParams::new("readiness").with_arguments(
                json!({ "project_name": "demo-project" })
                    .as_object()
                    .expect("readiness arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("prompts/get readiness");
    assert_eq!(readiness.messages.len(), 1, "single user message");
    let message = &readiness.messages[0];
    assert_eq!(message.role, rmcp::model::Role::User);
    let text = match &message.content {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    for fragment in [
        "readiness checklist for demo-project",
        "`load_session_context` once",
        "settings_get",
        "| MCP live |",
        "| Workspace | bound / missing |",
        "Next step: one concrete action",
    ] {
        assert!(
            text.contains(fragment),
            "readiness text missing {fragment:?}:\n{text}"
        );
    }
    assert!(
        readiness.description.is_some(),
        "get_prompt echoes the description"
    );

    // remember: kind guide + hygiene + defaults.
    let remember = peer
        .get_prompt(GetPromptRequestParams::new("remember"))
        .await
        .expect("prompts/get remember");
    let remember_text = match &remember.messages[0].content {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    for fragment in [
        "`memory_add`",
        "| `decision` |",
        "| `gotcha` |",
        "narrowest fit (fact when nothing else fits)",
        "visibility team",
        "supersede it with `memory_update`",
    ] {
        assert!(
            remember_text.contains(fragment),
            "remember text missing {fragment:?}:\n{remember_text}"
        );
    }

    // why_changed without arguments asks the user for a subject.
    let why = peer
        .get_prompt(GetPromptRequestParams::new("why_changed"))
        .await
        .expect("prompts/get why_changed");
    let why_text = match &why.messages[0].content {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        why_text.contains("ask the user which of: a file path, a symbol name, or a ticket id"),
        "subjectless why_changed asks for the subject:\n{why_text}"
    );

    // why_changed with a symbol interpolates it.
    let why_symbol = peer
        .get_prompt(
            GetPromptRequestParams::new("why_changed").with_arguments(
                json!({ "symbol_name": "parse_chunk" })
                    .as_object()
                    .expect("why_changed arguments object")
                    .clone(),
            ),
        )
        .await
        .expect("prompts/get why_changed");
    let why_symbol_text = match &why_symbol.messages[0].content {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        why_symbol_text.contains("the symbol `parse_chunk`")
            && why_symbol_text.contains("symbol_name: `parse_chunk`"),
        "why_changed interpolates the symbol:\n{why_symbol_text}"
    );

    // Unknown prompt → protocol error, not a prompt.
    let unknown = peer
        .get_prompt(GetPromptRequestParams::new("nonexistent"))
        .await;
    assert!(unknown.is_err(), "unknown prompt must error: {unknown:?}");
    client.shutdown().await;
}

#[tokio::test]
async fn prompts_complete_serves_static_and_config_backed_values() {
    let client = TestClient::connect(&["demo-project", "other-repo"]).await;
    let peer = client.peer();

    // remember.kind → the 7 durable kinds (static completions).
    let kinds = peer
        .complete_prompt_argument("remember", "kind", "", None)
        .await
        .expect("complete remember.kind");
    assert_eq!(
        kinds.values,
        prompts::MEMORY_KINDS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "kind completions are the durable kind set"
    );
    assert_eq!(kinds.total, Some(kinds.values.len() as u32));
    assert_eq!(kinds.has_more, Some(false));

    // Prefix filtering: "go" → gotcha only.
    let filtered = peer
        .complete_prompt_argument("remember", "kind", "go", None)
        .await
        .expect("complete remember.kind prefix");
    assert_eq!(filtered.values, vec!["gotcha".to_string()]);

    // remember.visibility → team|personal.
    let visibility = peer
        .complete_prompt_argument("remember", "visibility", "", None)
        .await
        .expect("complete remember.visibility");
    assert_eq!(
        visibility.values,
        vec!["team".to_string(), "personal".to_string()]
    );

    // status.project_name → the configured projects (config-backed).
    let projects = peer
        .complete_prompt_argument("status", "project_name", "", None)
        .await
        .expect("complete status.project_name");
    assert_eq!(
        projects.values,
        vec!["demo-project".to_string(), "other-repo".to_string()]
    );

    // Free-form arguments complete to empty.
    let free_form = peer
        .complete_prompt_argument("why_changed", "file_path", "src/ma", None)
        .await
        .expect("complete why_changed.file_path");
    assert!(
        free_form.values.is_empty(),
        "free-form argument: {free_form:?}"
    );

    // Unknown prompt/argument → empty completion, no error.
    let unknown_prompt = peer
        .complete_prompt_argument("nonexistent", "kind", "", None)
        .await
        .expect("unknown prompt completes empty");
    assert!(unknown_prompt.values.is_empty());
    let unknown_arg = peer
        .complete_prompt_argument("remember", "nope", "", None)
        .await
        .expect("unknown argument completes empty");
    assert!(unknown_arg.values.is_empty());

    // Resource references (no templates yet — F0.4) complete empty.
    let resource_ref = peer
        .complete(CompleteRequestParams::new(
            Reference::for_resource("ibis://memory/{kind}/{id}"),
            rmcp::model::ArgumentInfo::new("kind", "de"),
        ))
        .await
        .expect("resource ref completes empty");
    assert!(resource_ref.completion.values.is_empty());
    client.shutdown().await;
}

/// The wire shapes of the prompt surface: `prompts/list` rows serialize
/// camelCase with title + arguments, and `prompts/get` results are one
/// user-role text message (spot check via the spec types).
#[test]
fn prompts_wire_shapes() {
    let definitions = prompts::prompt_definitions();
    assert_eq!(definitions.len(), 5, "the F0.3 table has 5 prompts");
    let wire: Vec<Value> = definitions
        .iter()
        .map(|p| serde_json::to_value(p).expect("prompt serializes"))
        .collect();
    for row in &wire {
        assert!(row["name"].is_string(), "name: {row}");
        assert!(row["title"].is_string(), "title: {row}");
        assert!(row["description"].is_string(), "description: {row}");
        assert!(row["arguments"].is_array(), "arguments: {row}");
        for arg in row["arguments"].as_array().unwrap() {
            assert!(arg["name"].is_string(), "arg name: {arg}");
            assert!(arg["title"].is_string(), "arg title: {arg}");
            assert!(arg["description"].is_string(), "arg description: {arg}");
            assert!(arg["required"].is_boolean(), "arg required: {arg}");
        }
    }
    let remember = wire
        .iter()
        .find(|r| r["name"] == "remember")
        .expect("remember listed");
    let kinds = remember["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "kind")
        .expect("kind argument");
    assert_eq!(kinds["required"], json!(false));

    let message = rmcp::model::PromptMessage::new_text(rmcp::model::Role::User, "hello");
    let wire_message = serde_json::to_value(&message).expect("message serializes");
    assert_eq!(wire_message["role"], json!("user"));
    assert_eq!(wire_message["content"]["type"], json!("text"));
}
