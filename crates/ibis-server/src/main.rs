//! `ibis-assistant` — the server binary.
//!
//! Command surface ( main:79-168):
//! - no args → help
//! - `run` / `-run` / `--run` / any flag-argument → the server
//! - `config [fast|full|show|--recommend|ai]` → the wizard
//! - `version` / `-version` / `--version`
//! - `/run`, `install`, `uninstall`, `start`, `stop` → the daemon/service
//!   dispatch (D10) — explicit seam, wired by lane N-service
//! - anything else → the CLI client passthrough (`ibis_cli::execute_cli`)
//!
//! The binary consumes the `ibis_server` library target (module root in
//! `src/lib.rs`), mirroring the `package main` layout while keeping the
//! contract-gate tests able to drive the registry.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use clap::Parser;
use ibis_core::config::Config;
use ibis_db::executor::Executor;
use ibis_search::search::SearchService;
use ibis_server::{assembly, collision, mcp_server, registry, state};
use rmcp::service::ServiceExt as _;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

use state::AppState;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Scenario: shouldRunWizard : interactive, non-plugin launch,
    // no config file, and not a service/wizard command.
    if should_run_wizard(&argv) {
        if let Err(err) = ibis_wizard::run() {
            eprintln!("Config wizard failed: {err}");
            std::process::exit(1);
        }
        // Scenario: rewrote os.Args to `run` after a StartServer outcome.
        run_server(&[]);
    }

    if argv.is_empty() {
        print_help();
        return;
    }

    let cmd = argv[0].as_str();
    match cmd {
        "/run" => exit_with_service_dispatch(&argv),
        "run" | "-run" | "--run" => run_server(&argv[1..]),
        "/install" | "install" | "-install" | "--install" => exit_with_service_dispatch(&argv),
        "/uninstall" | "uninstall" | "-uninstall" | "--uninstall" => {
            exit_with_service_dispatch(&argv)
        }
        "config" => {
            let sub = argv.get(1).map(String::as_str).unwrap_or("fast");
            match sub {
                "full" => exit_with_wizard(ibis_wizard::run_mode(ibis_wizard::Mode::Full)),
                "--show" | "show" => {
                    ibis_wizard::show_ai_summary(&Config::load());
                }
                "--recommend" | "recommend" => {
                    if let Err(err) = ibis_wizard::print_recommendations_json(&Config::load()) {
                        eprintln!("{err}");
                        std::process::exit(1);
                    }
                }
                // Scenario: treated every other subcommand as fast (the unknown-name
                // branch body was empty — audit finding N-A07 dead code).
                _ => exit_with_wizard(ibis_wizard::run_mode(ibis_wizard::Mode::Fast)),
            }
        }
        "start" | "stop" => exit_with_service_dispatch(&argv),
        "version" | "-version" | "--version" => {
            if !mcp_server::COMMIT.is_empty() && mcp_server::COMMIT != "none" {
                println!(
                    "ibis-assistant {} ({})",
                    mcp_server::VERSION,
                    mcp_server::COMMIT
                );
            } else {
                println!("ibis-assistant {}", mcp_server::VERSION);
            }
        }
        other => {
            if other.starts_with('-') {
                run_server(&argv);
            } else {
                // : delegate to the CLI client.
                let result = ibis_cli::execute_cli(argv, |payload| {
                    let runtime = tokio::runtime::Runtime::new().expect("cli runtime");
                    if let Err(error) = runtime.block_on(ibis_cli::call_server_tool(payload)) {
                        eprintln!("{error}");
                        std::process::exit(1);
                    }
                });
                if let Err(error) = result {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// `handleService — the daemon/service dispatch seam.
///
/// **Wired by lane N-service (daemon/service) — orchestrator replaces.**
/// Lane N-service owns `install`/`uninstall` (windows-service SCM lifecycle,
/// F-011 one-pass elevation), `/run` (SCM mode) and `start`/`stop`
/// (PID-file background daemon). The raw arguments are passed through
/// verbatim (`["install"]`, `["/run"]`, `["stop"]`, …); `Some(code)` means
/// "handled, exit with this code".
fn dispatch_service_command(args: &[String]) -> Option<i32> {
    ibis_server::service::dispatch_service_command(args, run_server)
}

fn exit_with_service_dispatch(args: &[String]) -> ! {
    if let Some(code) = dispatch_service_command(args) {
        std::process::exit(code);
    }
    let cmd = args.first().map(String::as_str).unwrap_or("");
    eprintln!(
        "Error: the daemon/service command {cmd:?} is not wired yet (lane N-service owns the SCM/PID-file lifecycle)"
    );
    std::process::exit(1);
}

fn exit_with_wizard(result: Result<ibis_wizard::Outcome, ibis_wizard::WizardError>) -> ! {
    match result {
        Ok(ibis_wizard::Outcome::StartServer) => run_server(&[]),
        Ok(ibis_wizard::Outcome::Done) => std::process::exit(0),
        Err(err) => {
            eprintln!("Config wizard failed: {err}");
            std::process::exit(1);
        }
    }
}

/// Scenario: printHelp
fn print_help() {
    let bin_name = std::env::args()
        .next()
        .map(|p| {
            std::path::Path::new(&p)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(p)
        })
        .unwrap_or_else(|| "ibis-assistant".to_string());
    if !mcp_server::COMMIT.is_empty() && mcp_server::COMMIT != "none" {
        println!(
            "Ibis Assistant {} ({})",
            mcp_server::VERSION,
            mcp_server::COMMIT
        );
    } else {
        println!("Ibis Assistant {}", mcp_server::VERSION);
    }
    println!("MCP Knowledge Graph & Search Engine");
    println!("\nUsage:");
    println!("  {bin_name} <command> [options]");
    println!("\nServer commands:");
    println!("  run         Start the server interactively (default)");
    println!("  start       Start the server in the background");
    println!("  stop        Stop the background server");
    println!("  config       Interactive setup (default: fast)");
    println!("  config fast  Fast setup with hardware probe");
    println!("  config full  Full setup (DB, SMTP, ticketing, …)");
    println!("  config show  Print AI/settings summary (keys masked)");
    println!("  config recommend  Print JSON recommendations");
    println!("  version     Print the binary version");
    println!("  install     Install as a Windows service ('ibis-assistant')");
    println!("  uninstall   Uninstall the Windows service");
    println!("\nCLI client (MCP Streamable HTTP POST /mcp):");
    println!("  ask          Ask a reasoning question about indexed code");
    println!("  ingest       Sync and index code or Git");
    println!("  ticket       Tickets on Redmine / Jira / Azure DevOps");
    println!("  pr           Pull requests");
    println!("  logs         Analyze log text");
    println!("  credentials  Persist Git credentials");
    println!("  memory       Collaborative project memories");
    println!("  outcome      Save reasoning outcomes");
    println!("  optimize     RAFT knowledge-base optimization");
    println!("\nExample:");
    println!("  {bin_name} run -port 3030");
}

/// `configExists
fn config_exists() -> bool {
    if std::path::Path::new("config.json").exists() {
        return true;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("config.json").exists();
        }
    }
    false
}

/// `peekFlag — raw pre-flag scan.
fn peek_flag(args: &[String], names: &[&str]) -> String {
    for (i, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name && i + 1 < args.len() && !args[i + 1].starts_with('-') {
                return args[i + 1].clone();
            }
            if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// `isPluginLaunch
fn is_plugin_launch(args: &[String]) -> bool {
    if ibis_core::config::is_plugin_runtime(&std::env::var("RUNTIME_MODE").unwrap_or_default()) {
        return true;
    }
    ibis_core::config::is_plugin_runtime(&peek_flag(args, &["-runtime-mode", "--runtime-mode"]))
}

/// `shouldRunWizard
fn should_run_wizard(args: &[String]) -> bool {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        return false;
    }
    if is_plugin_launch(args) {
        return false;
    }
    if config_exists() {
        return false;
    }
    let arg1 = args.first().map(String::as_str).unwrap_or("");
    !matches!(
        arg1,
        "help"
            | "-h"
            | "--help"
            | "install"
            | "uninstall"
            | "/install"
            | "/uninstall"
            | "/run"
            | "start"
            | "stop"
            | "config"
    )
}

/// The clap flag surface . Defaults come from the loaded
/// config at runtime, so each field is `Option` and applied over the config
/// below (same observable semantics; the help text lists no dynamic default).
#[derive(Parser, Debug)]
struct ServerFlags {
    /// Port to listen on for SSE
    #[arg(long = "port")]
    port: Option<i64>,
    /// Mode: 'sse' or 'stdio'
    #[arg(long = "mode")]
    mode: Option<String>,
    /// Discover git repositories in current/root directory
    #[arg(long = "scan")]
    scan: Option<bool>,
    /// Root directory for discovery
    #[arg(long = "root")]
    root: Option<String>,
    /// Log file path
    #[arg(long = "log-file")]
    log_file: Option<String>,
    /// Log level: DEBUG, INFO, WARN, ERROR
    #[arg(long = "log-level")]
    log_level: Option<String>,
    /// Runtime: personal, server, or plugin
    #[arg(long = "runtime-mode")]
    runtime_mode: Option<String>,
    /// Opened folder / live working tree (personal/plugin)
    #[arg(long = "workspace")]
    workspace: Option<String>,
    /// Path to config.json (help-visibility only; consumed pre-parse)
    #[arg(long = "config")]
    #[arg(hide = true)]
    config: Option<String>,
}

/// Scenario: runServer
fn run_server(args: &[String]) -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(run_server_async(args.to_vec()));
    std::process::exit(code);
}

async fn run_server_async(args: Vec<String>) -> i32 {
    // 1. Pre-flag config/workspace handling
    let config_path = peek_flag(&args, &["-config", "--config"]);
    if is_plugin_launch(&args) && std::env::var("RUNTIME_MODE").unwrap_or_default().is_empty() {
        std::env::set_var("RUNTIME_MODE", "plugin");
    }
    let ws = peek_flag(&args, &["-workspace", "--workspace"]);
    if !ws.is_empty()
        && std::env::var("IBIS_WORKSPACE")
            .unwrap_or_default()
            .is_empty()
    {
        std::env::set_var("IBIS_WORKSPACE", ws);
    }

    // 2. Load config (defaults + config.json + env).
    let mut cfg = if config_path.is_empty() {
        Config::load()
    } else {
        Config::load_from(&[config_path])
    };

    // 3. Flags override config values
    let flags = match ServerFlags::try_parse_from(
        std::iter::once(String::from("ibis-assistant")).chain(args.clone()),
    ) {
        Ok(f) => f,
        Err(err) => {
            // clap prints help/version/parse errors itself; parse failures
            // exit 2 like the flag.ExitOnError.
            err.exit();
        }
    };
    let mode_set = flags.mode.is_some();
    if let Some(v) = flags.port {
        cfg.port = v;
    }
    if let Some(v) = flags.mode {
        cfg.mode = v;
    }
    if let Some(v) = flags.scan {
        cfg.auto_scan = v;
    }
    if let Some(v) = flags.root {
        cfg.discovery_root = v;
    }
    if let Some(v) = flags.log_file {
        cfg.log_file = v;
    }
    if let Some(v) = flags.log_level {
        cfg.log_level = v;
    }
    if let Some(v) = flags.runtime_mode {
        cfg.runtime_mode = v;
    }
    if let Some(v) = flags.workspace {
        cfg.workspace = v;
    }

    // Plugin/live-workspace adjustments
    if cfg.is_plugin() {
        if !mode_set {
            cfg.mode = "stdio".to_string();
        }
        ibis_core::config::apply_plugin_isolation(&mut cfg);
        let _ = ibis_core::workspace::ensure_opened_workspace(&mut cfg);
        cfg.auto_scan = false;
        if !cfg.projects.is_empty() && !cfg.projects[0].working_repo_path.is_empty() {
            cfg.discovery_root = cfg.projects[0].working_repo_path.clone();
            cfg.git_repos
                .push(cfg.projects[0].working_repo_path.clone());
        }
    } else if cfg.live_workspace() {
        let _ = ibis_core::workspace::ensure_opened_workspace(&mut cfg);
    }

    // Initialize the logger (stdout stays clean for stdio children).
    let stdio_quiet = cfg.mode == "stdio" || cfg.is_plugin();
    if let Err(err) = ibis_logger::init(&cfg.log_file, &cfg.log_level, stdio_quiet) {
        if !stdio_quiet {
            eprintln!("Error initializing logger: {err}");
        }
        return 1;
    }
    tracing::info!(
        "[INIT] Flags parsed. Mode: {}, Port: {}",
        cfg.mode,
        cfg.port
    );

    if cfg.is_plugin() {
        collision::warn_plugin_collisions(&cfg);
    }
    if cfg.config_loaded {
        tracing::info!("Configuration loaded from: {}", cfg.config_path);
    } else {
        tracing::warn!("No config file found, using defaults and environment variables.");
    }

    // 3. Discovery
    tracing::info!("Starting Ibis Assistant (Mode: {})...", cfg.mode);
    let mut active_repos: Vec<String> = Vec::new();
    if cfg.auto_scan {
        tracing::info!("Scanning for repositories in {}...", cfg.discovery_root);
        let scanner = ibis_core::discovery::Scanner::new(&cfg.discovery_root);
        match scanner.scan() {
            Ok(repos) => {
                active_repos.extend(repos.iter().cloned());
                tracing::info!("Discovered {} repositories.", repos.len());
                for r in &repos {
                    tracing::debug!("Discovered repo: {r}");
                }
            }
            Err(err) => tracing::warn!("Discovery failed: {err}"),
        }
    }
    if !cfg.git_repos.is_empty() {
        active_repos.extend(cfg.git_repos.iter().cloned());
        tracing::debug!(
            "Added {} manually configured repositories",
            cfg.git_repos.len()
        );
    }

    let plugin_unbound = cfg.is_plugin() && !ibis_core::workspace::plugin_has_bound_workspace(&cfg);
    if plugin_unbound {
        tracing::info!(
            "Plugin stdio has no opened folder yet; serving MCP handshake without SurrealDB or ingest."
        );
    }
    let cfg = Arc::new(cfg);

    // 4./5. Embedded DB + connect (degraded when unreachable — D8).
    let (db_executor, db_process) = if plugin_unbound {
        (None, None)
    } else {
        assembly::ensure_database(&cfg).await
    };

    // 6. AI providers (the embedding half may consult the DB).
    let ai = state::assemble_ai(&cfg, db_executor.clone(), plugin_unbound)
        .await
        .client;

    // Search service + git-bound claim plumbing
    let search = build_search_service(&cfg, db_executor.clone(), &ai);

    // Shared cells the ingestion closure reads late (plugin binding swaps).
    let db_cell: Arc<RwLock<Option<Arc<dyn Executor>>>> = Arc::new(RwLock::new(db_executor));
    let search_cell: Arc<RwLock<Arc<SearchService>>> = Arc::new(RwLock::new(search));

    // Ticketing + repopr
    let db_snapshot = db_cell.read().unwrap().clone();
    let (ticket, _) = assembly::assemble_ticketing(db_snapshot).await;
    if ticket.capabilities().providers.is_empty() {
        tracing::warn!("\u{26a0}\u{fe0f}  No ticketing providers configured. ticket_* tools will return configuration errors.");
    }
    let ticket_cfg = ticket.config().clone();
    let repopr = assembly::assemble_repopr(&ticket_cfg);

    // Ingestion queue — wired before the state exists,
    // capturing the shared cells so the plugin late binding is visible.
    let ingestion = Arc::new(ibis_ingest_dynamic::queue::ProjectIngestionManager::new(
        cfg.clone(),
        db_cell.read().unwrap().clone(),
    ));
    {
        let mut manager = Arc::clone(&ingestion);
        // Exclusive until the first enqueue (single-threaded assembly).
        if let Some(m) = Arc::get_mut(&mut manager) {
            assembly::wire_ingestion_manager(
                m,
                cfg.clone(),
                db_cell.clone(),
                ai.clone(),
                ticket.clone(),
                search_cell.clone(),
            );
        }
    }

    let state = Arc::new(AppState {
        cfg: Arc::new(RwLock::new((*cfg).clone())),
        db: db_cell,
        search: search_cell,
        ai: ai.clone(),
        ticket,
        repopr,
        sessions: Mutex::new(std::collections::HashMap::new()),
        ingestion,
        active_repos,
        db_process: Mutex::new(db_process),
    });

    // Background workers
    let shutdown = tokio_util::sync::CancellationToken::new();
    start_log_watcher(&state, shutdown.child_token());
    assembly::start_notification_aggregator(&state, shutdown.child_token());
    assembly::start_polling_manager(&state, shutdown.child_token());
    assembly::trigger_background_indexing(state.clone(), shutdown.child_token());
    let batch_manager = assembly::start_batch_manager(&state);

    // 7./8. MCP server + run loop.
    let with_logging = state.config().mode != "stdio";
    let code = if state.config().mode == "sse" {
        run_http(state.clone(), with_logging, shutdown.clone()).await
    } else {
        run_stdio(state.clone(), with_logging).await
    };

    // GRACEFUL SHUTDOWN SEQUENCE
    shutdown.cancel();
    tracing::info!("Waiting for background tasks to finish...");
    tracing::info!("Stopping AI workers...");
    ai.stop().await;
    if let Some(manager) = batch_manager {
        tracing::info!("Stopping Batch Manager...");
        manager.stop().await;
    }
    tracing::info!("Stopping the ProjectIngestionManager...");
    state.ingestion.stop().await;
    if state.db().is_some() {
        tracing::info!("Closing database connection...");
        // Dropping the executor closes the underlying connection.
        *state.db.write().unwrap() = None;
    }
    // Give the websocket connection time to close before killing the DB
    // process ( slept 1s).
    tokio::time::sleep(Duration::from_secs(1)).await;
    let process = state.db_process.lock().unwrap().take();
    if let Some(mut process) = process {
        tracing::info!("Stopping embedded database process...");
        if let Err(err) = process.stop().await {
            tracing::error!("Error stopping database: {err}");
        }
    }
    code
}

/// Builds the search service with the git-bound claim plumbing and the
/// runtime policy
fn build_search_service(
    cfg: &Config,
    db: Option<Arc<dyn Executor>>,
    ai: &Arc<ibis_ai::Client>,
) -> Arc<SearchService> {
    let mut service = SearchService::new(
        db,
        Some(Arc::new(state::ClientAIAdapter(ai.clone())) as Arc<dyn ibis_ai::AIProvider>),
    );
    let runner = ibis_git::GitRunner::anonymous();
    service.git_anchors = Some(Arc::new(ibis_git::GitAnchorRunner::new(runner.clone())));
    service.claim_writer = Some(Arc::new(ibis_git::GitCommitter::new(runner.clone())));
    service.notes = Some(Arc::new(ibis_git::GitNotesRunner::new(runner)));
    service.personal_claims_policy =
        ibis_search::memory::claim_store::resolve_personal_claims_policy(
            &cfg.memory.git_store.personal_claims,
            &cfg.runtime_mode,
        );
    service.hot_ref_cache_disabled = cfg.world_state.code_cache_disabled;
    Arc::new(service)
}

fn start_log_watcher(state: &Arc<AppState>, shutdown: tokio_util::sync::CancellationToken) {
    let cfg = state.config();
    if cfg.logs_root.is_empty() || state.db().is_none() {
        return;
    }
    let mut watcher = ibis_ingest_logs::Watcher::new(Some(cfg), state.db(), Some(state.ai.clone()));
    if let Err(err) = watcher.start() {
        tracing::error!("Failed to start Log Watcher: {err}");
        return;
    }
    tokio::spawn(async move {
        shutdown.cancelled().await;
        watcher.stop().await;
    });
}

/// The HTTP (sse-mode) run loop —
async fn run_http(
    state: Arc<AppState>,
    with_logging: bool,
    shutdown: tokio_util::sync::CancellationToken,
) -> i32 {
    let cfg = state.config();
    let mcp_service: StreamableHttpService<mcp_server::IbisServer, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                move || Ok(mcp_server::IbisServer::new(state.clone(), with_logging))
            },
            Arc::new(LocalSessionManager::default()),
            ibis_httpserver::streamable_http_config(),
        );
    let memory: Option<Arc<dyn ibis_settings::MemoryService>> =
        Some(Arc::new(ibis_settings::SearchMemoryAdapter(state.search())));
    let app = ibis_httpserver::mux::handler(
        cfg.clone(),
        mcp_service,
        registry::discovery_tools(),
        Some(ibis_httpserver::log_upload_handler(
            cfg.clone(),
            state.db(),
            Some(state.ai.clone()),
        )),
        Some(ibis_settings::settings_api_router(
            state.cfg.clone(),
            memory,
        )),
        Some(ibis_settings::settings_ui_router()),
    );

    let listen_addr = cfg.listen_addr();
    let listener = match tokio::net::TcpListener::bind(&listen_addr).await {
        Ok(l) => l,
        Err(err) => {
            tracing::error!("Server error: failed to bind {listen_addr}: {err}");
            return 1;
        }
    };
    tracing::info!("Starting Streamable HTTP at {listen_addr}...");
    tracing::info!("Ibis Assistant is operational. Press Ctrl+C to stop.");
    let serve = tokio::spawn(ibis_httpserver::mux::serve(
        listener,
        app,
        shutdown.child_token(),
    ));

    // Block until a shutdown signal is received ( selected on sigChan).
    tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("Shutdown signal received.");
        }
        _ = shutdown.cancelled() => {
            tracing::info!("Cancellation signal received.");
        }
    }
    shutdown.cancel();
    match serve.await {
        Ok(Ok(())) => {
            tracing::info!("HTTP server stopped gracefully.");
            0
        }
        Ok(Err(err)) => {
            tracing::warn!("HTTP server graceful shutdown failed: {err}");
            1
        }
        Err(err) => {
            tracing::error!("Server error: {err}");
            1
        }
    }
}

/// The stdio run loop —
async fn run_stdio(state: Arc<AppState>, with_logging: bool) -> i32 {
    tracing::info!("Starting STDIO server...");
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let service = match mcp_server::IbisServer::new(state, with_logging)
        .serve((stdin, stdout))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Server error: {e}");
            return 1;
        }
    };

    // STDIO usually blocks until stdin closes ( signaled SIGTERM then).
    let result = service.waiting().await;
    tracing::info!("stdio closed; shutting down...");
    if let Err(err) = result {
        tracing::error!("Server error: {err}");
        return 1;
    }
    0
}

/// Ctrl+C (+ SIGTERM on Unix) shutdown signal ( signal.Notify).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(t) => t,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
