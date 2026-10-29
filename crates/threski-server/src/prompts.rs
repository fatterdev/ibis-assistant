//! F0.3 MCP prompts — `prompts/list`, `prompts/get` and argument
//! completions (`completion/complete`).
//!
//! Five prompts surface the session contract as one-shot intents; VS Code
//! renders them as slash commands prefixed with the server name
//! (`/threski.readiness`, `/threski.status`, `/threski.why_changed`,
//! `/threski.blast_radius`, `/threski.remember` — docs/planning/naming-map.md).
//! The texts are derived from the session contract (the `threski-usage`
//! skill and its siblings): each prompt instructs exactly which Threski
//! tools to call, with the hygiene rules attached.
//!
//! Data-driven like the tool registry ([`crate::registry`]): this table IS
//! the prompt surface; `tests/prompts_protocol_test.rs` verifies it over the
//! wire (list/get/complete) and the completeness test pins the set.

use std::collections::HashMap;

use rmcp::model::{
    CompleteRequestParams, CompleteResult, CompletionInfo, JsonObject, Prompt, PromptArgument,
    Reference,
};

use crate::state::AppState;

/// One prompt argument (spec `PromptArgument`) plus its completion values.
pub struct PromptArgSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub required: bool,
    /// Static completion values (prefix-filtered). Empty for free-form
    /// arguments; `project_name` additionally completes from the configured
    /// projects at request time.
    pub completions: &'static [&'static str],
}

/// One row of the prompt table.
pub struct PromptSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub arguments: &'static [PromptArgSpec],
    /// Renders the single user message from the argument values.
    pub render: fn(&ArgValues<'_>) -> String,
}

/// The static completion values for memory kinds (the durable narrow set of
/// the memory hygiene rules — `threski-remember` kind guide).
pub const MEMORY_KINDS: &[&str] = &[
    "decision",
    "fix",
    "rule",
    "constraint",
    "gotcha",
    "design",
    "fact",
];

/// The static completion values for memory visibility.
pub const MEMORY_VISIBILITY: &[&str] = &["team", "personal"];

/// The authoritative 5-prompt table.
pub static PROMPT_SPECS: &[PromptSpec] = &[
    PromptSpec {
        name: "readiness",
        title: "Threski Readiness",
        description: "First-use and empty-window readiness checklist for Threski: probe the MCP surface, load session context once, report a structured readiness table with one concrete next step.",
        arguments: &[PromptArgSpec {
            name: "project_name",
            title: "Project",
            description: "Project/repo scope (optional; defaults to the workspace project)",
            required: false,
            completions: &[],
        }],
        render: render_readiness,
    },
    PromptSpec {
        name: "status",
        title: "Threski Status",
        description: "Report the Threski runtime status (backend, AI reasoning provider, ticketing, workspace binding) by calling settings_get once.",
        arguments: &[PromptArgSpec {
            name: "project_name",
            title: "Project",
            description: "Project/repo scope (optional; defaults to the workspace project)",
            required: false,
            completions: &[],
        }],
        render: render_status,
    },
    PromptSpec {
        name: "why_changed",
        title: "Why Changed",
        description: "Change forensics: explain why a file, symbol or issue changed using memory provenance, the commit/issue graph and commit meaning excerpts.",
        arguments: &[
            PromptArgSpec {
                name: "file_path",
                title: "File path",
                description: "File path subject (exactly one of file_path/symbol_name/issue_id)",
                required: false,
                completions: &[],
            },
            PromptArgSpec {
                name: "symbol_name",
                title: "Symbol",
                description: "Symbol name subject (exactly one of file_path/symbol_name/issue_id)",
                required: false,
                completions: &[],
            },
            PromptArgSpec {
                name: "issue_id",
                title: "Issue",
                description: "Issue/ticket ID subject (exactly one of file_path/symbol_name/issue_id)",
                required: false,
                completions: &[],
            },
        ],
        render: render_why_changed,
    },
    PromptSpec {
        name: "blast_radius",
        title: "Blast Radius",
        description: "Analyze what breaks if a symbol changes: map callers and dependents across the ingested code graph before editing.",
        arguments: &[
            PromptArgSpec {
                name: "symbol_name",
                title: "Symbol",
                description: "Function or symbol to analyze (required)",
                required: true,
                completions: &[],
            },
            PromptArgSpec {
                name: "file_path",
                title: "File path",
                description: "File containing the symbol (required)",
                required: true,
                completions: &[],
            },
            PromptArgSpec {
                name: "repo_name",
                title: "Repository",
                description: "Repository name (optional; defaults to the workspace repository)",
                required: false,
                completions: &[],
            },
        ],
        render: render_blast_radius,
    },
    PromptSpec {
        name: "remember",
        title: "Remember",
        description: "Capture one durable project memory with the correct type using memory_add: kind guide, content hygiene, supersede duplicates.",
        arguments: &[
            PromptArgSpec {
                name: "content",
                title: "Fact",
                description: "The durable fact to store (one fact, actionable by a future agent)",
                required: true,
                completions: &[],
            },
            PromptArgSpec {
                name: "kind",
                title: "Kind",
                description: "Memory kind: decision|fix|rule|constraint|gotcha|design|fact (default: narrowest fit, fact when nothing else fits)",
                required: false,
                completions: MEMORY_KINDS,
            },
            PromptArgSpec {
                name: "visibility",
                title: "Visibility",
                description: "team (default) | personal",
                required: false,
                completions: MEMORY_VISIBILITY,
            },
        ],
        render: render_remember,
    },
];

/// Looks up a prompt spec by name.
#[allow(dead_code)]
pub fn spec_of(name: &str) -> Option<&'static PromptSpec> {
    PROMPT_SPECS.iter().find(|s| s.name == name)
}

/// The `prompts/list` rows: spec table → spec `Prompt` objects.
pub fn prompt_definitions() -> Vec<Prompt> {
    PROMPT_SPECS
        .iter()
        .map(|spec| {
            Prompt::from_raw(
                spec.name,
                Some(spec.description),
                Some(
                    spec.arguments
                        .iter()
                        .map(|a| {
                            PromptArgument::new(a.name)
                                .with_title(a.title)
                                .with_description(a.description)
                                .with_required(a.required)
                        })
                        .collect(),
                ),
            )
            .with_title(spec.title)
        })
        .collect()
}

/// The argument values of one `prompts/get` request: strings only,
/// trimmed; missing/blank arguments read as absent.
pub struct ArgValues<'a> {
    values: Option<&'a JsonObject>,
}

impl<'a> ArgValues<'a> {
    pub fn new(values: Option<&'a JsonObject>) -> Self {
        ArgValues { values }
    }

    /// The trimmed string value of an argument, when non-empty.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .and_then(|v| v.get(name))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The argument value or a default text (used inside the rendered
    /// message for optional arguments).
    pub fn text(&self, name: &str, default: &str) -> String {
        self.get(name)
            .map(str::to_string)
            .unwrap_or_else(|| default.to_string())
    }
}

fn render_readiness(v: &ArgValues<'_>) -> String {
    let project = v.text("project_name", "the workspace project");
    format!(
        "Run the Threski readiness checklist for {project}. Run it only when the \
session context is empty or something fails — in a healthy session \
`load_session_context` was already called once.\n\n\
## Checklist\n\n\
1. Probe the MCP surface: is the Threski MCP server reachable? If spawned, is \
the `threski` binary on PATH? If attached, is `THRESKI_MCP_URL` reachable?\n\
2. Call `load_session_context` once for {project}.\n\
3. If AI answers look degraded, check `settings_get` \u{2192} `ai.reasoning.provider` \
(sampling / lm_bridge / ollama) and report, do not fix silently.\n\
4. If tickets fail, call `ticket_get_capabilities` once — only now, only for \
diagnosis.\n\n\
## Report format\n\n\
End with exactly this table and a single next step:\n\n\
| Check | Status | Detail |\n\
|---|---|---|\n\
| MCP live | ok / fail | spawn cmdline or attach URL |\n\
| Session context | loaded / empty | project name |\n\
| AI reasoning | provider | sampling / lm_bridge / ollama |\n\
| Ticketing | configured / off | capabilities summary |\n\
| Workspace | bound / missing | folder path |\n\n\
Next step: one concrete action (e.g. \"run `threski run`\", \"ingest code with \
`threski ingest code --path <folder>`\", \"ask the user for THRESKI_MCP_URL\")."
    )
}

fn render_status(v: &ArgValues<'_>) -> String {
    let project = v.text("project_name", "the workspace project");
    format!(
        "Report the Threski runtime status for {project}. Call `settings_get` once \
and summarize. Do not call other Threski tools for a plain status check; only \
if diagnosing a failure add a single `ticket_get_capabilities` call.\n\n\
Report, in this order:\n\n\
1. Backend: spawned child (`threski -mode stdio -runtime-mode plugin`) or \
attached personal runtime (URL).\n\
2. AI reasoning: `ai.reasoning.provider` value and whether embeddings run on \
Ollama.\n\
3. Ticketing: configured providers or \"off\".\n\
4. Workspace: bound project name.\n\
5. Anything the user explicitly asked about.\n\n\
Keep it short. End with one actionable line if something is unhealthy; \
otherwise say the runtime is healthy and stop."
    )
}

fn render_why_changed(v: &ArgValues<'_>) -> String {
    let file = v.get("file_path");
    let symbol = v.get("symbol_name");
    let issue = v.get("issue_id");
    let (subject, subject_args) = match (file, symbol, issue) {
        (Some(f), _, _) => (format!("the file `{f}`"), format!("file_path: `{f}`")),
        (_, Some(s), _) => (format!("the symbol `{s}`"), format!("symbol_name: `{s}`")),
        (_, _, Some(i)) => (format!("the issue `{i}`"), format!("issue_id: `{i}`")),
        _ => (
            "the subject the user points at".to_string(),
            "ask the user which of: a file path, a symbol name, or a ticket id".to_string(),
        ),
    };
    format!(
        "Explain why {subject} changed, using Threski change forensics.\n\n\
Call the `why_changed` tool with exactly one subject — {subject_args}. It \
combines memory provenance, the commit/issue graph and commit meaning \
excerpts (message + patch); prefer it for why/when questions over code, \
symbols and tickets. For open-ended questions over the current code and \
memories, prefer `ask_project` instead.\n\n\
Present: what changed, when, and why (memory + commit meaning), with the \
source records. If `why_changed` returns nothing, say so and fall back to \
`ask_project`. Stay read-only: no confirmation needed."
    )
}

fn render_blast_radius(v: &ArgValues<'_>) -> String {
    let symbol = v.text(
        "symbol_name",
        "the symbol the user points at — ask which one",
    );
    let file = v.text("file_path", "the file containing it");
    let repo = v.text("repo_name", "the workspace repository");
    format!(
        "Analyze what breaks if the symbol `{symbol}` in `{file}` ({repo}) changes.\n\n\
Call the `analyze_blast_radius` tool with repo_name: `{repo}`, file_path: \
`{file}` and symbol_name: `{symbol}`. It maps the callers and dependents of \
the symbol across the ingested code graph.\n\n\
Present: the blast radius (direct callers and impacted paths), the riskiest \
impacts first, and — if the area touches known decisions or gotchas — one \
`memory_search` call to surface them. Stay read-only: no confirmation needed."
    )
}

fn render_remember(v: &ArgValues<'_>) -> String {
    let content = v.text("content", "the fact the user asked to remember");
    let kind = v.text("kind", "the narrowest fit (fact when nothing else fits)");
    let visibility = v.text("visibility", "team");
    let mut kinds = String::new();
    for (name, use_when) in [
        (
            "decision",
            "A choice the team should keep (approach, trade-off, direction)",
        ),
        (
            "fix",
            "What actually resolved a failure (root cause + remedy)",
        ),
        ("rule", "A durable convention the project follows"),
        (
            "constraint",
            "A hard boundary the project must respect (versions, licenses, SLAs)",
        ),
        (
            "gotcha",
            "A trap future agents should avoid (non-obvious failure mode)",
        ),
        (
            "design",
            "Design rationale worth keeping (why the architecture is so)",
        ),
        (
            "fact",
            "A verifiable statement about the project (paths, commands, owners)",
        ),
    ] {
        kinds.push_str(&format!("| `{name}` | {use_when} |\n"));
    }
    format!(
        "Store one durable fact in Threski memory with `memory_add`.\n\n\
Fact: {content}\n\n\
Pick the narrowest kind ({kind}) and say which; ask only if genuinely \
ambiguous. Kind guide:\n\n\
| Kind | Use when |\n\
|---|---|\n\
{kinds}\n\
Content rules: title short and imperative (\"Use Streamable HTTP at /mcp\"); \
one fact per memory, actionable by a future agent; include file paths or \
commands when they make the memory verifiable; visibility {visibility} \
(team benefits the whole team, personal only on explicit request); never \
store secrets, tokens or temporary task state. If a similar memory exists \
(`memory_search` first when unsure), supersede it with `memory_update` \
instead of adding a duplicate.\n\n\
After writing, confirm in one line: kind, title, visibility."
    )
}

/// Handles one `completion/complete` request for prompt arguments:
/// static completion values (prefix-filtered) plus, for `project_name`,
/// the configured project names. Unknown prompts/arguments and resource
/// references yield an empty completion (spec: servers return an empty
/// completion when they cannot complete).
pub fn complete(request: &CompleteRequestParams, projects: &[String]) -> CompleteResult {
    let empty = || CompleteResult::new(CompletionInfo::default());
    let Reference::Prompt(prompt_ref) = &request.r#ref else {
        return empty();
    };
    let Some(spec) = spec_of(&prompt_ref.name) else {
        return empty();
    };
    let Some(arg) = spec
        .arguments
        .iter()
        .find(|a| a.name == request.argument.name)
    else {
        return empty();
    };

    let mut values: Vec<String> = arg.completions.iter().map(|s| s.to_string()).collect();
    if arg.name == "project_name" {
        for project in projects {
            if !project.trim().is_empty() && !values.iter().any(|v| v == project) {
                values.push(project.clone());
            }
        }
    }
    values = filter_completions(values, &request.argument.value);
    values.truncate(CompletionInfo::MAX_VALUES);
    CompleteResult::new(
        CompletionInfo::with_all_values(values).unwrap_or_else(|_| CompletionInfo::default()),
    )
}

/// Case-insensitive prefix filter; an empty input returns everything, and a
/// prefix with no matches falls back to a substring match (the client may
/// complete mid-token).
fn filter_completions(values: Vec<String>, input: &str) -> Vec<String> {
    let input = input.trim();
    if input.is_empty() {
        return values;
    }
    let needle = input.to_lowercase();
    let prefix: Vec<String> = values
        .iter()
        .filter(|v| v.to_lowercase().starts_with(&needle))
        .cloned()
        .collect();
    if !prefix.is_empty() {
        return prefix;
    }
    values
        .into_iter()
        .filter(|v| v.to_lowercase().contains(&needle))
        .collect()
}

/// The configured project names for `project_name` completions (non-empty,
/// deduplicated by exact string).
pub fn project_names(state: &AppState) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for project in &state.config().projects {
        let name = project.name.trim();
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Re-export for the handler: completions read the live config snapshot.
pub fn complete_with_state(request: &CompleteRequestParams, state: &AppState) -> CompleteResult {
    complete(request, &project_names(state))
}

/// The per-prompt argument names (protocol-test surface).
#[allow(dead_code)]
pub fn argument_names_of(name: &str) -> Vec<&'static str> {
    spec_of(name)
        .map(|s| s.arguments.iter().map(|a| a.name).collect())
        .unwrap_or_default()
}

/// Renders one prompt's user message (protocol-test surface; the handler
/// wraps it into a [`PromptMessage`]).
#[allow(dead_code)]
pub fn render_named(name: &str, values: &HashMap<String, String>) -> Option<String> {
    let spec = spec_of(name)?;
    let mut object = JsonObject::new();
    for (k, v) in values {
        object.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    Some((spec.render)(&ArgValues::new(Some(&object))))
}
