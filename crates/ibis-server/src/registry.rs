//! The declarative 56-tool MCP registry (D3) — the 56 tool registrations,
//! reproduced byte-for-byte.
//!
//! Every tool is data: `name`, exact `description`, and an input schema
//! matching what the reference MCP library generates — per property
//! `{"description": …, "type": …}` with keys (and property names) marshaled
//! in sorted-map order, wrapped in `{"properties": …, "type":"object"}`
//! (no `required` — none of the registrations used `Required`).
//! `tests/tool_contract_test.rs` verifies the set against the gate
//! and this table drives both `tools/list` and the discovery `tools[]`
//! snapshot (D3 verification fixture).
//!
//! FORBIDDEN legacy names (`redmine_search_issues`, `redmine_update_issue`,
//! …) are asserted absent by the contract test.
//!
//! F0.2 tool annotations: [`TOOL_ANNOTATIONS`] is the data table pairing
//! every registered tool with a human-readable `title` and an explicit
//! `readOnlyHint` — read-only tools never prompt for confirmation, mutators
//! always declare themselves. The completeness test pins the bijection with
//! [`TOOL_SPECS`] and the exact read-only set.
//!
//! Handlers live in [`crate::tools`]; the router wires one dynamic route per
//! spec to [`crate::tools::dispatch`].

use std::sync::{Arc, OnceLock};

use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{json, Value};

/// One row of the tool table (D3): schema is generated as data so the
/// discovery snapshot fixture can verify byte-compatibility with
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// `{"type":"object","properties":{…}}` — property objects are
    /// `{"description": …, "type": …}`.
    pub properties: &'static [(&'static str, &'static str, PropKind)],
}

/// The reference property shapes used by the original registrations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// `mcp.WithString`
    String,
    /// `mcp.WithNumber`
    Number,
    /// `mcp.WithBoolean`
    Boolean,
}

/// Builds the the reference MCP library-equivalent schema for a spec:
/// `{"type":"object","properties":{"<n>":{"description":"<d>","type":"<t>"}}}`
/// with sorted keys (`json.Marshal sorts map keys; serde_json's default
/// BTreeMap does the same).
pub fn schema_of(spec: &ToolSpec) -> Value {
    let mut properties = serde_json::Map::new();
    for (name, description, kind) in spec.properties {
        properties.insert(
            (*name).to_string(),
            json!({ "description": description, "type": kind.as_str() }),
        );
    }
    json!({ "type": "object", "properties": properties, "required": [] })
}

impl PropKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PropKind::String => "string",
            PropKind::Number => "number",
            PropKind::Boolean => "boolean",
        }
    }
}

macro_rules! p {
    ($name:literal, $desc:literal, $kind:ident) => {
        ($name, $desc, PropKind::$kind)
    };
}

/// The authoritative 56-tool table. Descriptions are the exact
/// strings.
pub static TOOL_SPECS: &[ToolSpec] = &[
    // ---- Project session & ingest (4) ----
    ToolSpec {
        name: "init_project",
        description: "Initialize a repository session. Checks out the specific branch/commit and triggers ingestion if necessary.",
        properties: &[
            p!("project_name", "Name of the project", String),
            p!("origin_url", "Git origin URL", String),
            p!("branch", "Target branch", String),
            p!("commit", "Target commit hash (optional, takes precedence)", String),
        ],
    },
    ToolSpec {
        name: "update_project_status",
        description: "Update project status after local commits/pushes to trigger incremental ingestion.",
        properties: &[
            p!("project_name", "Name of the project", String),
            p!("origin_url", "Git origin URL", String),
            p!("branch", "Current branch", String),
            p!("commit", "Current commit hash", String),
        ],
    },
    ToolSpec {
        name: "sync_local_patch",
        description: "Apply a local unpushed unified diff to the project workspace after init_project returned requires_patch, then re-index the patched tree.",
        properties: &[
            p!("project_name", "Name of the project (same as init_project)", String),
            p!("patch", "Unified diff text (git format) to apply with git apply", String),
            p!("commit", "Optional local commit hash for session tracking after patch", String),
        ],
    },
    ToolSpec {
        name: "ingest_code",
        description: "Trigger codebase vectorization (Delta RAG). If no path provided, scans all repos.",
        properties: &[p!("path", "Optional specific repo path", String)],
    },
    // ---- Git credentials (1) ----
    ToolSpec {
        name: "git_configure_credentials",
        description: "Configure persistent Git credentials on the server for a specific domain or repository URL.",
        properties: &[
            p!("target", "Domain (e.g. github.com) or repository URL", String),
            p!("provider", "Git provider name: github, gitlab, azure_devops, or generic", String),
            p!("auth_type", "Authentication type: token, basic, or ssh", String),
            p!("token", "PAT, Token, or Password (required for token/basic)", String),
            p!("username", "Username (optional, for basic auth)", String),
            p!("ssh_private_key", "SSH Private Key (optional, for ssh auth)", String),
        ],
    },
    // ---- Memory / knowledge graph / temporal (26) ----
    ToolSpec {
        name: "provide_collaborative_memory",
        description: "Inject collaborative memory/context for the project. Optional embeddings array can be provided to skip server-side AI embedding generation.",
        properties: &[
            p!("project_name", "Name of the project", String),
            p!("memory_text", "The textual content of the memory/insight", String),
            p!("embedding", "Optional pre-computed vector embedding (JSON array of floats)", String),
        ],
    },
    ToolSpec {
        name: "save_reasoning_outcome",
        description: "Save a semantic deduction/outcome for the project and reinforce the paths that led to it. Replaces reinforce_path.",
        properties: &[
            p!("project_name", "Name of the project", String),
            p!("question", "The original question or problem statement", String),
            p!("outcome_text", "The deduced reasoning outcome or solution", String),
            p!("useful_sources", "Comma-separated list of source IDs (commits, issues, files) that were helpful in reaching the outcome", String),
        ],
    },
    ToolSpec {
        name: "memory_add",
        description: "Create a typed, temporally-aware memory (decision/fix/constraint/rule/gotcha/design/fact/observation/reasoning). Defaults to visibility=team. Pass source_hash (source_file content hash) and/or source_refs to source_file:* so git-driven RefreshStaleMemories can invalidate after HEAD ingest. Layer 3: pass introduced_in (full commit OID) or a commit:* source_ref to git-bind the claim; file-anchored claims follow anchor_mode (require_clean refuses dirty paths, worktree marks provenance.dirty, promote_on_commit binds on later commit).",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("kind", "decision|fix|constraint|rule|gotcha|design|fact|working_state|observation|reasoning (default fact)", String),
            p!("title", "Optional short title", String),
            p!("content", "Full memory text (required)", String),
            p!("tags", "Optional comma-separated tags", String),
            p!("status", "current|superseded|archived|stale (default current)", String),
            p!("confidence", "0.0-1.0 (default 0.7)", Number),
            p!("visibility", "team (default) | personal", String),
            p!("branch", "Optional git branch this memory applies to", String),
            p!("valid_from", "Optional RFC3339 timestamp (default now)", String),
            p!("valid_until", "Optional RFC3339 expiry", String),
            p!("source_refs", "Optional comma-separated record IDs this memory is derived_from", String),
            p!("source_hash", "Optional provenance hash of linked source_file content (auto-resolved from source_file refs when omitted)", String),
            p!("source_path", "Optional canonical source path for path-based staleness (auto-resolved from source_file refs when omitted)", String),
            p!("introduced_in", "Optional full commit OID this claim was introduced in (Layer 3 git binding; commit:* source_refs bind too)", String),
            p!("anchor_mode", "Optional dirty-tree policy for file-anchored claims: require_clean | worktree (default) | promote_on_commit (flagged in provenance; promotion lands with the as_of phases)", String),
            p!("supersedes", "Optional memory record ID this one replaces", String),
            p!("embedding", "Optional precomputed JSON float array embedding", String),
            p!("actor", "Optional identity fallback when X-Ibis-Actor header is absent", String),
        ],
    },
    ToolSpec {
        name: "memory_bootstrap_git_store",
        description: "Claim store addendum \u{a7}5: render existing durable claims (decision/rule/fix, visibility per personal-claims policy) into .ibis/claims/ and create ONE commit (chore(claims): bootstrap git claim store (N claims)). Idempotent: claims already in the tree are skipped; zero new files produce no commit. Pre-bootstrap honesty: git-owned claims get I(m) = bootstrap commit; earlier wall-clock history stays Surreal-only.",
        properties: &[
            p!("project_name", "Target project/repo scope (optional when unambiguous)", String),
            p!("dry_run", "Report the planned files without writing or committing", Boolean),
        ],
    },
    ToolSpec {
        name: "memory_rebuild_git_notes",
        description: "Claim store addendum \u{a7}6: rebuild refs/notes/ibis-claims from the Surreal claim index — one `claim=<id>` pointer note per historical commit anchored to a git-owned claim (introduced_in/invalidated_at). Notes are cross-links enriching commit meaning: pointers only, never claim storage, never required for correctness, rebuildable from the DB. Idempotent reconcile: missing notes added, mismatched rewritten, stale notes removed; anchors absent from the object database are skipped and counted.",
        properties: &[
            p!("project_name", "Target project/repo scope (optional when unambiguous)", String),
            p!("dry_run", "Report the planned note writes without changing the notes ref", Boolean),
        ],
    },
    ToolSpec {
        name: "as_of_project",
        description: "Layer 3 world slice at ref R: claims that HOLDAt(m,R) and/or code bytes at R — what was true at a specific commit, not just at HEAD. Requires git context; unanchored claims are excluded (strict); personal claims visible to their owner only. Code projection is the honest hybrid: HEAD embeddings restricted to paths in tree(R) with bodies replaced by git show bytes (no per-commit embedding corpus). Every hit carries as_of_ref and source_kind.",
        properties: &[
            p!("project_name", "Target project/repo scope (required)", String),
            p!("ref", "Full commit OID, branch, tag, or HEAD (required)", String),
            p!("projection", "claims (default) | code | both", String),
            p!("actor", "Optional identity fallback when X-Ibis-Actor header is absent", String),
        ],
    },
    ToolSpec {
        name: "memory_search",
        description: "Semantic search over typed memories, filtered by kind/status/visibility/tags.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("query", "Semantic query text (required)", String),
            p!("kind", "Optional comma-separated kind filter", String),
            p!("status", "Optional status filter (default current)", String),
            p!("visibility", "team|personal|all (default all: team + caller's own personal)", String),
            p!("tags", "Optional comma-separated tag filter (ANY match)", String),
            p!("limit", "Optional max results (default 10)", Number),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "memory_expand",
        description: "Expand one memory with its supersession lineage, derived_from sources, and workstream.",
        properties: &[
            p!("id", "Memory record ID (required)", String),
            p!("depth", "Optional traversal depth (default 1)", Number),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "memory_update",
        description: "Patch fields on an existing memory. Does not create a supersedes edge; use memory_add(supersedes=...) to replace a memory.",
        properties: &[
            p!("id", "Memory record ID (required)", String),
            p!("content", "Optional new content", String),
            p!("title", "Optional new title", String),
            p!("kind", "Optional new kind", String),
            p!("status", "Optional new status", String),
            p!("confidence", "Optional new confidence", Number),
            p!("tags", "Optional comma-separated replacement tags", String),
            p!("visibility", "Optional new visibility", String),
            p!("valid_until", "Optional RFC3339 expiry", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "memory_delete",
        description: "Soft-delete (archive) or hard-delete a memory. Hard delete requires owner == actor.",
        properties: &[
            p!("id", "Memory record ID (required)", String),
            p!("hard", "true = permanently delete; default false (archive)", Boolean),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "memory_history",
        description: "Return the full supersedes lineage for a memory, ordered oldest to newest.",
        properties: &[
            p!("id", "Any memory record ID or subject phrase in the lineage (required)", String),
            p!("depth", "Optional supersession traversal depth (default 1, max 5)", Number),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "store_rule",
        description: "Store a durable team rule/convention (kind=rule, visibility=team).",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("title", "Rule title (required)", String),
            p!("content", "Rule text (required)", String),
            p!("tags", "Optional comma-separated tags", String),
            p!("branch", "Optional branch this rule applies to", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "get_rules",
        description: "List current team rules/conventions, optionally filtered by query/tags.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("query", "Optional semantic filter; empty returns all current rules by recency", String),
            p!("tags", "Optional comma-separated tag filter", String),
            p!("limit", "Optional max results (default 50)", Number),
        ],
    },
    ToolSpec {
        name: "set_working_state",
        description: "Upsert the caller's current task/working state for a project+branch (kind=working_state, personal).",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("branch", "Optional branch", String),
            p!("content", "Current task, open files, next steps (required)", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "get_working_state",
        description: "Fetch the caller's current task/working state for a project+branch.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("branch", "Optional branch", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "recent_failures",
        description: "List recently captured fix/gotcha memories (plus related log error types) so the agent avoids repeating an already-solved mistake.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("kind", "Optional comma-separated kind filter (default fix,gotcha)", String),
            p!("since", "Duration like 7d/30d (default 30d)", String),
            p!("limit", "Optional max results (default 10)", Number),
        ],
    },
    ToolSpec {
        name: "why_changed",
        description: "Explain why a file/symbol/issue changed using memory provenance, the commit/issue graph, and commit_chunk meaning excerpts (message+patch). Prefer this for why/when questions. Exactly one of file_path, symbol_name, issue_id is expected.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("file_path", "Optional file path subject", String),
            p!("symbol_name", "Optional symbol name subject", String),
            p!("issue_id", "Optional issue ID subject", String),
            p!("limit", "Optional max memories returned (default 5)", Number),
        ],
    },
    ToolSpec {
        name: "load_session_context",
        description: "Single aggregator for session start: working state + current (non-expired) rules + recent failures + top current team memories/decisions + active workstreams. Call ONCE per session for durable truth; use ask_project/why_changed for code and commit meaning.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("branch", "Optional branch", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "capture_observation",
        description: "Cheap, un-classified capture of a raw observation (kind=observation). Use sparingly for genuinely new, reusable findings; a later consolidation pass classifies these.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("text", "Raw observation text (required)", String),
            p!("source", "agent|tool|user (default agent)", String),
            p!("session_id", "Optional session identifier grouping observations", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "memory_consolidate",
        description: "Trigger the local-first sleeptime pass that distills observations into typed memories, merges duplicates, and decays noise.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("max_items", "Maximum observations to process (default 20)", Number),
        ],
    },
    ToolSpec {
        name: "memory_share",
        description: "Promote a memory's visibility to team. Only the owner may share their own memory; demoting back to personal is not supported.",
        properties: &[
            p!("id", "Memory record ID (required)", String),
            p!("visibility", "Must be 'team' (required)", String),
            p!("actor", "Optional identity fallback; must match owner", String),
        ],
    },
    ToolSpec {
        name: "memory_export",
        description: "Export memories as JSON or markdown from this runtime's SurrealDB. Defaults to visibility=team so other owners' personal rows are never exported. Pass visibility=personal|all with a trusted actor (loopback/stdio) to include your own personal memories. Use with memory_import to transfer between plugin-isolated (%LOCALAPPDATA%\\ibis\\plugin), personal (127.0.0.1 daemon), and server runtimes. SurrealDB stays the source of truth; markdown also backs optional .ibis/memory projection.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("kind", "Optional kind filter", String),
            p!("status", "Optional status filter (default current)", String),
            p!("visibility", "team (default) | personal | all", String),
            p!("format", "json (default) | markdown", String),
            p!("actor", "Optional identity for personal|all exports (trusted on loopback/stdio only)", String),
        ],
    },
    ToolSpec {
        name: "memory_import",
        description: "Import a JSON array of memories (typically from memory_export) into this runtime. Covers plugin-isolated \u{2194} personal \u{2194} server transfers. on_conflict=skip (default, match by provenance.source_hash or content) or supersede.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("items_json", "JSON array of memory objects (required)", String),
            p!("on_conflict", "skip (default) | supersede", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "ingest_design_note",
        description: "Ingest a design document/note as kind=design memory with optional derived_from provenance. Pass content text and/or a local file path.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("title", "Optional title (defaults from filename when path is set)", String),
            p!("content", "Design note body (optional when path is set)", String),
            p!("path", "Optional local markdown/design file to read when content is empty", String),
            p!("source_refs", "Optional comma-separated record IDs for derived_from edges", String),
            p!("tags", "Optional comma-separated tags", String),
            p!("visibility", "team (default) | personal", String),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "memory_insights",
        description: "Cluster typed memories by kind/tags (optional embedding similarity + local-first AI labels) for a lightweight project insight view.",
        properties: &[
            p!("project_name", "Target project/repo scope", String),
            p!("kind", "Optional comma-separated kind filter", String),
            p!("status", "Optional status filter (default current)", String),
            p!("limit", "Max memories to cluster (default 200)", Number),
            p!("use_embeddings", "Split same kind/tag buckets by stored embedding similarity", Boolean),
            p!("label_with_ai", "Ask local-first AI (bulk_local) for short cluster labels", Boolean),
            p!("actor", "Optional identity fallback", String),
        ],
    },
    ToolSpec {
        name: "knowledge_diff",
        description: "Layer 3: structured delta of truths between two refs — claims added/removed via the \u{b1} HOLDAt symmetric difference (supersessions appear as old-removed + new-added, never phantom content_changed) plus the git path diff. Churn-capped: oversized results are truncated and flagged, never dumped unbounded.",
        properties: &[
            p!("project_name", "Target project/repo scope (required)", String),
            p!("from_ref", "Base ref (full OID, branch, tag, or HEAD) (required)", String),
            p!("to_ref", "Target ref (required)", String),
            p!("max_claims", "Optional soft cap on total added+removed claims (default 200); overflow sets truncated=true", Number),
            p!("diff_mode", "explicit (default: from_ref as given) | since_divergence (from_ref implicit: merge base of from_ref and to_ref — what changed on to's line since the branches diverged)", String),
            p!("actor", "Optional identity fallback when X-Ibis-Actor header is absent", String),
        ],
    },
    // ---- Code search & analysis (3) ----
    ToolSpec {
        name: "ask_project",
        description: "Hybrid agentic Q&A over HEAD file_chunk (current code), commit_chunk (change meaning), current team memory, and reasoning. Prefer why_changed for focused change forensics; prefer load_session_context/memory_search for durable team truth. Layer 3: pass at_ref (+ project_name) for the world slice at a commit — claims filtered by HOLDAt (status-current ignored at historical refs), changelog hits restricted to commits reachable from that ref, code bytes at ref via git show (HEAD-embedding hits outside tree(ref) are dropped). Claim store: without SurrealDB the at_ref slice degrades to git-native channels (claims from .ibis/claims/ at the ref — stubs title-only — and exact-path code) and the reply flags degraded:true.",
        properties: &[
            p!("query", "The question (e.g., 'Why was login changed?')", String),
            p!("branch_or_commit", "Optional current branch or commit to provide topological context to the AI", String),
            p!("at_ref", "Optional full commit OID (or branch/tag) for the as_of world slice; skips HEAD agentic synthesis and returns structured HOLDAt-filtered hits", String),
            p!("project_name", "Required with at_ref: project scope for claim filtering and repo resolution", String),
        ],
    },
    ToolSpec {
        name: "analyze_blast_radius",
        description: "Analyze the blast radius of changing a specific function or symbol.",
        properties: &[
            p!("repo_name", "Name of the repository", String),
            p!("file_path", "Relative path to the file containing the symbol", String),
            p!("symbol_name", "Name of the function or symbol", String),
        ],
    },
    ToolSpec {
        name: "find_dead_code",
        description: "Find potential dead code (functions with no callers) in a repository.",
        properties: &[p!("repo_name", "Name of the repository to scan", String)],
    },
    // ---- Settings (3) ----
    ToolSpec {
        name: "settings_get",
        description: "Get AI/settings recommendations and masked current config (probe + hybrid routing).",
        properties: &[],
    },
    ToolSpec {
        name: "settings_apply",
        description: "Apply AI settings choices (keys written to config.json). Prefer settings UI when possible.",
        properties: &[
            p!("ai_mode", "hybrid|ollama|gemini|openai_compat|claude|cursor|ide_sampling|none (ide_sampling = reasoning via MCP sampling; default in plugin mode)", String),
            p!("gemini_key", "Optional Gemini API key to store", String),
            p!("openai_key", "Optional OpenAI-compatible API key", String),
            p!("claude_key", "Optional Claude API key", String),
            p!("always_smallest_local", "Force granite4.1:3b", Boolean),
            p!("use_cloud_when_no_gpu", "On CPU-only PCs, use cloud for hard questions", Boolean),
            p!("use_ide_provided_models", "Use Cursor/VS Code LM bridge when available", Boolean),
        ],
    },
    ToolSpec {
        name: "settings_open_ui",
        description: "Return the local Settings UI URL (same page as personal HTTP /settings). Open it in a browser. In plugin mode starts an ephemeral 127.0.0.1 listener with a one-shot token when personal HTTP is not reachable.",
        properties: &[],
    },
    // ---- Knowledge optimization (1) ----
    ToolSpec {
        name: "optimize_knowledge",
        description: "Trigger the RAFT self-optimization loop to improve knowledge graph weights.",
        properties: &[p!("iterations", "Number of chunks to process (default 10)", Number)],
    },
    // ---- Ticketing (15) ----
    ToolSpec {
        name: "ticket_get_capabilities",
        description: "Return ticketing capabilities, configured providers, header hints, and client env hints.",
        properties: &[],
    },
    ToolSpec {
        name: "ticket_search",
        description: "Search issues/work items across configured ticketing providers.",
        properties: &[
            p!("provider", "Optional provider override: redmine|jira|azure_devops", String),
            p!("query", "Optional search query", String),
            p!("project_key", "Optional project key for routing/filtering", String),
            p!("status", "Optional status filter", String),
            p!("type", "Optional issue/work-item type filter", String),
            p!("assignee", "Optional assignee filter", String),
            p!("author", "Optional author filter", String),
            p!("priority", "Optional priority filter", String),
            p!("updated_from", "Optional lower bound date", String),
            p!("updated_to", "Optional upper bound date", String),
            p!("limit", "Optional max results (default 20)", Number),
            p!("offset", "Optional pagination offset", Number),
            p!("sort", "Optional sort expression", String),
        ],
    },
    ToolSpec {
        name: "ticket_search_my",
        description: "Search issues/work items assigned to the current user.",
        properties: &[
            p!("provider", "Optional provider override: redmine|jira|azure_devops", String),
            p!("query", "Optional search query", String),
            p!("project_key", "Optional project key", String),
            p!("status", "Optional status filter", String),
            p!("type", "Optional issue/work-item type filter", String),
            p!("priority", "Optional priority filter", String),
            p!("updated_from", "Optional lower bound date", String),
            p!("updated_to", "Optional upper bound date", String),
            p!("limit", "Optional max results (default 20)", Number),
            p!("offset", "Optional pagination offset", Number),
            p!("sort", "Optional sort expression", String),
        ],
    },
    ToolSpec {
        name: "ticket_get",
        description: "Get a ticket/work-item by ID or external key.",
        properties: &[
            p!("provider", "Optional provider override: redmine|jira|azure_devops", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key for routing", String),
        ],
    },
    ToolSpec {
        name: "ticket_create",
        description: "Create a new ticket/work-item on the selected provider.",
        properties: &[
            p!("provider", "Optional provider override: redmine|jira|azure_devops", String),
            p!("project_key", "Project key or identifier", String),
            p!("title", "Ticket title/summary", String),
            p!("description", "Optional description", String),
            p!("type", "Optional issue/work-item type", String),
            p!("assignee", "Optional assignee", String),
            p!("priority", "Optional priority", String),
            p!("provider_fields_json", "Optional provider-specific JSON fields", String),
        ],
    },
    ToolSpec {
        name: "ticket_update",
        description: "Update an existing ticket/work-item.",
        properties: &[
            p!("provider", "Optional provider override: redmine|jira|azure_devops", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key for routing", String),
            p!("notes", "Optional notes/comment", String),
            p!("status", "Optional status", String),
            p!("type", "Optional issue/work-item type", String),
            p!("assignee", "Optional assignee", String),
            p!("priority", "Optional priority", String),
            p!("workflow_action", "Optional semantic action: resolve|close|reopen", String),
            p!("fixed_version", "Optional fixed/target version", String),
            p!("provider_fields_json", "Optional provider-specific JSON fields", String),
        ],
    },
    ToolSpec {
        name: "ticket_add_comment",
        description: "Add a comment to a ticket/work-item.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key", String),
            p!("comment", "Comment text", String),
        ],
    },
    ToolSpec {
        name: "ticket_assign",
        description: "Assign a ticket/work-item to a user.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key", String),
            p!("assignee", "Assignee identifier", String),
        ],
    },
    ToolSpec {
        name: "ticket_transition",
        description: "Transition a ticket/work-item to a specific state/status.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key", String),
            p!("transition", "Target transition/status id or name", String),
        ],
    },
    ToolSpec {
        name: "ticket_list_statuses",
        description: "List statuses/states for a provider/project and optional issue type.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("project_key", "Optional project key", String),
            p!("issue_type", "Optional issue/work-item type", String),
        ],
    },
    ToolSpec {
        name: "ticket_search_users",
        description: "Search users in ticketing provider for assignment/filtering.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("project_key", "Optional project key", String),
            p!("query", "User search query", String),
            p!("limit", "Optional max results (default 20)", Number),
        ],
    },
    ToolSpec {
        name: "ticket_list_projects",
        description: "List projects available in the selected ticketing provider.",
        properties: &[p!("provider", "Optional provider override", String)],
    },
    ToolSpec {
        name: "ticket_mark_resolved",
        description: "Move a ticket/work-item to resolved state using provider/project workflow mapping.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key", String),
            p!("issue_type", "Optional issue/work-item type", String),
            p!("notes", "Optional comment to append after transition", String),
        ],
    },
    ToolSpec {
        name: "ticket_mark_closed",
        description: "Move a ticket/work-item to closed state using provider/project workflow mapping.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key", String),
            p!("issue_type", "Optional issue/work-item type", String),
            p!("notes", "Optional comment to append after transition", String),
        ],
    },
    ToolSpec {
        name: "ticket_reopen",
        description: "Reopen a ticket/work-item using provider/project workflow mapping.",
        properties: &[
            p!("provider", "Optional provider override", String),
            p!("id", "Ticket/work-item ID or key", String),
            p!("project_key", "Optional project key", String),
            p!("issue_type", "Optional issue/work-item type", String),
            p!("notes", "Optional comment to append after transition", String),
        ],
    },
    // ---- Repo PR (2) ----
    ToolSpec {
        name: "repo_pr_create",
        description: "Create a pull request using configured SCM provider (Azure DevOps in v1).",
        properties: &[
            p!("provider", "Optional provider override: azure_devops", String),
            p!("project_name", "Optional project name (fallback to init_project session)", String),
            p!("origin_url", "Optional repository origin URL (fallback to session)", String),
            p!("repository", "Optional repository id/name (fallback derive from origin_url/session)", String),
            p!("source_branch", "Source branch (fallback to current init_project branch)", String),
            p!("target_branch", "Optional target branch (fallback project/default config)", String),
            p!("title", "Optional PR title (auto-generated if omitted)", String),
            p!("description", "Optional PR description (auto-generated if omitted)", String),
            p!("ticket_ids", "Optional comma-separated ticket ids/keys", String),
            p!("reviewer_ids", "Optional comma-separated reviewer ids", String),
            p!("auto_complete", "Optional true/false to request auto-complete", String),
        ],
    },
    ToolSpec {
        name: "repo_pr_complete",
        description: "Complete/merge an existing pull request.",
        properties: &[
            p!("provider", "Optional provider override: azure_devops", String),
            p!("project_name", "Optional project name", String),
            p!("repository", "Optional repository id/name", String),
            p!("pr_id", "Pull request id", String),
            p!("delete_source_branch", "Optional true/false", String),
            p!("squash", "Optional true/false", String),
        ],
    },
    // ---- Logs (1) ----
    ToolSpec {
        name: "analyze_logs",
        description: "Analyze log entries/text synchronously and return detected errors or anomalies.",
        properties: &[
            p!("project_name", "Name of the project", String),
            p!("log_file", "Optional log file name", String),
            p!("log_text", "Raw log text content to analyze (entries separated by newlines)", String),
        ],
    },
];

/// Looks up a spec by name (contract-test surface).
#[allow(dead_code)]
pub fn spec_of(name: &str) -> Option<&'static ToolSpec> {
    TOOL_SPECS.iter().find(|s| s.name == name)
}

/// One row of the F0.2 annotations table: the MCP hints clients use to
/// skip confirmations on pure reads and to render friendly names.
pub struct ToolAnnotation {
    pub name: &'static str,
    /// Human-readable title — set both top-level (`Tool::title`) and inside
    /// the annotations.
    pub title: &'static str,
    /// MCP `readOnlyHint`: `true` = never modifies its environment,
    /// `false` = explicit mutator (destructive defaults apply).
    pub read_only: bool,
}

/// The F0.2 annotations table: one row per registered tool, titles and the
/// explicit `readOnlyHint` split. The read-only set is exactly the pure-read
/// surface (search/expansion/history/insights, session context, settings
/// read, ticketing reads, rules read); everything else declares
/// `readOnlyHint: false`. `tests/tool_contract_test.rs` enforces the
/// bijection with [`TOOL_SPECS`].
pub static TOOL_ANNOTATIONS: &[ToolAnnotation] = &[
    // ---- Project session & ingest (4) — mutators ----
    ToolAnnotation {
        name: "init_project",
        title: "Initialize Project Session",
        read_only: false,
    },
    ToolAnnotation {
        name: "update_project_status",
        title: "Update Project Status",
        read_only: false,
    },
    ToolAnnotation {
        name: "sync_local_patch",
        title: "Sync Local Patch",
        read_only: false,
    },
    ToolAnnotation {
        name: "ingest_code",
        title: "Ingest Code",
        read_only: false,
    },
    // ---- Git credentials (1) — mutator ----
    ToolAnnotation {
        name: "git_configure_credentials",
        title: "Configure Git Credentials",
        read_only: false,
    },
    // ---- Memory / knowledge graph / temporal (26) ----
    ToolAnnotation {
        name: "provide_collaborative_memory",
        title: "Provide Collaborative Memory",
        read_only: false,
    },
    ToolAnnotation {
        name: "save_reasoning_outcome",
        title: "Save Reasoning Outcome",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_add",
        title: "Add Memory",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_bootstrap_git_store",
        title: "Bootstrap Git Claim Store",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_rebuild_git_notes",
        title: "Rebuild Git Claim Notes",
        read_only: false,
    },
    ToolAnnotation {
        name: "as_of_project",
        title: "As-Of Project Slice",
        read_only: true,
    },
    ToolAnnotation {
        name: "memory_search",
        title: "Search Memories",
        read_only: true,
    },
    ToolAnnotation {
        name: "memory_expand",
        title: "Expand Memory",
        read_only: true,
    },
    ToolAnnotation {
        name: "memory_update",
        title: "Update Memory",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_delete",
        title: "Delete Memory",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_history",
        title: "Memory History",
        read_only: true,
    },
    ToolAnnotation {
        name: "store_rule",
        title: "Store Team Rule",
        read_only: false,
    },
    ToolAnnotation {
        name: "get_rules",
        title: "Get Team Rules",
        read_only: true,
    },
    ToolAnnotation {
        name: "set_working_state",
        title: "Set Working State",
        read_only: false,
    },
    ToolAnnotation {
        name: "get_working_state",
        title: "Get Working State",
        read_only: true,
    },
    ToolAnnotation {
        name: "recent_failures",
        title: "Recent Failures",
        read_only: true,
    },
    ToolAnnotation {
        name: "why_changed",
        title: "Why Changed",
        read_only: true,
    },
    ToolAnnotation {
        name: "load_session_context",
        title: "Load Session Context",
        read_only: true,
    },
    ToolAnnotation {
        name: "capture_observation",
        title: "Capture Observation",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_consolidate",
        title: "Consolidate Memories",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_share",
        title: "Share Memory",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_export",
        title: "Export Memories",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_import",
        title: "Import Memories",
        read_only: false,
    },
    ToolAnnotation {
        name: "ingest_design_note",
        title: "Ingest Design Note",
        read_only: false,
    },
    ToolAnnotation {
        name: "memory_insights",
        title: "Memory Insights",
        read_only: true,
    },
    ToolAnnotation {
        name: "knowledge_diff",
        title: "Knowledge Diff",
        read_only: true,
    },
    // ---- Code search & analysis (3) — read-only ----
    ToolAnnotation {
        name: "ask_project",
        title: "Ask Project",
        read_only: true,
    },
    ToolAnnotation {
        name: "analyze_blast_radius",
        title: "Analyze Blast Radius",
        read_only: true,
    },
    ToolAnnotation {
        name: "find_dead_code",
        title: "Find Dead Code",
        read_only: true,
    },
    // ---- Settings (3) ----
    ToolAnnotation {
        name: "settings_get",
        title: "Get Settings",
        read_only: true,
    },
    ToolAnnotation {
        name: "settings_apply",
        title: "Apply Settings",
        read_only: false,
    },
    ToolAnnotation {
        name: "settings_open_ui",
        title: "Open Settings UI",
        read_only: false,
    },
    // ---- Knowledge optimization (1) — mutator ----
    ToolAnnotation {
        name: "optimize_knowledge",
        title: "Optimize Knowledge",
        read_only: false,
    },
    // ---- Ticketing (15) ----
    ToolAnnotation {
        name: "ticket_get_capabilities",
        title: "Ticketing Capabilities",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_search",
        title: "Search Tickets",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_search_my",
        title: "Search My Tickets",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_get",
        title: "Get Ticket",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_create",
        title: "Create Ticket",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_update",
        title: "Update Ticket",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_add_comment",
        title: "Add Ticket Comment",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_assign",
        title: "Assign Ticket",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_transition",
        title: "Transition Ticket",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_list_statuses",
        title: "List Ticket Statuses",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_search_users",
        title: "Search Ticket Users",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_list_projects",
        title: "List Ticket Projects",
        read_only: true,
    },
    ToolAnnotation {
        name: "ticket_mark_resolved",
        title: "Mark Ticket Resolved",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_mark_closed",
        title: "Mark Ticket Closed",
        read_only: false,
    },
    ToolAnnotation {
        name: "ticket_reopen",
        title: "Reopen Ticket",
        read_only: false,
    },
    // ---- Repo PR (2) — mutators ----
    ToolAnnotation {
        name: "repo_pr_create",
        title: "Create Pull Request",
        read_only: false,
    },
    ToolAnnotation {
        name: "repo_pr_complete",
        title: "Complete Pull Request",
        read_only: false,
    },
    // ---- Logs (1) — mutator (ingestion-adjacent, emits progress) ----
    ToolAnnotation {
        name: "analyze_logs",
        title: "Analyze Logs",
        read_only: false,
    },
];

/// Looks up the annotation row for a tool name (contract-test surface and
/// [`annotated_tool`]).
pub fn annotation_of(name: &str) -> Option<&'static ToolAnnotation> {
    TOOL_ANNOTATIONS.iter().find(|a| a.name == name)
}

/// F0.6 dynamic tool discovery: the 15 `ticket_*` tools are config-gated —
/// announced only when at least one ticketing provider is registered
/// (`AppState::ticketing_configured`), so unconfigured installs surface the
/// core tool set only instead of 15 tools that always error.
pub static TICKETING_TOOLS: &[&str] = &[
    "ticket_get_capabilities",
    "ticket_search",
    "ticket_search_my",
    "ticket_get",
    "ticket_create",
    "ticket_update",
    "ticket_add_comment",
    "ticket_assign",
    "ticket_transition",
    "ticket_list_statuses",
    "ticket_search_users",
    "ticket_list_projects",
    "ticket_mark_resolved",
    "ticket_mark_closed",
    "ticket_reopen",
];

/// Whether `name` belongs to the config-gated ticketing group
/// ([`TICKETING_TOOLS`]).
pub fn is_ticketing_tool(name: &str) -> bool {
    TICKETING_TOOLS.contains(&name)
}

/// Builds one rmcp [`Tool`] for a spec: reference-shaped schema plus the
/// F0.2 annotations — the human-readable title (top-level and inside the
/// annotations) and the explicit `readOnlyHint` — and, for the three memory
/// tools, the F0.7 `_meta.ui.resourceUri` linkage to the Memory Curator app
/// (SEP-1865 tool-UI linkage; hosts without MCP Apps support ignore it and
/// use the plain textual output).
pub fn annotated_tool(spec: &ToolSpec) -> Tool {
    let ann = annotation_of(spec.name)
        .unwrap_or_else(|| panic!("tool {} missing its F0.2 annotation row", spec.name));
    let schema: JsonObject =
        serde_json::from_value(schema_of(spec)).expect("tool schema is a valid JSON object");
    let tool = Tool::new(spec.name, spec.description, schema)
        .with_title(ann.title)
        .with_annotations(ToolAnnotations::with_title(ann.title).read_only(ann.read_only));
    if is_curator_tool(spec.name) {
        tool.with_meta(ibis_httpserver::curator::ui_link_meta())
    } else {
        tool
    }
}

/// F0.7 — the tools whose results the Memory Curator app renders
/// (`_meta.ui.resourceUri` on the tool objects and on the successful
/// results).
pub static CURATOR_TOOLS: &[&str] = &["memory_add", "memory_search", "memory_expand"];

/// Whether `name` is linked to the Memory Curator MCP App.
pub fn is_curator_tool(name: &str) -> bool {
    CURATOR_TOOLS.contains(&name)
}

/// Precomputed schemas keyed by tool name (byte-match verification fixture,
/// D3; consumed by the contract test).
#[allow(dead_code)]
pub fn schemas() -> &'static std::collections::HashMap<&'static str, Value> {
    static SCHEMAS: OnceLock<std::collections::HashMap<&'static str, Value>> = OnceLock::new();
    SCHEMAS.get_or_init(|| TOOL_SPECS.iter().map(|s| (s.name, schema_of(s))).collect())
}

/// Builds the rmcp [`ToolRouter`] wiring one dynamic route per spec to the
/// dispatcher (`crate::tools::dispatch`) — D3: the table IS the registry.
/// Every route carries the F0.2 annotations (title + explicit
/// `readOnlyHint`).
pub fn build_tool_router() -> ToolRouter<crate::mcp_server::IbisServer> {
    let mut router = ToolRouter::new();
    for spec in TOOL_SPECS {
        let tool = annotated_tool(spec);
        router.add_route(ToolRoute::new_dyn(tool, |ctx| {
            Box::pin(async move {
                let name = ctx.name().to_string();
                let args = ctx.arguments.clone().unwrap_or_default();
                crate::tools::dispatch(&name, args, ctx)
                    .await
                    .map(|r| r.into())
            })
        }));
    }
    router
}

/// Builds the discovery `tools[]` provider (sorted by name,
/// `ListTools`-equivalent rows).
pub fn discovery_tools() -> ibis_httpserver::discovery::ToolsProvider {
    Arc::new(|| {
        let mut tools: Vec<ibis_httpserver::discovery::ToolMeta> = TOOL_SPECS
            .iter()
            .map(|s| ibis_httpserver::discovery::ToolMeta {
                name: s.name.to_string(),
                description: s.description.to_string(),
                input_schema: schema_of(s),
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    })
}
