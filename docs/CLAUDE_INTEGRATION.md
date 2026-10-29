# Claude Desktop Integration Guide

This guide describes how to connect **Claude Desktop** to the **Threski Studio** using a local bridge.

## 🌉 Why a Bridge?

Claude Desktop currently supports "Stdio" (standard input/output) connections for local MCP servers. Threski Studio's **product default** for HTTP clients is Streamable HTTP at `/mcp`. Legacy HTTP+SSE (`/sse`) remains so `mcp-bridge` and older clients can share the same knowledge graph.

To connect Claude Desktop, we use a lightweight **MCP Bridge** that runs locally, talks "Stdio" to Claude, and forwards messages to the Threski Studio **SSE** endpoint (`http://localhost:3030/sse`) until the bridge is moved onto `/mcp`.

---

## 🛠️ Installation & Setup

### 1. Prerequisite: The Threski Studio
Ensure the Threski Studio is running (e.g. `http://localhost:3030/mcp` for new HTTP clients; the bridge below still uses `/sse`).

### 2. Build the Bridge Tool
If you haven't already, compile the bridge tool included in this repository.

```powershell
# In the repository root
go build -o mcp-bridge.exe ./tools/mcp-bridge
```

You should now have `mcp-bridge.exe` in your folder. Move it to a permanent location if desired (e.g., `C:\Tools\mcp-bridge.exe`).

### 3. Configure Claude Desktop
Open your Claude Desktop configuration file:
*   **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`
*   **Mac**: `~/Library/Application Support/Claude/claude_desktop_config.json`

Add the following entry to the `mcpServers` section:

```json
{
  "mcpServers": {
    "knowledge-graph": {
      "command": "C:\\Tools\\mcp-bridge.exe",
      "args": [
        "-url", "http://localhost:3030/sse",
        "-key", "YOUR_REDMINE_API_KEY"
      ]
    }
  }
}
```

> **Note**: Replace `YOUR_REDMINE_API_KEY` with your personal Redmine API Key found in your Redmine account page. This is required for actions like updating tickets.

Restart Claude Desktop. You should see a green connection icon for "knowledge-graph".

---

## Available tools reference

Source of truth: `cmd/server/main.go`. Obsolete names (`ingest_git`, `reinforce_path`) are **not** registered.

### Knowledge and search

* **`ask_project(query, branch_or_commit?)`**: Hybrid Q&A over HEAD `file_chunk`, **`commit_chunk` change meaning**, current team memory, and reasoning (agentic). Prefer `why_changed` for focused change forensics; prefer `load_session_context` / `memory_search` for durable team truth. See [commit_meaning_temporal.md](commit_meaning_temporal.md).
* **`provide_collaborative_memory(project_name, memory_text, embedding?)`**: Legacy collaborative write (still registered); also creates a typed `kind=fact` team memory.
* **`save_reasoning_outcome(project_name, question, outcome_text, useful_sources)`**: Persist outcomes, lightly reinforce useful paths, and link a `kind=reasoning` memory (secondary to commit-meaning historicization).
* **`optimize_knowledge(iterations?)`**: RAFT-style self-optimization loop over code chunks.
* **`analyze_blast_radius(...)`** / **`find_dead_code(repo_name)`**: Structural impact / dead-symbol helpers.

### Typed memory (`memory_*`)

Sharing-first model (default `visibility=team`). Prefer these over ad-hoc notes. Operator guide: [memory_layer.md](memory_layer.md). Design: [superpowers/specs/2026-10-09-memory-absorption-design.md](superpowers/specs/2026-10-09-memory-absorption-design.md).

* **CRUD / query:** `memory_add`, `memory_search`, `memory_expand`, `memory_update`, `memory_delete`, `memory_history`
* **Session:** `load_session_context`, `capture_observation`, `store_rule` / `get_rules`, `set_working_state` / `get_working_state`
* **Forensics:** `recent_failures`, `why_changed` (includes `commit_chunk` meaning excerpts)
* **Lifecycle:** `memory_consolidate` (local-first distill), `memory_share`, `memory_export` / `memory_import`
* **Design / insights:** `ingest_design_note` (`kind=design` + `derived_from`), `memory_insights` (kind/tag clustering, optional embeddings + local AI labels)
* **Optional projection:** `memory.projection_enabled` writes regenerable `.threski/memory/*.md` (SurrealDB remains source of truth)

Do **not** call `memory_*` on every turn — use `load_session_context` once when you need prior context; `capture_observation` only for reusable findings.

### Ingestion and git

* **`init_project(project_name, origin_url, branch, commit?)`**: Clone/sync and start ingestion. May return `credentials_required`, `requires_patch`, or `aligned`.
* **`sync_local_patch(project_name, patch, commit?)`**: Apply a unified diff when the tip is not on the remote (`requires_patch`) **on a server-owned clone**. Personal/plugin live trees return `live_tree` instead.
* **`update_project_status(project_name, origin_url, branch, commit)`**: Queue status/commit updates.
* **`ingest_code`**: Vectorize / AST-chunk source (ast-grep + rules).
* **`ingest_design_note`**: Store a design doc/note as typed `kind=design` memory (see Typed memory above).
* **`git_configure_credentials(...)`**: Persist Git auth in SurrealDB.
* **`analyze_logs(project_name, log_text, log_file?)`**: On-demand log analysis (static → AI → enrich). See [log_analysis_pipeline.md](log_analysis_pipeline.md).

### Settings (`settings_*`)

* **`settings_get`**: Hardware probe + recommendations (hybrid AI, auto-tier, clouds).
* **`settings_apply`**: Apply choices; keys written to `config.json`.
* **`settings_open_ui`**: URL for the Settings UI (`/settings/` when personal HTTP is up).

Operator guide: [ai_and_settings.md](ai_and_settings.md).

### Ticketing (`ticket_*`) and PRs (`repo_pr_*`)

Full list and playbooks: [TICKETING_CLIENT.md](TICKETING_CLIENT.md) and [../copilot-instructions.md](../copilot-instructions.md).

---

## Usage samples (prompt engineering)

Here are practical examples of how to leverage the system effectively.

### Scenario 1: Onboarding and understanding
*For when you are new to a module or debugging legacy code.*

> **User**: "How does the authentication system work in this project? Explain the flow and show me the relevant files."
>
> **Claude**: Uses `ask_project` to retrieve architecture, identify key files, and summarize the logic.

### Scenario 2: Historical forensics
*For understanding why code is written a certain way.*

> **User**: "Why was the timeout increased in the database connection? Check the commit history."
>
> **Claude**: Uses `ask_project` to find the commit and related issue context.

### Scenario 3: Local unpushed commit
*When `init_project` returns `requires_patch`.*

> **User**: "Index my local tip; it is not on the remote yet."
>
> **Claude**: Calls `init_project`, then `sync_local_patch` with a unified diff of the unpushed commits.

### Scenario 4: Log triage
*For production or local failures.*

> **User**: "Analyze this error log and tell me which file is at fault."
>
> **Claude**: Calls `analyze_logs` with the log text (and optional file name), then explains enrich results.

### Scenario 5: Task execution
*For planning complex changes.*

> **User**: "I need to add a new field 'LoyaltyPoints' to the User model. Please analyze the impact."
>
> **Claude**: Uses `ask_project` and optionally `analyze_blast_radius`, then proposes a plan.

### Scenario 6: Managing work
*For updating project status without leaving the chat.*

> **User**: "Find the ticket about 'Slow Startup' and add a note that I've found the bottleneck in the logger initialization."
>
> **Claude**: `ticket_search` → `ticket_update` / `ticket_add_comment`.

### Scenario 7: Teaching the system
*For improving future answers.*

> **User**: "Remember that InvoiceService and TaxCalculator are tightly coupled for tax calculation."
>
> **Claude**: Prefers `memory_add(kind="constraint"| "fact", …)` or `capture_observation` for raw findings; may still call `provide_collaborative_memory` / `save_reasoning_outcome` (back-compat — both feed the typed model). For design docs, `ingest_design_note`.
