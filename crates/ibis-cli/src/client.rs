//! CLI command tree and MCP payload construction.
//!
//! built the tree by hand with one `flag.NewFlagSet` per subcommand and a
//! `switch` dispatcher; this crate uses clap derive with the **same subcommand
//! names, flag names, defaults and custom error messages**, so each
//! subcommand still maps 1:1 onto a single MCP tool call:
//!
//! | CLI | MCP tool |
//! |---|---|
//! | `ask` | `ask_project` |
//! | `ingest code` / `ingest git` | `ingest_code` / `init_project` |
//! | `ticket search|my-issues|get|create|update|comment|assign|transition|resolve|close|reopen|list-statuses|list-projects` | `ticket_*` |
//! | `pr create` / `pr complete` | `repo_pr_create` / `repo_pr_complete` |
//! | `logs analyze` | `analyze_logs` |
//! | `credentials add` | `git_configure_credentials` |
//! | `memory add` (untyped) | `provide_collaborative_memory` |
//! | `memory add` (any typed flag) | `memory_add` |
//! | `memory search/expand/update/delete/history/rule/rules/state/failures/why/session/capture/share/bootstrap-git-store/rebuild-git-notes` | `memory_*` / `store_rule` / `get_rules` / ... |
//! | `outcome add` | `save_reasoning_outcome` |
//! | `optimize` | `optimize_knowledge` |
//!
//! Faithful behaviors preserved from
//! - Usage/validation failures carry the exact message text (as
//!   [`CliError::Usage`]; printed to stdout + exit 1).
//! - Numeric args (`limit`, `depth`, `iterations`, `confidence`) travel as
//!   JSON floats, mirroring the `float64(...)` conversions (the server arg
//!   helpers read `float64` — ).
//! - `pr complete --delete-branch/--squash` send the **string** `"true"`
//!   because the server declares `WithString("delete_source_branch")` and
//!   parses via `getBoolArg` — parity wart.
//! - `memory add` with an explicitly-set flag (even `--confidence 0`) routes
//!   to the typed `memory_add` tool; untouched flags route to the legacy
//!   `provide_collaborative_memory`.
//! - `ticket search --limit` (default 20) and `optimize --iterations`
//!   (default 10) are always sent, even when untouched.
//! - Empty-string flags are treated as absent (`if *x != ""` pattern).

use std::ffi::OsString;

use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{json, Map, Value};

use crate::{CliError, ToolCallPayload};

type Args = Map<String, Value>;

/// Parsed CLI (`ibis-assistant <command> [options]`) — clap equivalent of 's
/// `ExecuteCLI` switch. `help`/`-h`/`--help` come from clap likewise's
/// hardcoded cases; an unknown subcommand is clap's `unrecognized subcommand`
/// error (stderr, exit 2) instead of the stdout + exit 1 (L-A07).
#[derive(Debug, Parser)]
#[command(
    name = "ibis-assistant",
    about = "Ibis Assistant CLI (MCP Streamable HTTP POST /mcp)",
    disable_version_flag = true
)]
pub struct Cli {
    /// Dispatched subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// Root subcommands, mirroring.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Ask a reasoning question about indexed code
    Ask {
        /// Question about the indexed code
        query: Option<String>,
        /// Context branch or commit
        #[arg(long)]
        branch: Option<String>,
    },
    /// Sync/index code or Git history
    Ingest {
        #[command(subcommand)]
        kind: IngestKind,
    },
    /// Tickets on Redmine / Jira / Azure DevOps
    Ticket {
        #[command(subcommand)]
        action: TicketAction,
    },
    /// Pull requests
    Pr {
        #[command(subcommand)]
        action: PrAction,
    },
    /// Analyze log text
    Logs {
        #[command(subcommand)]
        action: LogsAction,
    },
    /// Persist Git credentials
    Credentials {
        #[command(subcommand)]
        action: CredentialsAction,
    },
    /// Typed memories: add, search, expand, update, delete, history, rule, rules, state, failures, why, session, capture, share, bootstrap-git-store, rebuild-git-notes
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Save reasoning outcomes
    Outcome {
        #[command(subcommand)]
        action: OutcomeAction,
    },
    /// RAFT knowledge-base optimization
    Optimize {
        /// RAFT iterations
        #[arg(long, default_value_t = 10)]
        iterations: i64,
    },
}

/// `ingest` targets.
#[derive(Debug, Subcommand)]
pub enum IngestKind {
    /// Index the local code tree
    Code {
        /// Local repository path
        #[arg(long)]
        path: Option<String>,
    },
    /// Sync/index Git history (bootstrap a project)
    Git {
        /// Project name (required)
        #[arg(long)]
        name: Option<String>,
        /// Git origin URL (required)
        #[arg(long)]
        url: Option<String>,
        /// Branch to track (required)
        #[arg(long)]
        branch: Option<String>,
        /// Specific commit (optional)
        #[arg(long)]
        commit: Option<String>,
    },
}

/// `ticket` actions.
#[derive(Debug, Subcommand)]
pub enum TicketAction {
    /// Search tickets
    Search {
        /// Provider (redmine/jira/azure_devops)
        #[arg(long)]
        provider: Option<String>,
        /// Text search query
        #[arg(long)]
        query: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
        /// Status
        #[arg(long)]
        status: Option<String>,
        /// Result limit
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Search issues assigned to the caller
    MyIssues {
        /// Provider (redmine/jira/azure_devops)
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
    },
    /// Get one ticket by ID
    Get {
        /// Ticket ID
        id: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
    },
    /// Create a ticket
    Create {
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key (required)
        #[arg(long)]
        project: Option<String>,
        /// Ticket title (required)
        #[arg(long)]
        title: Option<String>,
        /// Description
        #[arg(long)]
        desc: Option<String>,
        /// Priority
        #[arg(long)]
        priority: Option<String>,
    },
    /// Update a ticket
    Update {
        /// Ticket ID
        id: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
        /// Comment or notes
        #[arg(long)]
        notes: Option<String>,
        /// Status
        #[arg(long)]
        status: Option<String>,
        /// Assignee
        #[arg(long)]
        assignee: Option<String>,
    },
    /// Comment on a ticket
    Comment {
        /// Ticket ID
        id: Option<String>,
        /// Comment text
        comment: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
    },
    /// Assign a ticket
    Assign {
        /// Ticket ID
        id: Option<String>,
        /// Assignee
        assignee: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
    },
    /// Transition a ticket
    Transition {
        /// Ticket ID
        id: Option<String>,
        /// Transition name
        transition: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
    },
    /// Mark a ticket resolved
    Resolve {
        /// Ticket ID
        id: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
        /// Comment
        #[arg(long)]
        notes: Option<String>,
    },
    /// Mark a ticket closed
    Close {
        /// Ticket ID
        id: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
        /// Comment
        #[arg(long)]
        notes: Option<String>,
    },
    /// Reopen a ticket
    Reopen {
        /// Ticket ID
        id: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
        /// Comment
        #[arg(long)]
        notes: Option<String>,
    },
    /// List a project's ticket statuses
    ListStatuses {
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project key
        #[arg(long)]
        project: Option<String>,
    },
    /// List the provider's projects
    ListProjects {
        /// Provider
        #[arg(long)]
        provider: Option<String>,
    },
}

/// `pr` actions.
#[derive(Debug, Subcommand)]
pub enum PrAction {
    /// Create a pull request
    Create {
        /// Provider (azure_devops)
        #[arg(long)]
        provider: Option<String>,
        /// Project name
        #[arg(long)]
        project: Option<String>,
        /// Git origin URL
        #[arg(long)]
        url: Option<String>,
        /// Repository name
        #[arg(long)]
        repo: Option<String>,
        /// Source branch (required)
        #[arg(long)]
        source: Option<String>,
        /// Target branch
        #[arg(long)]
        target: Option<String>,
        /// PR title
        #[arg(long)]
        title: Option<String>,
        /// PR description
        #[arg(long)]
        desc: Option<String>,
    },
    /// Complete a pull request
    Complete {
        /// PR ID
        pr_id: Option<String>,
        /// Provider
        #[arg(long)]
        provider: Option<String>,
        /// Project name
        #[arg(long)]
        project: Option<String>,
        /// Repository name
        #[arg(long)]
        repo: Option<String>,
        /// Delete the source branch
        #[arg(long)]
        delete_branch: bool,
        /// Squash merge
        #[arg(long)]
        squash: bool,
    },
}

/// `logs` actions.
#[derive(Debug, Subcommand)]
pub enum LogsAction {
    /// Analyze log text
    Analyze {
        /// Project name (required)
        #[arg(long)]
        project: Option<String>,
        /// Log file name (optional)
        #[arg(long)]
        file: Option<String>,
        /// Log text to analyze (required)
        #[arg(long)]
        text: Option<String>,
    },
}

/// `credentials` actions.
#[derive(Debug, Subcommand)]
pub enum CredentialsAction {
    /// Persist credentials for a repository domain or URL
    Add {
        /// Repository domain or URL (required)
        #[arg(long)]
        target: Option<String>,
        /// Provider: github, gitlab, azure_devops, generic
        #[arg(long, default_value = "generic")]
        provider: String,
        /// Auth type: token, basic, ssh
        #[arg(long = "auth-type", default_value = "token")]
        auth_type: String,
        /// Token / password (required for token/basic)
        #[arg(long)]
        token: Option<String>,
        /// Username (optional)
        #[arg(long)]
        username: Option<String>,
        /// SSH private key (required for ssh)
        #[arg(long = "ssh-key")]
        ssh_key: Option<String>,
    },
}

/// `memory` actions.
#[derive(Debug, Subcommand)]
pub enum MemoryAction {
    /// Add a memory (typed flags route to the typed tool, rest to the legacy one)
    Add {
        /// Project name (required)
        #[arg(long)]
        project: Option<String>,
        /// Memory text (required)
        #[arg(long)]
        text: Option<String>,
        /// Optional kind (uses typed memory_add when set)
        #[arg(long)]
        kind: Option<String>,
        /// Optional title (typed memory_add only)
        #[arg(long)]
        title: Option<String>,
        /// Optional comma-separated tags (typed memory_add only)
        #[arg(long)]
        tags: Option<String>,
        /// Optional visibility: team|personal (typed memory_add only)
        #[arg(long)]
        visibility: Option<String>,
        /// Optional confidence from 0.0 to 1.0 (typed memory_add only)
        #[arg(long)]
        confidence: Option<f64>,
    },
    /// Search memories
    Search {
        /// Query text
        query: Option<String>,
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Optional comma-separated kind filter
        #[arg(long)]
        kind: Option<String>,
        /// Optional status filter
        #[arg(long)]
        status: Option<String>,
        /// team|personal|all
        #[arg(long)]
        visibility: Option<String>,
        /// Optional comma-separated tag filter
        #[arg(long)]
        tags: Option<String>,
        /// Optional max results
        #[arg(long, default_value_t = 0)]
        limit: i64,
    },
    /// Expand a memory's neighborhood
    Expand {
        /// Memory ID
        id: Option<String>,
        /// Optional traversal depth
        #[arg(long, default_value_t = 0)]
        depth: i64,
    },
    /// Update a memory
    Update {
        /// Memory ID
        id: Option<String>,
        /// Optional new content
        #[arg(long)]
        content: Option<String>,
        /// Optional new title
        #[arg(long)]
        title: Option<String>,
        /// Optional new kind
        #[arg(long)]
        kind: Option<String>,
        /// Optional new status
        #[arg(long)]
        status: Option<String>,
        /// Optional comma-separated replacement tags
        #[arg(long)]
        tags: Option<String>,
        /// Optional new visibility
        #[arg(long)]
        visibility: Option<String>,
    },
    /// Delete a memory
    Delete {
        /// Memory ID
        id: Option<String>,
        /// Permanently delete instead of archiving
        #[arg(long)]
        hard: bool,
    },
    /// Show a memory's supersession history
    History {
        /// Memory ID or subject
        id: Option<String>,
        /// Optional supersession traversal depth (default 1, max 5)
        #[arg(long, default_value_t = 0)]
        depth: i64,
    },
    /// Store a rule
    Rule {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Rule title (required)
        #[arg(long)]
        title: Option<String>,
        /// Rule text (required)
        #[arg(long)]
        content: Option<String>,
        /// Optional comma-separated tags
        #[arg(long)]
        tags: Option<String>,
        /// Optional branch
        #[arg(long)]
        branch: Option<String>,
    },
    /// Get stored rules
    Rules {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Optional semantic filter
        #[arg(long)]
        query: Option<String>,
        /// Optional comma-separated tag filter
        #[arg(long)]
        tags: Option<String>,
        /// Optional max results
        #[arg(long, default_value_t = 0)]
        limit: i64,
    },
    /// Get or set the working state
    State {
        #[command(subcommand)]
        action: StateAction,
    },
    /// Recent failure memories
    Failures {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Optional comma-separated kind filter (default fix,gotcha)
        #[arg(long)]
        kind: Option<String>,
        /// Duration like 7d/30d (default 30d)
        #[arg(long)]
        since: Option<String>,
        /// Optional max results
        #[arg(long, default_value_t = 0)]
        limit: i64,
    },
    /// Ask why a file/symbol/issue changed
    Why {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// File path subject
        #[arg(long)]
        file: Option<String>,
        /// Symbol name subject
        #[arg(long)]
        symbol: Option<String>,
        /// Issue ID subject
        #[arg(long)]
        issue: Option<String>,
        /// Optional max memories returned
        #[arg(long, default_value_t = 0)]
        limit: i64,
    },
    /// Load session context
    Session {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Optional branch
        #[arg(long)]
        branch: Option<String>,
    },
    /// Capture an observation
    Capture {
        /// Observation text
        text: Option<String>,
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// agent|tool|user (default agent)
        #[arg(long)]
        source: Option<String>,
        /// Optional session identifier
        #[arg(long = "session-id")]
        session_id: Option<String>,
    },
    /// Change a memory's visibility
    Share {
        /// Memory ID
        id: Option<String>,
        /// New visibility (team|personal)
        visibility: Option<String>,
    },
    /// Render durable claims into .ibis/claims/ with one bootstrap commit
    BootstrapGitStore {
        /// Project name (optional when unambiguous)
        #[arg(long)]
        project: Option<String>,
        /// Report the planned claim files without writing or committing
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Reconcile refs/notes/ibis-claims from the Surreal claim index
    RebuildGitNotes {
        /// Project name (optional when unambiguous)
        #[arg(long)]
        project: Option<String>,
        /// Report the planned note writes without changing the notes ref
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// `memory state` actions.
#[derive(Debug, Subcommand)]
pub enum StateAction {
    /// Set the working state
    Set {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Optional branch
        #[arg(long)]
        branch: Option<String>,
        /// Current task/working state (required)
        #[arg(long)]
        content: Option<String>,
    },
    /// Get the working state
    Get {
        /// Project name (optional)
        #[arg(long)]
        project: Option<String>,
        /// Optional branch
        #[arg(long)]
        branch: Option<String>,
    },
}

/// `outcome` actions.
#[derive(Debug, Subcommand)]
pub enum OutcomeAction {
    /// Save a reasoning outcome
    Add {
        /// Project name (required)
        #[arg(long)]
        project: Option<String>,
        /// Original question (required)
        #[arg(long)]
        question: Option<String>,
        /// Outcome / conclusion (required)
        #[arg(long)]
        outcome: Option<String>,
        /// Comma-separated useful source IDs (optional)
        #[arg(long)]
        sources: Option<String>,
    },
}

/// Insert a string arg only when set and non-empty.
fn set_str(args: &mut Args, key: &str, value: &Option<String>) {
    if let Some(v) = value {
        if !v.is_empty() {
            args.insert(key.to_string(), Value::String(v.clone()));
        }
    }
}

/// Insert a numeric arg as a JSON float (the `float64(*n)` conversions).
fn set_num(args: &mut Args, key: &str, value: i64) {
    args.insert(key.to_string(), json!(value as f64));
}

impl Command {
    /// Build the MCP [`ToolCallPayload`] this subcommand maps to, applying the
    /// Scenario: handlers' validation (exact messages, empty-string = absent).
    pub fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        match self {
            Command::Ask { query, branch } => {
                let Some(query) = query else {
                    return Err(CliError::Usage(
                        "Error: provide a question.\nUsage: ibis-assistant ask \"your question\" [--branch <branch>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("query".to_string(), json!(query));
                set_str(&mut arguments, "branch_or_commit", &branch);
                Ok(ToolCallPayload::new("ask_project", arguments))
            }
            Command::Ingest { kind } => match kind {
                IngestKind::Code { path } => {
                    let mut arguments = Args::new();
                    set_str(&mut arguments, "path", &path);
                    Ok(ToolCallPayload::new("ingest_code", arguments))
                }
                IngestKind::Git {
                    name,
                    url,
                    branch,
                    commit,
                } => {
                    let (name, url, branch) = (
                        name.unwrap_or_default(),
                        url.unwrap_or_default(),
                        branch.unwrap_or_default(),
                    );
                    if name.is_empty() || url.is_empty() || branch.is_empty() {
                        return Err(CliError::Usage(
                            "Error: --name, --url, and --branch are required for Git ingest."
                                .to_string(),
                        ));
                    }
                    let mut arguments = Args::new();
                    arguments.insert("project_name".to_string(), json!(name));
                    arguments.insert("origin_url".to_string(), json!(url));
                    arguments.insert("branch".to_string(), json!(branch));
                    set_str(&mut arguments, "commit", &commit);
                    Ok(ToolCallPayload::new("init_project", arguments))
                }
            },
            Command::Ticket { action } => action.tool_call(),
            Command::Pr { action } => action.tool_call(),
            Command::Logs { action } => action.tool_call(),
            Command::Credentials { action } => action.tool_call(),
            Command::Memory { action } => action.tool_call(),
            Command::Outcome { action } => action.tool_call(),
            Command::Optimize { iterations } => {
                let mut arguments = Args::new();
                set_num(&mut arguments, "iterations", iterations);
                Ok(ToolCallPayload::new("optimize_knowledge", arguments))
            }
        }
    }
}

impl TicketAction {
    fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        // provider/project pairs repeat across actions (the per-action
        // FlagSets share the same flag names and mapping).
        macro_rules! provider_project {
            ($provider:expr, $project:expr, $arguments:expr) => {{
                set_str(&mut $arguments, "provider", &$provider);
                set_str(&mut $arguments, "project_key", &$project);
            }};
        }
        let payload = match self {
            TicketAction::Search {
                provider,
                query,
                project,
                status,
                limit,
            } => {
                let mut arguments = Args::new();
                // Scenario: always sends the limit (default 20), even untouched.
                set_num(&mut arguments, "limit", limit);
                set_str(&mut arguments, "provider", &provider);
                set_str(&mut arguments, "query", &query);
                set_str(&mut arguments, "project_key", &project);
                set_str(&mut arguments, "status", &status);
                ToolCallPayload::new("ticket_search", arguments)
            }
            TicketAction::MyIssues { provider, project } => {
                let mut arguments = Args::new();
                provider_project!(provider, project, arguments);
                ToolCallPayload::new("ticket_search_my", arguments)
            }
            TicketAction::Get {
                id,
                provider,
                project,
            } => {
                let Some(id) = id else {
                    return Err(CliError::Usage("Error: ticket ID is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                provider_project!(provider, project, arguments);
                ToolCallPayload::new("ticket_get", arguments)
            }
            TicketAction::Create {
                provider,
                project,
                title,
                desc,
                priority,
            } => {
                let (project, title) = (project.unwrap_or_default(), title.unwrap_or_default());
                if project.is_empty() || title.is_empty() {
                    return Err(CliError::Usage(
                        "Error: --project and --title are required.".to_string(),
                    ));
                }
                let mut arguments = Args::new();
                arguments.insert("project_key".to_string(), json!(project));
                arguments.insert("title".to_string(), json!(title));
                set_str(&mut arguments, "provider", &provider);
                set_str(&mut arguments, "description", &desc);
                set_str(&mut arguments, "priority", &priority);
                ToolCallPayload::new("ticket_create", arguments)
            }
            TicketAction::Update {
                id,
                provider,
                project,
                notes,
                status,
                assignee,
            } => {
                let Some(id) = id else {
                    return Err(CliError::Usage("Error: ticket ID is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                provider_project!(provider, project, arguments);
                set_str(&mut arguments, "notes", &notes);
                set_str(&mut arguments, "status", &status);
                set_str(&mut arguments, "assignee", &assignee);
                ToolCallPayload::new("ticket_update", arguments)
            }
            TicketAction::Comment {
                id,
                comment,
                provider,
                project,
            } => {
                let (Some(id), Some(comment)) = (id, comment) else {
                    return Err(CliError::Usage(
                        "Error: ticket ID and comment are required.\nUsage: ibis-assistant ticket comment <id> \"comment\" [--provider <p>] [--project <k>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                arguments.insert("comment".to_string(), json!(comment));
                provider_project!(provider, project, arguments);
                ToolCallPayload::new("ticket_add_comment", arguments)
            }
            TicketAction::Assign {
                id,
                assignee,
                provider,
                project,
            } => {
                let (Some(id), Some(assignee)) = (id, assignee) else {
                    return Err(CliError::Usage(
                        "Error: ticket ID and assignee are required.\nUsage: ibis-assistant ticket assign <id> <assignee> [--provider <p>] [--project <k>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                arguments.insert("assignee".to_string(), json!(assignee));
                provider_project!(provider, project, arguments);
                ToolCallPayload::new("ticket_assign", arguments)
            }
            TicketAction::Transition {
                id,
                transition,
                provider,
                project,
            } => {
                let (Some(id), Some(transition)) = (id, transition) else {
                    return Err(CliError::Usage(
                        "Error: ticket ID and transition are required.\nUsage: ibis-assistant ticket transition <id> <transition> [--provider <p>] [--project <k>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                arguments.insert("transition".to_string(), json!(transition));
                provider_project!(provider, project, arguments);
                ToolCallPayload::new("ticket_transition", arguments)
            }
            TicketAction::Resolve {
                id,
                provider,
                project,
                notes,
            } => {
                let Some(id) = id else {
                    return Err(CliError::Usage("Error: ticket ID is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                provider_project!(provider, project, arguments);
                set_str(&mut arguments, "notes", &notes);
                ToolCallPayload::new("ticket_mark_resolved", arguments)
            }
            TicketAction::Close {
                id,
                provider,
                project,
                notes,
            } => {
                let Some(id) = id else {
                    return Err(CliError::Usage("Error: ticket ID is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                provider_project!(provider, project, arguments);
                set_str(&mut arguments, "notes", &notes);
                ToolCallPayload::new("ticket_mark_closed", arguments)
            }
            TicketAction::Reopen {
                id,
                provider,
                project,
                notes,
            } => {
                let Some(id) = id else {
                    return Err(CliError::Usage("Error: ticket ID is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                provider_project!(provider, project, arguments);
                set_str(&mut arguments, "notes", &notes);
                ToolCallPayload::new("ticket_reopen", arguments)
            }
            TicketAction::ListStatuses { provider, project } => {
                let mut arguments = Args::new();
                provider_project!(provider, project, arguments);
                ToolCallPayload::new("ticket_list_statuses", arguments)
            }
            TicketAction::ListProjects { provider } => {
                let mut arguments = Args::new();
                set_str(&mut arguments, "provider", &provider);
                ToolCallPayload::new("ticket_list_projects", arguments)
            }
        };
        Ok(payload)
    }
}

impl PrAction {
    fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        match self {
            PrAction::Create {
                provider,
                project,
                url,
                repo,
                source,
                target,
                title,
                desc,
            } => {
                let Some(source) = source else {
                    return Err(CliError::Usage("Error: --source is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("source_branch".to_string(), json!(source));
                set_str(&mut arguments, "provider", &provider);
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "origin_url", &url);
                set_str(&mut arguments, "repository", &repo);
                set_str(&mut arguments, "target_branch", &target);
                set_str(&mut arguments, "title", &title);
                set_str(&mut arguments, "description", &desc);
                Ok(ToolCallPayload::new("repo_pr_create", arguments))
            }
            PrAction::Complete {
                pr_id,
                provider,
                project,
                repo,
                delete_branch,
                squash,
            } => {
                let Some(pr_id) = pr_id else {
                    return Err(CliError::Usage("Error: PR ID is required.".to_string()));
                };
                let mut arguments = Args::new();
                arguments.insert("pr_id".to_string(), json!(pr_id));
                set_str(&mut arguments, "provider", &provider);
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "repository", &repo);
                // Scenario: sends stringified booleans because the server declares
                // WithString args for these and parses with getBoolArg
                //
                if delete_branch {
                    arguments.insert("delete_source_branch".to_string(), json!("true"));
                }
                if squash {
                    arguments.insert("squash".to_string(), json!("true"));
                }
                Ok(ToolCallPayload::new("repo_pr_complete", arguments))
            }
        }
    }
}

impl LogsAction {
    fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        match self {
            LogsAction::Analyze {
                project,
                file,
                text,
            } => {
                let (project, text) = (project.unwrap_or_default(), text.unwrap_or_default());
                if project.is_empty() || text.is_empty() {
                    return Err(CliError::Usage(
                        "Error: --project and --text are required.".to_string(),
                    ));
                }
                let mut arguments = Args::new();
                arguments.insert("project_name".to_string(), json!(project));
                arguments.insert("log_text".to_string(), json!(text));
                set_str(&mut arguments, "log_file", &file);
                Ok(ToolCallPayload::new("analyze_logs", arguments))
            }
        }
    }
}

impl CredentialsAction {
    fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        match self {
            CredentialsAction::Add {
                target,
                provider,
                auth_type,
                token,
                username,
                ssh_key,
            } => {
                let target = target.unwrap_or_default();
                if target.is_empty() {
                    return Err(CliError::Usage("Error: --target is required.".to_string()));
                }
                // Scenario: unconditionally required --token , which
                // made ssh credentials impossible to configure — the server
                // wants ssh_private_key for ssh auth instead
                // . Validation now matches the
                // server per auth type (L-A01).
                let token_value = token.as_deref().unwrap_or_default();
                let ssh_key_value = ssh_key.as_deref().unwrap_or_default();
                match auth_type.as_str() {
                    "token" | "basic" => {
                        if token_value.is_empty() {
                            return Err(CliError::Usage("Error: --token is required.".to_string()));
                        }
                    }
                    "ssh" if ssh_key_value.is_empty() => {
                        return Err(CliError::Usage(
                            "Error: --ssh-key is required for --auth-type ssh.".to_string(),
                        ));
                    }
                    // Unknown auth types pass through; the server validates
                    // ("Invalid auth_type, must be: token, basic, or ssh").
                    _ => {}
                }
                let mut arguments = Args::new();
                arguments.insert("target".to_string(), json!(target));
                arguments.insert("provider".to_string(), json!(provider));
                arguments.insert("auth_type".to_string(), json!(auth_type));
                // the literal map always carries the token key.
                arguments.insert("token".to_string(), json!(token.unwrap_or_default()));
                set_str(&mut arguments, "username", &username);
                set_str(&mut arguments, "ssh_private_key", &ssh_key);
                Ok(ToolCallPayload::new("git_configure_credentials", arguments))
            }
        }
    }
}

impl MemoryAction {
    fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        match self {
            MemoryAction::Add {
                project,
                text,
                kind,
                title,
                tags,
                visibility,
                confidence,
            } => {
                let (project, text) = (project.unwrap_or_default(), text.unwrap_or_default());
                if project.is_empty() || text.is_empty() {
                    return Err(CliError::Usage(
                        "Error: --project and --text are required.".to_string(),
                    ));
                }
                // Scenario: detects explicitly-set flags via fs.Visit: any typed-only
                // flag (even with an empty value or --confidence 0) routes to
                // memory_add, otherwise the legacy back-compat tool is used
                //
                let typed = kind.is_some()
                    || title.is_some()
                    || tags.is_some()
                    || visibility.is_some()
                    || confidence.is_some();
                if !typed {
                    let mut arguments = Args::new();
                    arguments.insert("project_name".to_string(), json!(project));
                    arguments.insert("memory_text".to_string(), json!(text));
                    return Ok(ToolCallPayload::new(
                        "provide_collaborative_memory",
                        arguments,
                    ));
                }
                let kind_value = match kind {
                    Some(k) if !k.is_empty() => k,
                    _ => "fact".to_string(),
                };
                let mut arguments = Args::new();
                arguments.insert("project_name".to_string(), json!(project));
                arguments.insert("content".to_string(), json!(text));
                arguments.insert("kind".to_string(), json!(kind_value));
                set_str(&mut arguments, "title", &title);
                set_str(&mut arguments, "tags", &tags);
                set_str(&mut arguments, "visibility", &visibility);
                if let Some(confidence) = confidence {
                    arguments.insert("confidence".to_string(), json!(confidence));
                }
                Ok(ToolCallPayload::new("memory_add", arguments))
            }
            MemoryAction::Search {
                query,
                project,
                kind,
                status,
                visibility,
                tags,
                limit,
            } => {
                let Some(query) = query else {
                    return Err(CliError::Usage(
                        "Error: provide a search query.\nUsage: ibis-assistant memory search \"query text\" [--project <p>] [--kind <k>] [--status <s>] [--visibility <v>] [--tags <t>] [--limit <n>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("query".to_string(), json!(query));
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "kind", &kind);
                set_str(&mut arguments, "status", &status);
                set_str(&mut arguments, "visibility", &visibility);
                set_str(&mut arguments, "tags", &tags);
                if limit > 0 {
                    set_num(&mut arguments, "limit", limit);
                }
                Ok(ToolCallPayload::new("memory_search", arguments))
            }
            MemoryAction::Expand { id, depth } => {
                let Some(id) = id else {
                    return Err(CliError::Usage(
                        "Error: memory ID is required.\nUsage: ibis-assistant memory expand <id> [--depth <n>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                if depth > 0 {
                    set_num(&mut arguments, "depth", depth);
                }
                Ok(ToolCallPayload::new("memory_expand", arguments))
            }
            MemoryAction::Update {
                id,
                content,
                title,
                kind,
                status,
                tags,
                visibility,
            } => {
                let Some(id) = id else {
                    return Err(CliError::Usage(
                        "Error: memory ID is required.\nUsage: ibis-assistant memory update <id> [--content <c>] [--title <t>] [--status <s>] ...".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                set_str(&mut arguments, "content", &content);
                set_str(&mut arguments, "title", &title);
                set_str(&mut arguments, "kind", &kind);
                set_str(&mut arguments, "status", &status);
                set_str(&mut arguments, "tags", &tags);
                set_str(&mut arguments, "visibility", &visibility);
                Ok(ToolCallPayload::new("memory_update", arguments))
            }
            MemoryAction::Delete { id, hard } => {
                let Some(id) = id else {
                    return Err(CliError::Usage(
                        "Error: memory ID is required.\nUsage: ibis-assistant memory delete <id> [--hard]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                if hard {
                    arguments.insert("hard".to_string(), json!(true));
                }
                Ok(ToolCallPayload::new("memory_delete", arguments))
            }
            MemoryAction::History { id, depth } => {
                let Some(id) = id else {
                    return Err(CliError::Usage(
                        "Error: memory ID or subject is required.\nUsage: ibis-assistant memory history <id-or-subject> [--depth <n>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                if depth > 0 {
                    set_num(&mut arguments, "depth", depth);
                }
                Ok(ToolCallPayload::new("memory_history", arguments))
            }
            MemoryAction::Rule {
                project,
                title,
                content,
                tags,
                branch,
            } => {
                let (title, content) = (title.unwrap_or_default(), content.unwrap_or_default());
                if title.is_empty() || content.is_empty() {
                    return Err(CliError::Usage(
                        "Error: --title and --content are required.".to_string(),
                    ));
                }
                let mut arguments = Args::new();
                arguments.insert("title".to_string(), json!(title));
                arguments.insert("content".to_string(), json!(content));
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "tags", &tags);
                set_str(&mut arguments, "branch", &branch);
                Ok(ToolCallPayload::new("store_rule", arguments))
            }
            MemoryAction::Rules {
                project,
                query,
                tags,
                limit,
            } => {
                let mut arguments = Args::new();
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "query", &query);
                set_str(&mut arguments, "tags", &tags);
                if limit > 0 {
                    set_num(&mut arguments, "limit", limit);
                }
                Ok(ToolCallPayload::new("get_rules", arguments))
            }
            MemoryAction::State { action } => match action {
                StateAction::Set {
                    project,
                    branch,
                    content,
                } => {
                    let content = content.unwrap_or_default();
                    if content.is_empty() {
                        return Err(CliError::Usage("Error: --content is required.".to_string()));
                    }
                    let mut arguments = Args::new();
                    arguments.insert("content".to_string(), json!(content));
                    set_str(&mut arguments, "project_name", &project);
                    set_str(&mut arguments, "branch", &branch);
                    Ok(ToolCallPayload::new("set_working_state", arguments))
                }
                StateAction::Get { project, branch } => {
                    let mut arguments = Args::new();
                    set_str(&mut arguments, "project_name", &project);
                    set_str(&mut arguments, "branch", &branch);
                    Ok(ToolCallPayload::new("get_working_state", arguments))
                }
            },
            MemoryAction::Failures {
                project,
                kind,
                since,
                limit,
            } => {
                let mut arguments = Args::new();
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "kind", &kind);
                set_str(&mut arguments, "since", &since);
                if limit > 0 {
                    set_num(&mut arguments, "limit", limit);
                }
                Ok(ToolCallPayload::new("recent_failures", arguments))
            }
            MemoryAction::Why {
                project,
                file,
                symbol,
                issue,
                limit,
            } => {
                if file.as_deref().unwrap_or_default().is_empty()
                    && symbol.as_deref().unwrap_or_default().is_empty()
                    && issue.as_deref().unwrap_or_default().is_empty()
                {
                    return Err(CliError::Usage(
                        "Error: one of --file, --symbol, or --issue is required.".to_string(),
                    ));
                }
                let mut arguments = Args::new();
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "file_path", &file);
                set_str(&mut arguments, "symbol_name", &symbol);
                set_str(&mut arguments, "issue_id", &issue);
                if limit > 0 {
                    set_num(&mut arguments, "limit", limit);
                }
                Ok(ToolCallPayload::new("why_changed", arguments))
            }
            MemoryAction::Session { project, branch } => {
                let mut arguments = Args::new();
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "branch", &branch);
                Ok(ToolCallPayload::new("load_session_context", arguments))
            }
            MemoryAction::Capture {
                text,
                project,
                source,
                session_id,
            } => {
                let Some(text) = text else {
                    return Err(CliError::Usage(
                        "Error: provide the observation text.\nUsage: ibis-assistant memory capture \"observation text\" [--project <p>] [--source <s>] [--session-id <id>]".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("text".to_string(), json!(text));
                set_str(&mut arguments, "project_name", &project);
                set_str(&mut arguments, "source", &source);
                set_str(&mut arguments, "session_id", &session_id);
                Ok(ToolCallPayload::new("capture_observation", arguments))
            }
            MemoryAction::Share { id, visibility } => {
                let (Some(id), Some(visibility)) = (id, visibility) else {
                    return Err(CliError::Usage(
                        "Error: memory ID and visibility are required.\nUsage: ibis-assistant memory share <id> <visibility>".to_string(),
                    ));
                };
                let mut arguments = Args::new();
                arguments.insert("id".to_string(), json!(id));
                arguments.insert("visibility".to_string(), json!(visibility));
                Ok(ToolCallPayload::new("memory_share", arguments))
            }
            MemoryAction::BootstrapGitStore { project, dry_run } => {
                // Scenario: always sends dry_run (bool), even false
                let mut arguments = Args::new();
                arguments.insert("dry_run".to_string(), json!(dry_run));
                set_str(&mut arguments, "project_name", &project);
                Ok(ToolCallPayload::new(
                    "memory_bootstrap_git_store",
                    arguments,
                ))
            }
            MemoryAction::RebuildGitNotes { project, dry_run } => {
                let mut arguments = Args::new();
                arguments.insert("dry_run".to_string(), json!(dry_run));
                set_str(&mut arguments, "project_name", &project);
                Ok(ToolCallPayload::new("memory_rebuild_git_notes", arguments))
            }
        }
    }
}

impl OutcomeAction {
    fn tool_call(self) -> Result<ToolCallPayload, CliError> {
        match self {
            OutcomeAction::Add {
                project,
                question,
                outcome,
                sources,
            } => {
                let (project, question, outcome) = (
                    project.unwrap_or_default(),
                    question.unwrap_or_default(),
                    outcome.unwrap_or_default(),
                );
                if project.is_empty() || question.is_empty() || outcome.is_empty() {
                    return Err(CliError::Usage(
                        "Error: --project, --question, and --outcome are required.".to_string(),
                    ));
                }
                let mut arguments = Args::new();
                arguments.insert("project_name".to_string(), json!(project));
                arguments.insert("question".to_string(), json!(question));
                arguments.insert("outcome_text".to_string(), json!(outcome));
                set_str(&mut arguments, "useful_sources", &sources);
                Ok(ToolCallPayload::new("save_reasoning_outcome", arguments))
            }
        }
    }
}

/// Dispatch terminal subcommands to MCP tools.
///
/// `args` are the arguments **after the program name** ('s
/// `cli.ExecuteCLI(os.Args[1:])`); the clap parser
/// is fed a synthetic argv[0].
///
/// - Empty args print the CLI help and return `Ok()` (`PrintCLIHelp,
///   exit 0).
/// - clap parse failures (unknown flag/subcommand, `--help`) print to stderr
///   and exit the process with code 2, mirroring the `flag.ExitOnError`.
/// - Validation failures return [`CliError::Usage`] with the message text.
/// - Otherwise the built payload is handed to `caller` (the swappable
///   `callServerTool` var).
pub fn execute_cli<I, T, F>(args: I, caller: F) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    F: FnOnce(ToolCallPayload),
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.is_empty() {
        let _ = Cli::command().print_help();
        return Ok(());
    }
    // clap exits the process on parse errors/help like the flag.ExitOnError.
    let cli = Cli::parse_from(std::iter::once(OsString::from("ibis-assistant")).chain(args));
    let payload = cli.command.tool_call()?;
    caller(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Run the dispatcher with a capturing caller (the swapped
    /// `callServerTool` test double).
    fn dispatch(args: &[&str]) -> Result<ToolCallPayload, CliError> {
        let captured: RefCell<Option<ToolCallPayload>> = RefCell::new(None);
        let result = execute_cli(args.iter().copied(), |payload| {
            *captured.borrow_mut() = Some(payload);
        });
        result.map(|()| captured.into_inner().expect("caller ran"))
    }

    fn payload_of(args: &[&str]) -> ToolCallPayload {
        dispatch(args).expect("dispatch ok")
    }

    fn arg<'a>(payload: &'a ToolCallPayload, key: &str) -> &'a Value {
        payload
            .arguments
            .get(key)
            .unwrap_or_else(|| panic!("missing argument key {key}"))
    }

    fn expect_str(payload: &ToolCallPayload, key: &str, want: &str) {
        assert_eq!(arg(payload, key), &json!(want), "key {key}");
    }

    fn expect_num(payload: &ToolCallPayload, key: &str, want: f64) {
        assert_eq!(arg(payload, key), &json!(want), "key {key}");
    }

    /// TestExecuteCLI_Subcommands — the full table
    #[test]
    fn execute_cli_subcommands() {
        // ask subcommand
        let p = payload_of(&["ask", "--branch", "main", "How to query DB?"]);
        assert_eq!(p.name, "ask_project");
        expect_str(&p, "query", "How to query DB?");
        expect_str(&p, "branch_or_commit", "main");

        // ingest code subcommand
        let p = payload_of(&["ingest", "code", "--path", "/src"]);
        assert_eq!(p.name, "ingest_code");
        expect_str(&p, "path", "/src");

        // ingest git subcommand
        let p = payload_of(&[
            "ingest",
            "git",
            "--name",
            "MyProj",
            "--url",
            "http://github.com",
            "--branch",
            "develop",
            "--commit",
            "abc",
        ]);
        assert_eq!(p.name, "init_project");
        expect_str(&p, "project_name", "MyProj");
        expect_str(&p, "origin_url", "http://github.com");
        expect_str(&p, "branch", "develop");
        expect_str(&p, "commit", "abc");

        // ticket search subcommand
        let p = payload_of(&[
            "ticket",
            "search",
            "--provider",
            "jira",
            "--query",
            "bug",
            "--project",
            "PRJ",
            "--status",
            "open",
            "--limit",
            "10",
        ]);
        assert_eq!(p.name, "ticket_search");
        expect_str(&p, "provider", "jira");
        expect_str(&p, "query", "bug");
        expect_str(&p, "project_key", "PRJ");
        expect_str(&p, "status", "open");
        expect_num(&p, "limit", 10.0);

        // pr create subcommand
        let p = payload_of(&[
            "pr",
            "create",
            "--provider",
            "azure_devops",
            "--source",
            "feat/x",
            "--target",
            "main",
            "--title",
            "Add X",
        ]);
        assert_eq!(p.name, "repo_pr_create");
        expect_str(&p, "provider", "azure_devops");
        expect_str(&p, "source_branch", "feat/x");
        expect_str(&p, "target_branch", "main");
        expect_str(&p, "title", "Add X");

        // logs analyze subcommand
        let p = payload_of(&[
            "logs",
            "analyze",
            "--project",
            "App",
            "--text",
            "Error occurred",
        ]);
        assert_eq!(p.name, "analyze_logs");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "log_text", "Error occurred");

        // credentials add subcommand
        let p = payload_of(&[
            "credentials",
            "add",
            "--target",
            "github.com",
            "--token",
            "secret",
        ]);
        assert_eq!(p.name, "git_configure_credentials");
        expect_str(&p, "target", "github.com");
        expect_str(&p, "token", "secret");
        expect_str(&p, "provider", "generic");
        expect_str(&p, "auth_type", "token");

        // memory add subcommand (legacy tool)
        let p = payload_of(&[
            "memory",
            "add",
            "--project",
            "App",
            "--text",
            "Use Postgres 16",
        ]);
        assert_eq!(p.name, "provide_collaborative_memory");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "memory_text", "Use Postgres 16");

        // memory add-typed subcommand (confidence 0 is explicitly set)
        let p = payload_of(&[
            "memory",
            "add",
            "--project",
            "App",
            "--text",
            "Use typed memory",
            "--kind",
            "decision",
            "--title",
            "Decision",
            "--tags",
            "a,b",
            "--confidence",
            "0",
        ]);
        assert_eq!(p.name, "memory_add");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "content", "Use typed memory");
        expect_str(&p, "kind", "decision");
        expect_str(&p, "title", "Decision");
        expect_str(&p, "tags", "a,b");
        expect_num(&p, "confidence", 0.0);

        // memory add visibility without kind uses typed tool
        let p = payload_of(&[
            "memory",
            "add",
            "--project",
            "App",
            "--text",
            "Private note",
            "--visibility",
            "personal",
        ]);
        assert_eq!(p.name, "memory_add");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "content", "Private note");
        expect_str(&p, "kind", "fact");
        expect_str(&p, "visibility", "personal");

        // memory search subcommand
        let p = payload_of(&[
            "memory",
            "search",
            "--project",
            "App",
            "--kind",
            "rule",
            "query text",
        ]);
        assert_eq!(p.name, "memory_search");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "kind", "rule");
        expect_str(&p, "query", "query text");

        // memory search trailing flags after query
        let p = payload_of(&[
            "memory",
            "search",
            "query text",
            "--project",
            "App",
            "--kind",
            "rule",
        ]);
        assert_eq!(p.name, "memory_search");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "kind", "rule");
        expect_str(&p, "query", "query text");

        // memory expand subcommand
        let p = payload_of(&["memory", "expand", "memory:one", "--depth", "2"]);
        assert_eq!(p.name, "memory_expand");
        expect_str(&p, "id", "memory:one");
        expect_num(&p, "depth", 2.0);

        // memory update subcommand
        let p = payload_of(&["memory", "update", "memory:one", "--status", "archived"]);
        assert_eq!(p.name, "memory_update");
        expect_str(&p, "id", "memory:one");
        expect_str(&p, "status", "archived");

        // memory delete subcommand
        let p = payload_of(&["memory", "delete", "memory:one", "--hard"]);
        assert_eq!(p.name, "memory_delete");
        expect_str(&p, "id", "memory:one");
        assert_eq!(arg(&p, "hard"), &json!(true));

        // memory history subcommand
        let p = payload_of(&["memory", "history", "memory:one", "--depth", "3"]);
        assert_eq!(p.name, "memory_history");
        expect_str(&p, "id", "memory:one");
        expect_num(&p, "depth", 3.0);

        // memory rule subcommand
        let p = payload_of(&[
            "memory",
            "rule",
            "--project",
            "App",
            "--title",
            "Title",
            "--content",
            "Body",
        ]);
        assert_eq!(p.name, "store_rule");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "title", "Title");
        expect_str(&p, "content", "Body");

        // memory rules subcommand
        let p = payload_of(&["memory", "rules", "--project", "App"]);
        assert_eq!(p.name, "get_rules");
        expect_str(&p, "project_name", "App");

        // memory state set subcommand
        let p = payload_of(&[
            "memory",
            "state",
            "set",
            "--project",
            "App",
            "--content",
            "Working on X",
        ]);
        assert_eq!(p.name, "set_working_state");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "content", "Working on X");

        // memory state get subcommand
        let p = payload_of(&["memory", "state", "get", "--project", "App"]);
        assert_eq!(p.name, "get_working_state");
        expect_str(&p, "project_name", "App");

        // memory failures subcommand
        let p = payload_of(&["memory", "failures", "--project", "App", "--since", "7d"]);
        assert_eq!(p.name, "recent_failures");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "since", "7d");

        // memory why subcommand
        let p = payload_of(&["memory", "why", "--file", "main.go"]);
        assert_eq!(p.name, "why_changed");
        expect_str(&p, "file_path", "main.go");

        // memory session subcommand
        let p = payload_of(&["memory", "session", "--project", "App"]);
        assert_eq!(p.name, "load_session_context");
        expect_str(&p, "project_name", "App");

        // memory capture subcommand
        let p = payload_of(&[
            "memory",
            "capture",
            "--project",
            "App",
            "The wizard crashes on empty input.",
        ]);
        assert_eq!(p.name, "capture_observation");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "text", "The wizard crashes on empty input.");

        // memory capture trailing flags after text
        let p = payload_of(&[
            "memory",
            "capture",
            "The wizard crashes on empty input.",
            "--project",
            "App",
            "--source",
            "user",
        ]);
        assert_eq!(p.name, "capture_observation");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "text", "The wizard crashes on empty input.");
        expect_str(&p, "source", "user");

        // memory share subcommand
        let p = payload_of(&["memory", "share", "memory:one", "team"]);
        assert_eq!(p.name, "memory_share");
        expect_str(&p, "id", "memory:one");
        expect_str(&p, "visibility", "team");

        // outcome add subcommand
        let p = payload_of(&[
            "outcome",
            "add",
            "--project",
            "App",
            "--question",
            "Why?",
            "--outcome",
            "Because",
            "--sources",
            "doc1",
        ]);
        assert_eq!(p.name, "save_reasoning_outcome");
        expect_str(&p, "project_name", "App");
        expect_str(&p, "question", "Why?");
        expect_str(&p, "outcome_text", "Because");
        expect_str(&p, "useful_sources", "doc1");

        // optimize subcommand
        let p = payload_of(&["optimize", "--iterations", "5"]);
        assert_eq!(p.name, "optimize_knowledge");
        expect_num(&p, "iterations", 5.0);
    }

    /// ticket actions not covered by the original table, pinned for arg mapping.
    #[test]
    fn ticket_remaining_actions() {
        let p = payload_of(&[
            "ticket",
            "my-issues",
            "--provider",
            "redmine",
            "--project",
            "PRJ",
        ]);
        assert_eq!(p.name, "ticket_search_my");
        expect_str(&p, "provider", "redmine");
        expect_str(&p, "project_key", "PRJ");

        let p = payload_of(&[
            "ticket",
            "get",
            "123",
            "--provider",
            "jira",
            "--project",
            "PRJ",
        ]);
        assert_eq!(p.name, "ticket_get");
        expect_str(&p, "id", "123");

        // Flags may precede the positional ID (L-A04): 's
        // `subArgs[0]` handling mis-parsed this as id="--provider".
        let p = payload_of(&["ticket", "get", "--provider", "jira", "123"]);
        assert_eq!(p.name, "ticket_get");
        expect_str(&p, "id", "123");
        expect_str(&p, "provider", "jira");

        let p = payload_of(&[
            "ticket",
            "create",
            "--provider",
            "redmine",
            "--project",
            "PRJ",
            "--title",
            "T",
            "--desc",
            "D",
            "--priority",
            "High",
        ]);
        assert_eq!(p.name, "ticket_create");
        expect_str(&p, "project_key", "PRJ");
        expect_str(&p, "title", "T");
        expect_str(&p, "description", "D");
        expect_str(&p, "priority", "High");

        let p = payload_of(&[
            "ticket",
            "update",
            "9",
            "--notes",
            "n",
            "--status",
            "open",
            "--assignee",
            "me",
        ]);
        assert_eq!(p.name, "ticket_update");
        expect_str(&p, "id", "9");
        expect_str(&p, "notes", "n");
        expect_str(&p, "status", "open");
        expect_str(&p, "assignee", "me");

        let p = payload_of(&["ticket", "comment", "7", "hello", "--provider", "jira"]);
        assert_eq!(p.name, "ticket_add_comment");
        expect_str(&p, "id", "7");
        expect_str(&p, "comment", "hello");
        expect_str(&p, "provider", "jira");

        let p = payload_of(&["ticket", "assign", "7", "me"]);
        assert_eq!(p.name, "ticket_assign");
        expect_str(&p, "id", "7");
        expect_str(&p, "assignee", "me");

        let p = payload_of(&["ticket", "transition", "7", "resolve"]);
        assert_eq!(p.name, "ticket_transition");
        expect_str(&p, "id", "7");
        expect_str(&p, "transition", "resolve");

        for (action, tool) in [
            ("resolve", "ticket_mark_resolved"),
            ("close", "ticket_mark_closed"),
            ("reopen", "ticket_reopen"),
        ] {
            let p = payload_of(&["ticket", action, "7", "--notes", "done"]);
            assert_eq!(p.name, tool);
            expect_str(&p, "id", "7");
            expect_str(&p, "notes", "done");
        }

        let p = payload_of(&["ticket", "list-statuses", "--project", "PRJ"]);
        assert_eq!(p.name, "ticket_list_statuses");
        expect_str(&p, "project_key", "PRJ");

        let p = payload_of(&["ticket", "list-projects", "--provider", "redmine"]);
        assert_eq!(p.name, "ticket_list_projects");
        expect_str(&p, "provider", "redmine");
    }

    /// Scenario: always sent `limit` for ticket search, even untouched (default 20).
    #[test]
    fn ticket_search_limit_default_is_always_sent() {
        let p = payload_of(&["ticket", "search"]);
        expect_num(&p, "limit", 20.0);
    }

    /// pr complete sends stringified booleans (server declares string args).
    #[test]
    fn pr_complete_flags_are_string_true() {
        let p = payload_of(&[
            "pr",
            "complete",
            "42",
            "--provider",
            "azure_devops",
            "--project",
            "P",
            "--repo",
            "R",
            "--delete-branch",
            "--squash",
        ]);
        assert_eq!(p.name, "repo_pr_complete");
        expect_str(&p, "pr_id", "42");
        expect_str(&p, "provider", "azure_devops");
        expect_str(&p, "project_name", "P");
        expect_str(&p, "repository", "R");
        assert_eq!(arg(&p, "delete_source_branch"), &json!("true"));
        assert_eq!(arg(&p, "squash"), &json!("true"));
    }

    /// logs analyze --file maps to log_file.
    #[test]
    fn logs_analyze_file_arg() {
        let p = payload_of(&[
            "logs",
            "analyze",
            "--project",
            "App",
            "--file",
            "uploaded.log",
            "--text",
            "boom",
        ]);
        expect_str(&p, "log_file", "uploaded.log");
    }

    /// L-A01: ssh auth requires --ssh-key, not a dummy --token.
    #[test]
    fn credentials_add_ssh_uses_ssh_key() {
        let p = payload_of(&[
            "credentials",
            "add",
            "--target",
            "git.internal",
            "--auth-type",
            "ssh",
            "--ssh-key",
            "KEYDATA",
        ]);
        assert_eq!(p.name, "git_configure_credentials");
        expect_str(&p, "target", "git.internal");
        expect_str(&p, "provider", "generic");
        expect_str(&p, "auth_type", "ssh");
        expect_str(&p, "ssh_private_key", "KEYDATA");

        // username and explicit provider map through
        let p = payload_of(&[
            "credentials",
            "add",
            "--target",
            "github.com",
            "--provider",
            "github",
            "--auth-type",
            "basic",
            "--token",
            "pw",
            "--username",
            "me",
        ]);
        expect_str(&p, "provider", "github");
        expect_str(&p, "username", "me");

        // token/basic auth still requires --token
        let err = dispatch(&["credentials", "add", "--target", "github.com"]).unwrap_err();
        assert!(matches!(err, CliError::Usage(ref m) if m.contains("--token")));

        // ssh auth requires --ssh-key
        let err = dispatch(&[
            "credentials",
            "add",
            "--target",
            "git.internal",
            "--auth-type",
            "ssh",
        ])
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(ref m) if m.contains("--ssh-key")));
    }

    /// the exact validation messages.
    #[test]
    fn validation_messages_match_reference() {
        let cases: &[(&[&str], &str)] = &[
            (&["ask"], "Error: provide a question."),
            (
                &["ingest", "git", "--name", "n", "--url", "u"],
                "Error: --name, --url, and --branch are required for Git ingest.",
            ),
            (
                &["ticket", "create", "--project", "P"],
                "Error: --project and --title are required.",
            ),
            (
                &["logs", "analyze", "--project", "App"],
                "Error: --project and --text are required.",
            ),
            (
                &["memory", "add", "--project", "App"],
                "Error: --project and --text are required.",
            ),
            (
                &["memory", "rule", "--title", "T"],
                "Error: --title and --content are required.",
            ),
            (
                &["memory", "state", "set", "--project", "App"],
                "Error: --content is required.",
            ),
            (
                &["memory", "why", "--file", ""],
                "Error: one of --file, --symbol, or --issue is required.",
            ),
            (
                &["outcome", "add", "--project", "App", "--question", "Q"],
                "Error: --project, --question, and --outcome are required.",
            ),
        ];
        for (args, want) in cases {
            let err = dispatch(args).expect_err("must fail");
            match err {
                CliError::Usage(message) => {
                    assert!(
                        message.starts_with(want),
                        "args {args:?}: message {message:?} should start with {want:?}"
                    );
                }
                other => panic!("args {args:?}: unexpected error {other:?}"),
            }
        }
    }

    /// Positionals required by the length checks.
    #[test]
    fn missing_positionals_fail_with_reference_messages() {
        for args in [
            &["ticket", "get"][..],
            &["ticket", "comment", "7"][..],
            &["pr", "complete"][..],
            &["memory", "expand"][..],
            &["memory", "update"][..],
            &["memory", "delete"][..],
            &["memory", "history"][..],
            &["memory", "share", "id"][..],
        ] {
            assert!(dispatch(args).is_err(), "expected usage error for {args:?}");
        }
        // empty-string ID is still "present" like the len(subArgs) check
        let p = payload_of(&["memory", "delete", ""]);
        expect_str(&p, "id", "");
    }

    /// `memory why` empty-string subjects behave like the == "" checks.
    #[test]
    fn memory_why_empty_strings_are_absent() {
        let err = dispatch(&["memory", "why", "--file", "", "--symbol", ""]).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
        let p = payload_of(&["memory", "why", "--symbol", "S", "--limit", "3"]);
        expect_str(&p, "symbol_name", "S");
        expect_num(&p, "limit", 3.0);
    }

    /// Trailing `--branch` after the positional question now parses
    /// (L-A05: the flag package silently dropped it).
    #[test]
    fn ask_trailing_branch_parses() {
        let p = payload_of(&["ask", "How to query DB?", "--branch", "main"]);
        expect_str(&p, "query", "How to query DB?");
        expect_str(&p, "branch_or_commit", "main");
    }

    /// dry_run is always sent as a bool, even false
    #[test]
    fn git_store_tools_send_dry_run_bool() {
        let p = payload_of(&["memory", "bootstrap-git-store"]);
        assert_eq!(arg(&p, "dry_run"), &json!(false));
        let p = payload_of(&[
            "memory",
            "bootstrap-git-store",
            "--project",
            "App",
            "--dry-run",
        ]);
        assert_eq!(arg(&p, "dry_run"), &json!(true));
        expect_str(&p, "project_name", "App");
        let p = payload_of(&["memory", "rebuild-git-notes", "--dry-run"]);
        assert_eq!(arg(&p, "dry_run"), &json!(true));
    }

    /// memory update/delete untouched optional flags stay absent.
    #[test]
    fn memory_update_untouched_flags_absent() {
        let p = payload_of(&["memory", "update", "m:1"]);
        assert_eq!(p.name, "memory_update");
        assert!(p.arguments.len() == 1, "only id expected: {p:?}");
        let p = payload_of(&["memory", "delete", "m:1"]);
        assert_eq!(p.arguments.len(), 1, "only id expected: {p:?}");
        let p = payload_of(&["memory", "expand", "m:1"]);
        assert_eq!(p.arguments.len(), 1, "only id expected: {p:?}");
        let p = payload_of(&["memory", "search", "q"]);
        assert_eq!(p.arguments.len(), 1, "only query expected: {p:?}");
    }
}
