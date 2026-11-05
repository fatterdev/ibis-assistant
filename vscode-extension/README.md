<p align="center">
  <img src="https://raw.githubusercontent.com/fatterdev/ibis-assistant/main/vscode-extension/assets/icon.png" alt="Ibis Assistant" width="128" height="128">
</p>

# Ibis Assistant for VS Code

**Living project memory over MCP.** Ibis turns a real codebase — Git history, polyglot AST structure, logs, tickets and PRs — into a queryable knowledge graph in SurrealDB, then keeps team decisions and session state on that same graph.

This extension is the VS Code packaging: it attaches to the always-on Ibis Assistant personal daemon by default (spawning an isolated stdio child only when explicitly configured), registers the MCP server for VS Code agent mode, and bridges IDE-provided language models back to the runtime.

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://github.com/fatterdev/ibis-assistant/blob/main/LICENSE)
[![VS Code](https://img.shields.io/badge/VS%20Code-%E2%89%A51.144-007ACC.svg)](https://code.visualstudio.com/)
[![Install MCP Server in VS Code](https://img.shields.io/badge/VS_Code-Install_MCP_Server-0098FF)](vscode:mcp/install?%7B%22name%22%3A%22ibis%22%2C%22command%22%3A%22ibis%22%2C%22args%22%3A%5B%22-mode%22%2C%22stdio%22%2C%22-runtime-mode%22%2C%22plugin%22%5D%7D "One click registers the Ibis MCP server (spawn mode) in VS Code")

## What you get

- **MCP server provider** — VS Code's MCP integration (`registerMcpServerDefinitionProvider`) attaches to the personal daemon over Streamable HTTP by default, or spawns `ibis -mode stdio -runtime-mode plugin` when `ibis.runtime.mode=spawn` is selected.
- **Status bar item** — shows the effective backend (attach/offline/spawn) plus LM-bridge and MCP-provider indicators; click it to run **Ibis Assistant: Status**, which offers **Start Daemon** when the personal daemon is unreachable.
- **Start Daemon** — **Ibis Assistant: Start Daemon** launches `ibis start` detached (the daemon outlives VS Code) and re-probes the endpoint until the MCP server answers.
- **Memory tree view** — activity-bar view browses typed memory by kind via MCP `memory_list` (current-status rows by default; `ibis.memory.statusFilter` / the title-bar filter button widens to every status), with semantic search honoring the same filter, refresh, and a read-only detail editor (MCP `memory_expand`). Both trees refresh on focus; search results are never re-run automatically (the embedding cost is paid only by an explicit search).
- **Workstreams view** — second tree in the same activity-bar container: roots from MCP `memory_workstreams` (name + current-member count, most-recently-touched first), children from `memory_list(workstream=…)` under the same status filter, same detail editor.
- **Inline curation menu** — right-click an entry (Memory or Workstreams view) to **Share** to team (`memory_share`), **Archive** (`memory_update status=archived`, confirmed), **Supersede…** (`memory_add supersedes=…` with kind/visibility prefilled — the governance route for rows you cannot write), or **Update Content…** (`memory_update content`, re-embeds; offered on owner-matching rows, falls back to Supersede on governance errors). Every action toasts the result and logs the full tool output; git-owned claims surface the runtime's repo-is-source-of-truth rejections.
- **Remember with provenance** — **Ibis Assistant: Remember Selection** (editor context menu) captures the selection plus `file:start-end` range, and **Ibis Assistant: Remember File** (editor/Explorer context menu) captures a fact about a whole file; both bind `memory_add` to the source file via `source_path` + sha256 `source_hash`, so they go stale when the file changes.
- **Session contract for agents** — the `ibis-session-contract` chat instruction (attach it in chat via *Add Instructions*) teaches agent sessions the tool selection map and memory hygiene; the **Get Started** walkthrough covers onboarding.
- **IDE LM bridge** — when `vscode.lm` is available, the runtime can use IDE-provided models (`use_ide_provided_models`); embeddings stay on Ollama.
- **Browser Settings** — **Ibis Assistant: Open Settings** reuses the personal `/settings/` UI or falls back to an ephemeral loopback URL with a one-shot token via MCP `settings_open_ui`.
- **Memory commands** — **Ibis Assistant: Remember** (`memory_add`) and **Ibis Assistant: Set working state** (`set_working_state`) straight from the Command Palette.
- **Diagnostics** — **Ibis Assistant: Status** and an **Ibis Assistant** output channel reporting backend, runtime uniqueness, and bridge state.
- **Extensive settings** — every runtime knob exposed as VS Code settings, with environment-variable fallbacks (see tables below).

## Requirements

- VS Code **1.144+** (required by `engines.vscode`; includes the MCP server-definition provider API).
- The `ibis` binary on `PATH` (or an absolute path in `ibis.runtime.binaryPath`). Download from [Releases](https://github.com/fatterdev/ibis-assistant/releases) — no Rust toolchain required for normal use.

## Install

Local VSIX (recommended while the Marketplace publishing is pending):

```powershell
cd vscode-extension
npm install
npx --yes @vscode/vsce package --no-dependencies
code --install-extension .\ibis-assistant-1.0.14.vsix
```

For iterative development open this folder in an **Extension Development Host** (`F5`) instead of `code --install-extension` on the source directory (that CLI only accepts an extension id or a `.vsix` file).

## Quick start

1. Put `ibis` on `PATH` (Windows: `ibis.exe`) and run it once so the wizard writes `config.json` (`config fast`, or `config full` for DB/SMTP/ticketing).
2. Reload VS Code. The extension attaches to the personal daemon on `127.0.0.1:3030` — when it is offline the status bar shows **Ibis daemon offline**; run **Ibis Assistant: Start Daemon** (or `ibis start` in a terminal) once and it stays up across windows and restarts.
3. Ask your agent about the repo, or call `init_project` once in a Git checkout.
4. Optional — share the same daemon with Cursor, or pin a custom endpoint:

   ```powershell
   ibis start                        # personal daemon on 127.0.0.1:3030 (detached; ibis stop to halt)
   # Cursor and VS Code both attach to http://127.0.0.1:3030/mcp by default;
   # a custom endpoint goes in ibis.runtime.mcpUrl (or IBIS_MCP_URL).
   ```

## Commands

| Command | What it does |
|---------|--------------|
| **Ibis Assistant: Open Settings** | Opens the Ibis browser Settings UI (hybrid AI, cloud keys, local models, paths, Memory browser). Probes personal HTTP first, then MCP `settings_open_ui`. |
| **Ibis Assistant: Open Extension Settings (VS Code)** | Opens VS Code Settings filtered to this extension (`@ext:fatterdev.ibis-assistant`). |
| **Ibis Assistant: Show Output** | Reveals the Ibis Assistant output channel with activation and status logs. |
| **Ibis Assistant: Status** | Reports the daemon state (reachable/offline + endpoint) or the effective backend (attach vs spawn), plugin data dir, bound workspace, LM bridge, and memory defaults. Offers **Start Daemon** when the daemon is offline. Output destination follows `ibis.status.outputMode`. |
| **Ibis Assistant: Start Daemon** | Launches `ibis start` detached and re-probes the MCP endpoint until it answers (manual-start hint on failure). |
| **Ibis Assistant: Remember** | Typed durable memory (`memory_add`) with kind quick pick and configurable default kind/visibility. |
| **Ibis Assistant: Set working state** | Branch/session scratchpad (`set_working_state`), optionally with the workspace folder as project name. |
| **Ibis Assistant: Toggle Memory Status Filter** | Flips `ibis.memory.statusFilter` between `current` (default) and `all`; the Memory view title shows `all` while the unfiltered browse is active. |
| **Ibis Assistant: Refresh Workstreams** | Re-fetches the Workstreams view roots (`memory_workstreams`) and their children. Also available as the view-title sync button. |

The **Share / Archive / Supersede… / Update Content…** actions live on the entry context menu of the Memory and Workstreams views (see *Inline curation menu* above); they are intentionally hidden from the Command Palette.

## Settings

All settings live under `ibis.*` in VS Code Settings. Precedence is **explicit VS Code setting > environment variable > default**; empty strings fall back.

### Runtime and connection

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `ibis.runtime.mode` | `auto` | `auto` attaches to the always-on personal daemon (offline state + **Start Daemon**), `spawn` keeps the isolated `ibis -mode stdio -runtime-mode plugin` child, `attach` pins the endpoint below without the daemon UX. | — |
| `ibis.runtime.mcpUrl` | `http://127.0.0.1:3030/mcp` | Daemon endpoint used when mode is `auto` or `attach`. | `IBIS_MCP_URL` (honored while `mcpUrl` is untouched) |
| `ibis.runtime.binaryPath` | `ibis` | Command or absolute path of the binary used when spawning and by **Start Daemon**. | `IBIS_BIN` |
| `ibis.runtime.dataDir` | *(empty)* | Overrides the plugin data directory (default `%LOCALAPPDATA%\ibis\plugin` or platform equivalent). | `IBIS_DATA_DIR` |
| `ibis.runtime.runtimeMode` | `plugin` | Passed to `-runtime-mode`. Keep `plugin` for IDE-owned children. | `RUNTIME_MODE` (forced for the child) |
| `ibis.runtime.extraArgs` | `[]` | Extra CLI arguments appended after the standard spawn arguments. | — |
| `ibis.runtime.workspaceOverride` | *(empty)* | Explicit opened-folder path; empty auto-detects the first workspace folder. | `IBIS_WORKSPACE` |
| `ibis.runtime.autoAttach` | `false` | Only in explicit `spawn` mode: probe the URLs below and attach to a reachable personal runtime instead of spawning. | — |
| `ibis.runtime.autoAttachProbeUrls` | `["http://127.0.0.1:3030/mcp", "http://127.0.0.1:3031/mcp"]` | Loopback MCP endpoints probed in order when auto-attach is on. | — |

### MCP and LM bridge

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `ibis.mcp.registerProvider` | `true` | Registers the MCP server-definition provider so agent mode can start the server. Reload to apply. | — |
| `ibis.lmBridge.enabled` | `true` | Starts the loopback IDE LM bridge when `vscode.lm` is available. Reload to apply. | — |
| `ibis.lmBridge.port` | `39201` | Bridge loopback port. Reload to apply. | `IBIS_LM_BRIDGE_PORT` |
| `ibis.lmBridge.url` | *(empty)* | Explicit bridge URL exported to the runtime; empty derives `http://127.0.0.1:<port>`. | `IBIS_LM_BRIDGE_URL` |
| `ibis.lmBridge.modelFamily` | *(empty)* | `vscode.lm` model family filter (e.g. `gpt-4o`); empty uses the first available model. | — |

### Settings UI

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `ibis.settings.url` | *(empty)* | URL opened by **Ibis Assistant: Open Settings**; empty probes the list below then falls back to MCP `settings_open_ui`. | `IBIS_SETTINGS_URL` |
| `ibis.settings.probeUrls` | `["http://127.0.0.1:3030/settings/", "http://127.0.0.1:3031/settings/"]` | Personal Settings UI endpoints probed in order. | — |
| `ibis.settings.probeTimeoutMs` | `800` | Probe timeout in milliseconds for settings/auto-attach HTTP checks. | — |

### Identity and claims

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `ibis.identity.actor` | *(empty)* | Actor identity sent as the `X-Ibis-Actor` header on every attach/daemon MCP request (initialize included). Attributes shared rows on team servers and drives the curation **Update Content** guard; empty stays the anonymous local actor (`unknown`). Trusted from loopback peers only. | `IBIS_ACTOR` |
| `ibis.claims.personalMode` | `stub` | Personal-claims policy forwarded to the runtime as `IBIS_CLAIMS_PERSONAL_MODE` for spawned children and daemons started via **Ibis Assistant: Start Daemon** (only when explicitly configured). `stub` renders title-only claim files in `.ibis/claims/`, `db_only` keeps personal claims in the database. | `IBIS_CLAIMS_PERSONAL_MODE` |

### Memory and status

| Setting | Default | Description |
|---------|---------|-------------|
| `ibis.memory.defaultKind` | `ask` | Kind used by **Ibis Assistant: Remember**: `ask` keeps the quick pick, otherwise `decision` / `fix` / `rule` / `constraint` / `gotcha` / `design` / `fact` writes directly. |
| `ibis.memory.defaultVisibility` | `team` | `visibility` for `memory_add`: sharing-first `team`, or `personal` (owner-only). |
| `ibis.memory.statusFilter` | `current` | Status filter for the Memory tree kind browse and search and the Workstreams view children: `current` shows only still-valid memories, `all` includes superseded/archived/stale rows. Toggle from the view title. |
| `ibis.workingState.useWorkspaceProject` | `true` | Sends the first workspace folder name as `project_name` to `set_working_state`. |
| `ibis.status.outputMode` | `toast` | **Ibis Assistant: Status** destination: `toast`, `output`, or `both`. |
| `ibis.debug.verboseLogging` | `false` | Logs verbose activation and backend-resolution details to the output channel. |

## MCP integration

The extension contributes the `ibis` MCP server-definition provider. On VS Code 1.144+ the server appears in the MCP servers list and is started on demand in agent mode. The definition is recomputed from live settings on every request, so changes to runtime settings apply to the next session without a reload (LM bridge port and provider registration need a window reload).

- **Attach (default `auto` + explicit `attach`):** VS Code connects to the personal daemon endpoint (Streamable HTTP, default `http://127.0.0.1:3030/mcp`); the MCP definition advertises the HTTP server to agent mode. Extension-issued calls ride the endpoint with the `X-Ibis-Actor` header when `ibis.identity.actor` (or `IBIS_ACTOR`) is set — [docs/team_server.md](https://github.com/fatterdev/ibis-assistant/blob/main/docs/team_server.md) §4 documents the loopback trust model and the `actor` tool-argument fallback for agent-mode flows.
- **Spawn (explicit `spawn`):** VS Code runs `ibis -mode stdio -runtime-mode plugin` with `RUNTIME_MODE=plugin` and the resolved `IBIS_*` environment.

## IDE LM bridge

> **Deprecated since F0.5:** VS Code answers MCP `sampling/createMessage`
> directly, so the runtime reasons through the MCP session by default
> (`ai.reasoning.provider=ide_sampling` in plugin mode). The bridge stays
> functional only as the middle fallback of the `sampling → lm_bridge →
> ollama` chain.

When enabled, the extension registers a `vscode.lm` handler and listens on `127.0.0.1:<ibis.lmBridge.port>` (`/health`, `/v1/generate`). The runtime reaches it via `IBIS_LM_BRIDGE_URL` (exported automatically) when `ai.reasoning.use_ide_provided_models` is on. Embeddings remain Ollama `nomic-embed-text`; the context-only fallback still applies when the bridge is down.

## Runtime uniqueness

| Setup | Unique process / DB? |
|-------|----------------------|
| VS Code only, default `auto` | **Yes** — one personal daemon (start it with **Start Daemon** / `ibis start` if offline) |
| Cursor plugin + VS Code, both default `auto` | **Yes** — both attach to the same personal daemon (Cursor via its http variant / `IBIS_MCP_URL`) |
| Explicit spawn + agent plugin child | **No** — two children; never share one SurrealDB silently — switch the plugin `mcp.json` to the http variant (team setup, [docs/team_server.md](https://github.com/fatterdev/ibis-assistant/blob/main/docs/team_server.md)) |
| Both attach to a personal daemon (`ibis run`/`ibis start` + `attach` mode) | **Yes** — one personal daemon, the recommended shared setup |

See [docs/runtime_modes.md](https://github.com/fatterdev/ibis-assistant/blob/main/docs/runtime_modes.md) and [docs/ide_host_and_lm_bridge.md](https://github.com/fatterdev/ibis-assistant/blob/main/docs/ide_host_and_lm_bridge.md).

## Typed memory

Memory is sharing-first, stored in the SurrealDB owned by the attached runtime: `memory_add` / **Ibis Assistant: Remember** for durable notes, `set_working_state` / **Ibis Assistant: Set working state** for branch scratchpads, `load_session_context` once per session, `capture_observation` sparingly, `memory_consolidate` to distill. With isolated plugin children use `memory_export` / `memory_import`, or attach both IDEs to one personal daemon. Operator guide: [docs/memory_layer.md](https://github.com/fatterdev/ibis-assistant/blob/main/docs/memory_layer.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `ibis is not recognized` / spawn fails | Put the binary on `PATH`, or set an absolute `ibis.runtime.binaryPath`. The binary is spawned without the repo as cwd. |
| Status bar shows **Ibis daemon offline** | Run **Ibis Assistant: Start Daemon** (or `ibis start` / `ibis run` in a terminal); adjust `ibis.runtime.mcpUrl` for non-default ports. |
| No Ibis MCP server in agent mode | Requires VS Code 1.144+; check **Ibis Assistant: Status** (`MCP provider: registered`) and reload after changing `ibis.mcp.registerProvider`. |
| `LM bridge not started` in the output channel | `vscode.lm` is unavailable, `ibis.lmBridge.enabled` is off, or the port is busy — change `ibis.lmBridge.port` and reload. |
| **Ibis Assistant: Open Settings** cannot reach the UI | Start the personal daemon (**Start Daemon** or `ibis run`) or let it fall back to the ephemeral loopback URL from `settings_open_ui`; adjust `ibis.settings.probeUrls` for non-default ports. |
| Two IDEs, memories not shared | Attach both to one personal daemon (default `auto` + `ibis start`); do not point two plugin children at one SurrealDB. |

## Documentation

- [Repository README](https://github.com/fatterdev/ibis-assistant/blob/main/README.md) — product overview and quick start
- [AI & Settings](https://github.com/fatterdev/ibis-assistant/blob/main/docs/ai_and_settings.md) — hybrid AI, cloud keys, wizard, Settings UI
- [Runtime modes](https://github.com/fatterdev/ibis-assistant/blob/main/docs/runtime_modes.md) — personal / plugin / server
- [IDE host & LM bridge](https://github.com/fatterdev/ibis-assistant/blob/main/docs/ide_host_and_lm_bridge.md)
- [Memory layer](https://github.com/fatterdev/ibis-assistant/blob/main/docs/memory_layer.md)

## License

Apache-2.0 — see [LICENSE](https://github.com/fatterdev/ibis-assistant/blob/main/LICENSE) and [NOTICE](https://github.com/fatterdev/ibis-assistant/blob/main/NOTICE).
