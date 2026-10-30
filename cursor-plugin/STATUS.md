# Ibis Assistant — plugin STATUS

English readiness notes for agents and humans. **No Cursor MCP login** is required for this package.

## MCP

- Transport: stdio child `ibis -mode stdio -runtime-mode plugin`
- Namespace: `ibis` (ready when the Rust binary `ibis` is on `PATH`)
- Empty Cursor window (no folder): handshake still succeeds by design; SurrealDB/ingest stay off until a workspace is bound

## Data

- Default plugin knowledge dir: `%LOCALAPPDATA%\ibis\plugin` (override with `IBIS_DATA_DIR`)
- Separate from a personal HTTP daemon (`ibis run` on `:3030`)
- **Do not** run two plugin stdio children against the same SurrealDB. Prefer one personal daemon and `IBIS_MCP_URL=http://127.0.0.1:3030/mcp` for a shared runtime (see IDE host docs)
- Memory context: call `load_session_context` at most once for current team truth (working state, rules, fixes, decisions)
- Change meaning: `ask_project` / `why_changed` surface `commit_chunk` (why/when); do not treat commit hits as HEAD source
- Capture: use `capture_observation` only for a genuinely new, reusable lesson; the stop hook skips capture when there is nothing durable

## Commands

- **Ibis: Status** — readiness summary (STATUS, AI, ticketing, workspace)
- **Ibis: Open Settings** → MCP `settings_open_ui`
- **Ibis: Remember** → MCP `memory_add` with kind `decision` / `fix` / `rule`
- **Ibis: Set working state** → MCP `set_working_state` for the current task/branch

## Settings / AI

- Command **Ibis: Open Settings** → MCP `settings_open_ui`
- Plugin mode opens personal `/settings/` when reachable, else an ephemeral `127.0.0.1` UI with `?token=`
- Tools: `settings_get`, `settings_apply`, `settings_open_ui`
- Hybrid AI keys live in the plugin data-dir `config.json`. Context-only fallback when local and cloud fail

## Ticketing

- Call `ticket_get_capabilities` before ticket search/create
- Providers stay empty until configured (Redmine / Jira / Azure DevOps headers or config)
- Optional headers: `X-Redmine-API-Key`, `X-Jira-Email` + `X-Jira-API-Token`, `X-Azure-DevOps-PAT`, git token headers

## First-use checklist

1. Rust binary `ibis` on `PATH`
2. Open a git repo folder (for full value)
3. Configure AI keys via Settings (or personal daemon + shared `IBIS_MCP_URL`)
4. `init_project` then `ask_project` / log tools as needed
