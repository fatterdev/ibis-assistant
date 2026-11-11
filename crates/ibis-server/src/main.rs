//! `ibis` — the server binary.
//!
//! Command surface ( main:79-168):
//! - no args → help
//! - `run` / `-run` / `--run` / any flag-argument → the server
//! - `serve` / `-serve` / `--serve` → the server (alias of `run`; the
//!   `--team` profile flag turns it into the F4 team server)
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
use ibis_server::{assembly, collision, consolidation, mcp_server, registry, state};
use rmcp::service::ServiceExt as _;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

use state::AppState;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Parent-death watchdog mode (unix): the macOS arm path in
    // `process_tree` re-enters this binary with `-sidecar-watchdog <pgid>`;
    // it must short-circuit before any config/IO initialization.
    #[cfg(unix)]
    if let Some(code) = ibis_core::process_tree::watchdog_exit_code(&argv) {
        std::process::exit(code);
    }

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
        "serve" | "-serve" | "--serve" => run_server(&argv[1..]),
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
        "eval" => {
            // In-process evals (A1 retrieval, D11 decisions): no running
            // server, no MCP registry change — see eval_mode.
            let code = ibis_server::eval_mode::run_eval_cli(&argv[1..]);
            std::process::exit(code);
        }
        "version" | "-version" | "--version" => {
            if !mcp_server::COMMIT.is_empty() && mcp_server::COMMIT != "none" {
                println!("ibis {} ({})", mcp_server::VERSION, mcp_server::COMMIT);
            } else {
                println!("ibis {}", mcp_server::VERSION);
            }
        }
        other => {
            if other.starts_with('-') {
                run_server(&argv);
            } else {
                // : delegate to the CLI client.
                let result = ibis_cli::execute_cli(argv, |payload| {
                    let runtime = tokio::runtime::Runtime::new().expect("cli runtime");
                    runtime.block_on(ibis_cli::call_server_tool(payload))
                });
                match result {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("{error}");
                        std::process::exit(cli_exit_code(&error));
                    }
                }
            }
        }
    }
}

/// Exit code contract for the delegated CLI: `--check` uses 0/1/2; every
/// other failure stays on 1 (the pre-existing CLI behavior).
fn cli_exit_code(error: &ibis_cli::CliError) -> i32 {
    match error {
        ibis_cli::CliError::CheckUsage { .. } => 2,
        ibis_cli::CliError::CheckFailed { .. }
        | ibis_cli::CliError::Io { .. }
        | ibis_cli::CliError::Usage(_)
        | ibis_cli::CliError::Transport(_) => 1,
    }
}

/// `handleService — the daemon/service dispatch seam.///
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
        .unwrap_or_else(|| "ibis".to_string());
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
    println!("  serve       Alias of run; --team enables the team server profile");
    println!("  start       Start the server in the background");
    println!("  stop        Stop the background server");
    println!("  config       Interactive setup (default: fast)");
    println!("  config fast  Fast setup with hardware probe");
    println!("  config full  Full setup (DB, SMTP, ticketing, …)");
    println!("  config show  Print AI/settings summary (keys masked)");
    println!("  config recommend  Print JSON recommendations");
    println!("  version     Print the binary version");
    println!("  install     Install as a Windows service ('ibis')");
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
    println!("\nIn-process commands:");
    println!("  eval retrieval --dataset <path> [--min-recall-5 X]");
    println!("               Retrieval quality eval over the golden dataset");
    println!("  eval decisions --dataset <path> --provider ollama|openai_compat --model <id>");
    println!("               [--min-agreement X]");
    println!("               Bounded-decision calibration over the labeled is_noise dataset");
    println!("  eval observations --dataset <path> [--url <surreal-ws>]");
    println!("               Deterministic C8 cluster gate (exact memberships + hashes)");
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

/// The server long flags (matches the [`ServerFlags`] surface).
const SERVER_LONG_FLAGS: &[&str] = &[
    "port",
    "mode",
    "scan",
    "root",
    "log-file",
    "log-level",
    "runtime-mode",
    "workspace",
    "config",
    "team",
    "consolidate",
    "bind",
    "data-dir",
];

/// Go `flag` parity: the spawn contract everywhere (ide-host, the VSIX MCP
/// provider, `plugin/mcp.json`, `cursor-plugin/mcp.json`, evaluation
/// profiles, the install guides) passes single-dash long flags
/// (`ibis -mode stdio -runtime-mode plugin`), which Go accepted as
/// `--flag`; clap only accepts the double-dash form. Normalize the known
/// server flags before the clap parse — value tokens never matching a flag
/// name pass through untouched, so only the flag style is rewritten.
fn normalize_flag_style(args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let bare = arg.split('=').next().unwrap_or(arg);
            if let Some(name) = bare.strip_prefix('-') {
                if !name.is_empty() && !name.starts_with('-') && SERVER_LONG_FLAGS.contains(&name) {
                    return format!("-{arg}");
                }
            }
            arg.clone()
        })
        .collect()
}

/// The `--team` serve profile (F4 §B.1): the operator asked for the team
/// server, so unset knobs take team defaults while explicit choices (CLI
/// flags, config file) stay untouched:
/// - runtime mode defaults to `server` (db_only personal claims, headless
///   runtime semantics);
/// - the bind address defaults to `127.0.0.1` — LAN exposure requires an
///   explicit `--bind <lan-address>` (trust boundary is network reachability,
///   no token/auth);
/// - the `.ibis/memory` projection turns on unless the config file explicitly
///   disables it.
///
/// A no-op outside the profile and in plugin isolation (the spawn contract
/// never carries team flags, and isolation owns its paths).
fn apply_team_profile(cfg: &mut Config, team_profile: bool) {
    if !team_profile || cfg.is_plugin() {
        return;
    }
    if cfg.runtime_mode.trim().is_empty() {
        cfg.runtime_mode = "server".to_string();
    }
    if cfg.bind_address.trim().is_empty() {
        cfg.bind_address = "127.0.0.1".to_string();
    }
    if ibis_core::config::projection_explicitly_set(&cfg.config_path) != Some(false) {
        cfg.memory.projection_enabled = true;
    }
}

/// The `--consolidate` opt-in (F4 §B.5): the flag forces the daemon-owned
/// consolidation loop on, overriding a config `ai.consolidation.enabled`
/// false (CLI flags win over the config file, as everywhere else).
fn apply_consolidation_flag(cfg: &mut Config, consolidate: bool) {
    if consolidate {
        cfg.ai.consolidation.enabled = true;
    }
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
    /// Team server profile: server runtime, loopback bind default, .ibis/memory
    /// projection on (F4 §B.1)
    #[arg(long = "team")]
    team: bool,
    /// Run the daemon-owned consolidation loop (F4 §B.5; off by default,
    /// overrides ai.consolidation.enabled=false in the config)
    #[arg(long = "consolidate")]
    consolidate: bool,
    /// Address to bind the HTTP listener to (default: runtime-mode driven;
    /// the team profile defaults to 127.0.0.1)
    #[arg(long = "bind")]
    bind: Option<String>,
    /// Data directory for the embedded SurrealDB store (team hosts keep the
    /// team store out of personal plugin data)
    #[arg(long = "data-dir")]
    data_dir: Option<String>,
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

/// Routes panics through `tracing` so they land in the server log file as
/// ERROR lines (payload, thread, location, backtrace) instead of vanishing
/// on a stderr nobody reads in stdio/plugin mode — the regression where a
/// `clamp(min > max)` panic killed the handler task and the MCP call hung
/// with the log frozen right before the next `tracing::info!`. The previous
/// hook still runs, keeping the default behavior on interactive shells.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = ibis_server::mcp_server::panic_payload_message(info.payload());
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            thread = thread,
            location = location.as_str(),
            "panic: {message}\nbacktrace:\n{backtrace}"
        );
        previous(info);
    }));
}

async fn run_server_async(args: Vec<String>) -> i32 {
    // 0. Go-style single-dash flags (`-mode stdio`) normalized to clap's
    // `--mode stdio` before any consumer.
    let args = normalize_flag_style(&args);

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
        std::iter::once(String::from("ibis")).chain(args.clone()),
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
    if let Some(v) = flags.bind {
        cfg.bind_address = v;
    }
    if let Some(v) = flags.data_dir {
        cfg.db_data_path = v;
    }
    apply_consolidation_flag(&mut cfg, flags.consolidate);

    // The `--team` profile (F4 §B.1): server runtime semantics, loopback-safe
    // bind default, projection on — explicit choices (flags/config) win.
    apply_team_profile(&mut cfg, flags.team);

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
    if let Err(err) = ibis_logger::init(&cfg.log_file, &cfg.log_level, stdio_quiet, &cfg.telemetry)
    {
        if !stdio_quiet {
            eprintln!("Error initializing logger: {err}");
        }
        return 1;
    }
    install_panic_hook();
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

    // Team server startup banner (F4 §B.1): bind address + actor header
    // reminder + the no-auth trust boundary, stated where the operator looks.
    if flags.team {
        tracing::info!(
            "[TEAM] team server profile active — bind {} (LAN exposure requires an explicit --bind <lan-address>)",
            cfg.listen_addr()
        );
        tracing::info!(
            "[TEAM] actor identity: clients send the X-Ibis-Actor header (CLI: IBIS_ACTOR env)"
        );
        tracing::info!("[TEAM] no authentication: the trust boundary is network reachability");
        tracing::info!("[TEAM] .ibis/memory projection enabled (team-reviewable docs)");
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
    // F0.5: the MCP-sampling session registry — tool dispatch attaches the
    // client peer, the `mcp_sampling` reasoning leg sends
    // `sampling/createMessage` over it.
    let sampling_registry = Arc::new(ibis_server::sampling::SamplingSessionRegistry::new());
    let ai = state::assemble_ai(
        &cfg,
        db_executor.clone(),
        plugin_unbound,
        sampling_registry.clone(),
    )
    .await
    .client;

    // Search service + git-bound claim plumbing
    let search = build_search_service(&cfg, db_executor.clone(), &ai);

    // Shared cells the ingestion closure reads late (plugin binding swaps).
    let db_cell: Arc<RwLock<Option<Arc<dyn Executor>>>> = Arc::new(RwLock::new(db_executor));
    let search_cell: Arc<RwLock<Arc<SearchService>>> = Arc::new(RwLock::new(search));

    // Ticketing + repopr
    let db_snapshot = db_cell.read().unwrap().clone();
    let (ticket, _) = assembly::assemble_ticketing(db_snapshot.clone()).await;
    if ticket.capabilities().providers.is_empty() {
        tracing::warn!("\u{26a0}\u{fe0f}  No ticketing providers configured. ticket_* tools will return configuration errors.");
    }
    // F0.6: the tool list starts with this visibility (see the AppState
    // literal below).
    let ticketing_unconfigured = ticket.capabilities().providers.is_empty();
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

    // E16: durable task runner. The store exists only when the DB does; the
    // runner is fail-open at the call sites.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task_policy = ibis_optimization::tasks::TaskPolicy::new(
        cfg.tasks.lease_secs,
        cfg.tasks.heartbeat_secs,
        cfg.tasks.max_recoveries,
        cfg.tasks.retention_days,
    );
    let task_shutdown = shutdown.child_token();
    let tasks = db_snapshot.as_ref().map(|db| {
        Arc::new(ibis_server::tasks::TaskRunner::new(
            Arc::new(ibis_server::tasks::DbTaskStore::new(db.clone())),
            task_policy,
            task_shutdown.clone(),
        ))
    });
    // E16 §4.2/§4.7: recovery + retention prune at boot, independent of the
    // opt-in consolidation loop (a task created by the tool must recover too).
    if let Some(tasks) = &tasks {
        tasks.recover_and_prune().await;
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
        resource_subs: ibis_server::resources::ResourceSubscriptions::default(),
        sampling: sampling_registry,
        tasks: Mutex::new(tasks),
        task_shutdown,
        // F0.6: the published visibility matches the assembled ticketing
        // service (no providers → the ticket_* group starts hidden).
        tool_discovery: ibis_server::tool_discovery::ToolDiscovery::new(!ticketing_unconfigured),
    });
    // E16: task state changes feed `notifications/resources/updated`.
    if let Some(tasks) = state.tasks() {
        tasks.set_events(Some(Arc::new(ibis_server::tasks::ResourceTaskEvents::new(
            Arc::downgrade(&state),
        ))));
    }

    // Background workers
    start_log_watcher(&state, shutdown.child_token());
    assembly::start_notification_aggregator(&state, shutdown.child_token());
    assembly::start_polling_manager(&state, shutdown.child_token());
    assembly::trigger_background_indexing(state.clone(), shutdown.child_token());
    let batch_manager = assembly::start_batch_manager(&state);
    // F4 §B.5: the consolidation loop belongs to the daemon alone (opt-in,
    // off by default; stdio/plugin client surfaces never start it).
    let consolidation_handle = consolidation::start(&state, shutdown.child_token());

    // 7./8. MCP server + run loop.
    let with_logging = state.config().mode != "stdio";
    let code = if state.config().mode == "sse" {
        run_http(state.clone(), with_logging, shutdown.clone()).await
    } else {
        run_stdio(state.clone(), with_logging).await
    };

    // GRACEFUL SHUTDOWN SEQUENCE
    shutdown.cancel();
    // Stop the AI workers first so an in-flight LLM call aborts; the runner
    // then requeues at the item boundary (E16 §4.2).
    tracing::info!("Stopping AI workers...");
    ai.stop().await;
    // E16: bound the wait so `ibis stop` stays fast; on timeout abort the
    // loop so a detached pass cannot write after the DB is dropped.
    if let Some(mut handle) = consolidation_handle {
        match tokio::time::timeout(Duration::from_secs(10), &mut handle).await {
            Ok(_) => {}
            Err(_) => {
                tracing::warn!("Consolidation loop did not stop within 10s; aborting it");
                handle.abort();
            }
        }
    }
    tracing::info!("Waiting for background tasks to finish...");
    if let Some(manager) = batch_manager {
        tracing::info!("Stopping Batch Manager...");
        manager.stop().await;
    }
    tracing::info!("Stopping the ProjectIngestionManager...");
    state.ingestion.stop().await;
    // F17: flush pending OTLP spans before the database connection closes.
    // Bounded: an unreachable collector must not delay the exit beyond 5s.
    tracing::info!("Flushing telemetry...");
    ibis_logger::shutdown_telemetry(Duration::from_secs(5));
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
    service.fusion_mode = ibis_search::retrieval_boost::FusionMode::resolve(&cfg.search.fusion);
    service.candidate_window =
        (cfg.search.candidates as usize).max(ibis_search::search::VECTOR_CHANNEL_LIMIT);
    service.query_expansion =
        ibis_search::retrieval_boost::QueryExpansionMode::resolve(&cfg.search.query_expansion);
    service.rerank_mode = ibis_search::rerank::RerankMode::resolve(&cfg.search.rerank);
    service.schema_enforcement =
        ibis_search::memory::details::SchemaEnforcement::resolve(&cfg.memory.schema_enforcement);
    service.observations =
        ibis_search::memory::observations::ObservationSettings::from_config(&cfg.ai.observations);
    service.agentic = ibis_search::agentic::AgenticSettings::from_config(&cfg.search.agentic);
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
    // F0.6: a settings UI apply is a config change — hot-reload the
    // ticketing provider set and broadcast `notifications/tools/list_changed`
    // when the visibility gate flips.
    let on_change_state = state.clone();
    let app = ibis_httpserver::mux::handler(
        cfg.clone(),
        mcp_service,
        registry::discovery_tools(),
        Some(ibis_httpserver::log_upload_handler(
            cfg.clone(),
            state.db(),
            Some(state.ai.clone()),
        )),
        Some(ibis_settings::settings_api_router_with_on_change(
            state.cfg.clone(),
            memory,
            Arc::new(move || {
                let state = on_change_state.clone();
                tokio::spawn(async move {
                    assembly::reload_ticketing(&state).await;
                });
            }),
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

#[cfg(test)]
mod flag_parity_tests {
    use super::*;

    fn parse(args: &[&str]) -> ServerFlags {
        let normalized =
            normalize_flag_style(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        ServerFlags::try_parse_from(std::iter::once("ibis".to_string()).chain(normalized))
            .expect("normalized spawn args parse")
    }

    /// The exact spawn vectors every IDE surface uses (`-mode stdio
    /// -runtime-mode plugin`) parse like Go's flag package accepted them.
    #[test]
    fn ide_spawn_args_parse_with_single_dash() {
        let flags = parse(&["-mode", "stdio", "-runtime-mode", "plugin"]);
        assert_eq!(flags.mode.as_deref(), Some("stdio"));
        assert_eq!(flags.runtime_mode.as_deref(), Some("plugin"));
    }

    /// `-flag=value` (single dash) and `--flag value` (double dash) keep
    /// working; a `--flag=value` form is untouched.
    #[test]
    fn equals_form_and_double_dash_are_preserved() {
        let flags = parse(&["-mode=stdio", "--runtime-mode", "personal", "-port=3999"]);
        assert_eq!(flags.mode.as_deref(), Some("stdio"));
        assert_eq!(flags.runtime_mode.as_deref(), Some("personal"));
        assert_eq!(flags.port, Some(3999));
        let flags = parse(&["--mode", "sse", "--log-level", "DEBUG"]);
        assert_eq!(flags.mode.as_deref(), Some("sse"));
        assert_eq!(flags.log_level.as_deref(), Some("DEBUG"));
    }

    /// Value tokens that begin with `-` (unknown names) pass through
    /// untouched — only the known server flags are rewritten.
    #[test]
    fn unknown_dashed_values_pass_through() {
        let args = normalize_flag_style(
            &["-root", "-mydir", "-log-level", "-debug", "-h"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(args, ["--root", "-mydir", "--log-level", "-debug", "-h"]);
    }

    /// Every long flag of the [`ServerFlags`] surface round-trips through
    /// the single-dash spelling.
    #[test]
    fn all_server_flags_accept_the_single_dash_form() {
        let cases: Vec<(&str, &str)> = vec![
            ("-port", "4123"),
            ("-mode", "sse"),
            ("-scan", "true"),
            ("-root", "C:/repos"),
            ("-log-file", "server.log"),
            ("-log-level", "INFO"),
            ("-runtime-mode", "plugin"),
            ("-workspace", "C:/dev/repo"),
            ("-config", "config.json"),
        ];
        let raw: Vec<String> = cases
            .iter()
            .flat_map(|(flag, value)| vec![flag.to_string(), value.to_string()])
            .collect();
        let normalized = normalize_flag_style(&raw);
        let flags =
            ServerFlags::try_parse_from(std::iter::once(String::from("ibis")).chain(normalized))
                .expect("all single-dash flags parse");
        assert_eq!(flags.port, Some(4123));
        assert_eq!(flags.mode.as_deref(), Some("sse"));
        assert_eq!(flags.scan, Some(true));
        assert_eq!(flags.root.as_deref(), Some("C:/repos"));
        assert_eq!(flags.log_file.as_deref(), Some("server.log"));
        assert_eq!(flags.log_level.as_deref(), Some("INFO"));
        assert_eq!(flags.runtime_mode.as_deref(), Some("plugin"));
        assert_eq!(flags.workspace.as_deref(), Some("C:/dev/repo"));
        assert_eq!(flags.config.as_deref(), Some("config.json"));
    }

    /// The F4 team-server flags round-trip through the single-dash spelling.
    #[test]
    fn team_flags_accept_the_single_dash_form() {
        let flags = parse(&["-team", "-bind", "0.0.0.0", "-data-dir", "C:/team-data"]);
        assert!(flags.team);
        assert_eq!(flags.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(flags.data_dir.as_deref(), Some("C:/team-data"));
    }

    /// The `--consolidate` opt-in (F4 §B.5) parses in both spellings.
    #[test]
    fn consolidate_flag_accepts_both_spellings() {
        let flags = parse(&["-consolidate"]);
        assert!(flags.consolidate);
        let flags = parse(&["--consolidate"]);
        assert!(flags.consolidate);
        let flags = parse(&["--team"]);
        assert!(!flags.consolidate);
    }

    /// `--consolidate` forces the loop on over a config `false`; without the
    /// flag the config choice stands (either way).
    #[test]
    fn consolidate_flag_overrides_the_config() {
        let mut cfg = Config::default();
        assert!(!cfg.ai.consolidation.enabled);
        apply_consolidation_flag(&mut cfg, true);
        assert!(cfg.ai.consolidation.enabled);

        let mut cfg = Config::default();
        apply_consolidation_flag(&mut cfg, false);
        assert!(!cfg.ai.consolidation.enabled);

        let mut cfg = Config::default();
        cfg.ai.consolidation.enabled = true;
        apply_consolidation_flag(&mut cfg, false);
        assert!(cfg.ai.consolidation.enabled);
    }
}

#[cfg(test)]
mod team_profile_tests {
    use super::*;

    fn parse(args: &[&str]) -> ServerFlags {
        let normalized =
            normalize_flag_style(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        ServerFlags::try_parse_from(std::iter::once("ibis".to_string()).chain(normalized))
            .expect("team args parse")
    }

    /// `serve --team` (F4 §B.1): server runtime semantics, loopback-safe bind
    /// default (LAN exposure is an explicit --bind), projection on.
    #[test]
    fn team_profile_applies_server_defaults() {
        let flags = parse(&["--team"]);
        assert!(flags.team);
        let mut cfg = Config::default();
        apply_team_profile(&mut cfg, flags.team);
        assert_eq!(cfg.runtime_mode, "server");
        assert_eq!(cfg.bind_address, "127.0.0.1");
        assert_eq!(cfg.listen_addr(), "127.0.0.1:3030");
        assert!(cfg.memory.projection_enabled);
    }

    /// The profile only fills unset knobs: an explicit --bind (applied to the
    /// config before the profile) and an explicit runtime mode stay untouched.
    #[test]
    fn team_profile_defaults_do_not_override_explicit_choices() {
        let flags = parse(&["--team", "--bind", "0.0.0.0"]);
        assert_eq!(flags.bind.as_deref(), Some("0.0.0.0"));
        let mut cfg = Config {
            bind_address: "0.0.0.0".to_string(),
            runtime_mode: "personal".to_string(),
            ..Default::default()
        };
        apply_team_profile(&mut cfg, flags.team);
        assert_eq!(cfg.bind_address, "0.0.0.0");
        assert_eq!(cfg.runtime_mode, "personal");
    }

    /// An explicit `memory.projection_enabled: false` in the config file
    /// survives the team profile; an explicit true stays true.
    #[test]
    fn explicit_config_projection_survives_the_team_profile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        let flags = parse(&["--team"]);

        std::fs::write(&path, r#"{"memory": {"projection_enabled": false}}"#).expect("write");
        let mut cfg = Config {
            config_path: path.display().to_string(),
            ..Default::default()
        };
        apply_team_profile(&mut cfg, flags.team);
        assert!(!cfg.memory.projection_enabled);

        std::fs::write(&path, r#"{"memory": {"projection_enabled": true}}"#).expect("write");
        let mut cfg = Config {
            config_path: path.display().to_string(),
            ..Default::default()
        };
        apply_team_profile(&mut cfg, flags.team);
        assert!(cfg.memory.projection_enabled);
    }

    /// Plugin isolation owns its paths and spawn contract: the team profile
    /// is a no-op there.
    #[test]
    fn team_profile_is_a_noop_in_plugin_isolation() {
        let flags = parse(&["--team"]);
        let mut cfg = Config {
            runtime_mode: "plugin".to_string(),
            ..Default::default()
        };
        apply_team_profile(&mut cfg, flags.team);
        assert_eq!(cfg.runtime_mode, "plugin");
        assert!(cfg.bind_address.is_empty());
        assert!(!cfg.memory.projection_enabled);
    }

    /// Without `--team` nothing changes (plain `ibis serve` = `ibis run`).
    #[test]
    fn profile_is_a_noop_without_the_team_flag() {
        let flags = parse(&["--port", "3031"]);
        assert!(!flags.team);
        let mut cfg = Config::default();
        apply_team_profile(&mut cfg, flags.team);
        assert!(cfg.runtime_mode.is_empty());
        assert!(cfg.bind_address.is_empty());
        assert!(!cfg.memory.projection_enabled);
    }
}

#[cfg(test)]
mod panic_hook_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// In-memory writer shared with the fmt subscriber (same pattern as
    /// `tests/degraded_log_test.rs`).
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A panic captured by the hook is emitted through tracing with payload,
    /// thread, location and backtrace — the line the stdio/plugin log file
    /// never saw when the fetch-limit clamp panicked and the MCP call hung.
    #[test]
    fn panic_hook_logs_payload_location_and_backtrace() {
        install_panic_hook();
        let shared = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let writer = shared.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::ERROR)
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::subscriber::with_default(subscriber, || {
                panic!("panic-hook regression payload");
            });
        }));
        assert!(panicked.is_err(), "the probe must panic");
        let out = String::from_utf8(shared.0.lock().unwrap().clone()).expect("capture is utf-8");
        assert!(
            out.contains("panic: panic-hook regression payload"),
            "payload missing:\n{out}"
        );
        assert!(out.contains("main.rs:"), "location missing:\n{out}");
        assert!(out.contains("backtrace:\n"), "backtrace missing:\n{out}");
    }
}

#[cfg(test)]
mod cli_exit_code_tests {
    use super::*;

    #[test]
    fn cli_exit_codes_match_contract() {
        assert_eq!(
            cli_exit_code(&ibis_cli::CliError::CheckFailed {
                message: "x".into()
            }),
            1
        );
        assert_eq!(
            cli_exit_code(&ibis_cli::CliError::CheckUsage {
                message: "x".into()
            }),
            2
        );
        assert_eq!(
            cli_exit_code(&ibis_cli::CliError::Io {
                message: "x".into()
            }),
            1
        );
        assert_eq!(cli_exit_code(&ibis_cli::CliError::Transport("x".into())), 1);
        assert_eq!(cli_exit_code(&ibis_cli::CliError::Usage("x".into())), 1);
    }
}
