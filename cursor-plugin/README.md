<p align="center">
  <img src="assets/logo.png" alt="Threski Studio" width="128" height="128">
</p>

# Threski Studio for Cursor

Thin Cursor plugin that **spawns** the Threski Studio Go binary as an MCP stdio child (default). It does not embed Go in-process and does not ship the binary.

The opened folder is the live workspace (`RUNTIME_MODE=plugin`). Knowledge is stored under `%LOCALAPPDATA%\threski\plugin` (or `THRESKI_DATA_DIR`), separate from a personal `threski` daemon unless you explicitly use a **shared runtime** (below). A Cursor empty window (no folder) still completes the MCP handshake (no `${workspaceFolder}` in `mcp.json`, no SurrealDB) so the shared plugin id is not marked failed. The opened folder is bound from `THRESKI_WORKSPACE` / Cursor workspace env / MCP `roots/list` when present.

## What you get here

- **MCP** (`mcp.json`) — tools for ingest, ask, logs, tickets, PRs, settings, typed memory
- **Commands** — **Threski: Open Settings**, **Threski: Status**, **Threski: Remember**, **Threski: Set working state**
- **Rules / skill** — readiness guidance (`rules/`, `skills/threski-readiness/`)
- **Hooks** — non-spamming session context guidance plus conditional durable-observation capture (`hooks/hooks.json`)
- **STATUS.md** — Auth/readiness notes for agents (no Cursor MCP login)

Memory operator guide: [docs/memory_layer.md](../docs/memory_layer.md). Prefer `load_session_context` once when prior context helps; do not call `memory_*` on every turn.

## Settings

- Command **Threski: Open Settings** → MCP `settings_open_ui`
- Tools: `settings_get`, `settings_apply`, `settings_open_ui`
- Personal HTTP: `http://127.0.0.1:<port>/settings/`
- Plugin-only: ephemeral loopback Settings URL with `?token=` when personal HTTP is not reachable

## Install

1. Build or install the Go binary and put it on `PATH` as `threski` (Windows: `threski.exe`).

   From the repo root, install into the user Go bin (already on a typical Windows PATH):

   ```powershell
   go build -o "$env:USERPROFILE\go\bin\threski.exe" ./cmd/server
   ```

   A personal install (`threski /install`) also works if that directory is on `PATH`.

   Cursor spawns the command **without** the repo as cwd. If the exe exists only in the working tree, MCP fails with: `threski is not recognized as an internal or external command`.

2. Enable this plugin from the repo copy, or from the local Cursor plugins folder:

   - Canonical source: `cursor-plugin/` in [threski](https://github.com/fatterdev/threski-studio)
   - Local install: `~/.cursor/plugins/local/threski/` (copied from `cursor-plugin/`)

3. Reload Cursor. The MCP server `threski` starts with:

   ```text
   threski -mode stdio -runtime-mode plugin
   RUNTIME_MODE=plugin
   ```

   Do **not** put `${workspaceFolder}` in plugin `mcp.json`. Cursor empty windows cannot resolve that variable and abort spawn, which marks the shared MCP identifier as error. The Go child binds the opened folder from `THRESKI_WORKSPACE` (if you set it), Cursor/VS Code workspace env vars, MCP `roots/list`, or a git cwd that is not the user home directory.

## Personal daemon vs plugin / shared runtime

**Default:** each IDE that enables the plugin spawns its **own** stdio child. Do **not** point two plugin children at the same SurrealDB.

**Optional shared runtime:** run one personal HTTP daemon (`threski run` on `127.0.0.1:3030`) and set `THRESKI_MCP_URL=http://127.0.0.1:3030/mcp` so the IDE host attaches as an MCP client instead of spawning a second child. See [ide-host](../ide-host/README.md) when present.

If a personal process is already running and you still spawn plugin stdio without `THRESKI_MCP_URL`, the plugin logs a warning and continues on its own data dir.

## Usage

In a Git checkout, call `init_project` (or ask Threski Studio about the repo). Personal/plugin runtimes read the working tree in place; they do not `git clone` or `git reset --hard`.

Optional env:

| Variable | Purpose |
|----------|---------|
| `THRESKI_WORKSPACE` | Optional opened-folder path. Plugin `mcp.json` does not set `${workspaceFolder}` (empty windows cannot resolve it). |
| `THRESKI_DATA_DIR` | Override plugin data directory |
| `THRESKI_MCP_URL` | When set (e.g. `http://127.0.0.1:3030/mcp`), attach to that Streamable HTTP MCP instead of spawning stdio (shared runtime) |
| `RUNTIME_MODE` | Must stay `plugin` for isolated stdio spawn |

AI setup for plugin data dir: [docs/ai_and_settings.md](../docs/ai_and_settings.md).

The Go server, HTTP MCP (`/mcp`), Settings UI (`/settings/`), and Windows service stay in the main repository, not in this plugin package.
