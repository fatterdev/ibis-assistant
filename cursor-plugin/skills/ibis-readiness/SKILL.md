---
name: ibis-readiness
description: First-use and empty-window checklist for Ibis Assistant MCP (wired vs missing AI, tickets, workspace)
---

# Ibis readiness

Use this skill when the user asks whether Ibis is set up, why ask/logs/tickets fail, or how to use the plugin in an empty window.

## Checklist

1. **MCP**: Is `ibis` connected? No Cursor MCP login is required. Binary must be on `PATH`.
2. **STATUS**: Read plugin `STATUS.md` (Auth = none for MCP; data dir under `%LOCALAPPDATA%\ibis\plugin`).
3. **AI**: `settings_get` → if keys missing, `settings_open_ui` (tokenized ephemeral URL is OK).
4. **Tickets**: `ticket_get_capabilities` → providers `[]` means not configured yet.
5. **Workspace**: empty window = handshake only; open a git folder for `init_project` / `ask_project`.
6. **Shared runtime (optional)**: personal `ibis run` on `:3030` and `IBIS_MCP_URL=http://127.0.0.1:3030/mcp` so Cursor and VS Code attach to one process instead of two stdio children.
7. **Memory / temporal**: `load_session_context` once for durable **current** team truth; `ask_project` for HEAD code + **commit_chunk** change meaning; `why_changed` for why/when forensics. Use **Ibis: Remember** / **Set working state** for capture. Prefer `source_hash`/`source_refs` on `memory_add` when linking code so staleness refresh works.

## Output

A short table: MCP live | AI | Ticketing | Workspace | Next step.
