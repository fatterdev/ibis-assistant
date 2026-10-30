<p align="center">
  <img src="https://raw.githubusercontent.com/fatterdev/threski-studio/master/vscode-extension/assets/icon.png" alt="Threski Studio" width="128" height="128">
</p>

# Threski Studio for VS Code

**Living project memory over MCP.** Threski turns a real codebase — Git history, polyglot AST structure, logs, tickets and PRs — into a queryable knowledge graph in SurrealDB, then keeps team decisions and session state on that same graph.

This extension is the VS Code packaging: it spawns an isolated Threski Studio stdio child or attaches to a shared personal runtime, registers the MCP server for VS Code agent mode, and bridges IDE-provided language models back to the runtime.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/fatterdev/threski-studio/blob/master/LICENSE)
[![VS Code](https://img.shields.io/badge/VS%20Code-%E2%89%A51.101-007ACC.svg)](https://code.visualstudio.com/)

## What you get

- **MCP server provider** — VS Code's MCP integration (`registerMcpServerDefinitionProvider`) starts `threski -mode stdio -runtime-mode plugin` automatically, or attaches to a shared runtime over Streamable HTTP.
- **IDE LM bridge** — when `vscode.lm` is available, the runtime can use IDE-provided models (`use_ide_provided_models`); embeddings stay on Ollama.
- **Browser Settings** — **Threski: Open Settings** reuses the personal `/settings/` UI or falls back to an ephemeral loopback URL with a one-shot token via MCP `settings_open_ui`.
- **Memory commands** — **Threski: Remember** (`memory_add`) and **Threski: Set working state** (`set_working_state`) straight from the Command Palette.
- **Diagnostics** — **Threski: Status** and an **Threski Studio** output channel reporting backend, runtime uniqueness, and bridge state.
- **Extensive settings** — every runtime knob exposed as VS Code settings, with environment-variable fallbacks (see tables below).

## Requirements

- VS Code **1.101+** (the MCP server-definition provider API). Other commands work on older versions, but MCP auto-registration does not.
- The `threski` binary on `PATH` (or an absolute path in `threskiStudio.runtime.binaryPath`). Download from [Releases](https://github.com/fatterdev/threski-studio/releases) — no Rust toolchain required for normal use.

## Install

Local VSIX (recommended while the Marketplace publishing is pending):

```powershell
cd vscode-extension
npm install
npx --yes @vscode/vsce package --no-dependencies
code --install-extension .\threski-1.0.0.vsix
```

For iterative development open this folder in an **Extension Development Host** (`F5`) instead of `code --install-extension` on the source directory (that CLI only accepts an extension id or a `.vsix` file).

## Quick start

1. Put `threski` on `PATH` (Windows: `threski.exe`) and run it once so the wizard writes `config.json` (`config fast`, or `config full` for DB/SMTP/ticketing).
2. Reload VS Code. The extension starts an isolated plugin child bound to the opened folder and registers the Threski Studio MCP server.
3. Ask your agent about the repo, or call `init_project` once in a Git checkout.
4. Optional — share **one** brain between Cursor and VS Code:

   ```powershell
   threski run                        # personal daemon on 127.0.0.1:3030
   # then set in VS Code settings:
   #   threskiStudio.runtime.mode = attach
   #   threskiStudio.runtime.mcpUrl = http://127.0.0.1:3030/mcp
   ```

   Equivalently, set the `THRESKI_MCP_URL` environment variable while leaving `threskiStudio.runtime.mode` at its default `spawn`.

## Commands

| Command | What it does |
|---------|--------------|
| **Threski: Open Settings** | Opens the Threski browser Settings UI (hybrid AI, cloud keys, local models, paths, Memory browser). Probes personal HTTP first, then MCP `settings_open_ui`. |
| **Threski: Open Extension Settings (VS Code)** | Opens VS Code Settings filtered to this extension (`@ext:fatterdev.threski`). |
| **Threski: Show Output** | Reveals the Threski Studio output channel with activation and status logs. |
| **Threski: Status** | Reports the effective backend (spawn vs attach), plugin data dir, bound workspace, LM bridge, and memory defaults. Output destination follows `threskiStudio.status.outputMode`. |
| **Threski: Remember** | Typed durable memory (`memory_add`) with kind quick pick and configurable default kind/visibility. |
| **Threski: Set working state** | Branch/session scratchpad (`set_working_state`), optionally with the workspace folder as project name. |

## Settings

All settings live under `threski.*` in VS Code Settings. Precedence is **explicit VS Code setting > environment variable > default**; empty strings fall back.

### Runtime and connection

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `threskiStudio.runtime.mode` | `spawn` | `spawn` an isolated `threski -mode stdio -runtime-mode plugin` child, or `attach` to a shared Streamable HTTP runtime. | — |
| `threskiStudio.runtime.mcpUrl` | `http://127.0.0.1:3030/mcp` | Endpoint used when mode is `attach`. | `THRESKI_MCP_URL` (honored while mode is untouched) |
| `threskiStudio.runtime.binaryPath` | `threski` | Command or absolute path of the binary used when spawning. | `THRESKI_BIN` |
| `threskiStudio.runtime.dataDir` | *(empty)* | Overrides the plugin data directory (default `%LOCALAPPDATA%\threski\plugin` or platform equivalent). | `THRESKI_DATA_DIR` |
| `threskiStudio.runtime.runtimeMode` | `plugin` | Passed to `-runtime-mode`. Keep `plugin` for IDE-owned children. | `RUNTIME_MODE` (forced for the child) |
| `threskiStudio.runtime.extraArgs` | `[]` | Extra CLI arguments appended after the standard spawn arguments. | — |
| `threskiStudio.runtime.workspaceOverride` | *(empty)* | Explicit opened-folder path; empty auto-detects the first workspace folder. | `THRESKI_WORKSPACE` |
| `threskiStudio.runtime.autoAttach` | `false` | Probe the URLs below and attach to a reachable personal runtime instead of spawning. | — |
| `threskiStudio.runtime.autoAttachProbeUrls` | `["http://127.0.0.1:3030/mcp", "http://127.0.0.1:3031/mcp"]` | Loopback MCP endpoints probed in order when auto-attach is on. | — |

### MCP and LM bridge

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `threskiStudio.mcp.registerProvider` | `true` | Registers the MCP server-definition provider so agent mode can start the server. Reload to apply. | — |
| `threskiStudio.lmBridge.enabled` | `true` | Starts the loopback IDE LM bridge when `vscode.lm` is available. Reload to apply. | — |
| `threskiStudio.lmBridge.port` | `39201` | Bridge loopback port. Reload to apply. | `THRESKI_LM_BRIDGE_PORT` |
| `threskiStudio.lmBridge.url` | *(empty)* | Explicit bridge URL exported to the runtime; empty derives `http://127.0.0.1:<port>`. | `THRESKI_LM_BRIDGE_URL` |
| `threskiStudio.lmBridge.modelFamily` | *(empty)* | `vscode.lm` model family filter (e.g. `gpt-4o`); empty uses the first available model. | — |

### Settings UI

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `threskiStudio.settings.url` | *(empty)* | URL opened by **Threski: Open Settings**; empty probes the list below then falls back to MCP `settings_open_ui`. | `THRESKI_SETTINGS_URL` |
| `threskiStudio.settings.probeUrls` | `["http://127.0.0.1:3030/settings/", "http://127.0.0.1:3031/settings/"]` | Personal Settings UI endpoints probed in order. | — |
| `threskiStudio.settings.probeTimeoutMs` | `800` | Probe timeout in milliseconds for settings/auto-attach HTTP checks. | — |

### Memory and status

| Setting | Default | Description |
|---------|---------|-------------|
| `threskiStudio.memory.defaultKind` | `ask` | Kind used by **Threski: Remember**: `ask` keeps the quick pick, otherwise `decision` / `fix` / `rule` / `constraint` / `gotcha` / `design` / `fact` writes directly. |
| `threskiStudio.memory.defaultVisibility` | `team` | `visibility` for `memory_add`: sharing-first `team`, or `personal` (owner-only). |
| `threskiStudio.workingState.useWorkspaceProject` | `true` | Sends the first workspace folder name as `project_name` to `set_working_state`. |
| `threskiStudio.status.outputMode` | `toast` | **Threski: Status** destination: `toast`, `output`, or `both`. |
| `threskiStudio.debug.verboseLogging` | `false` | Logs verbose activation and backend-resolution details to the output channel. |

## MCP integration

The extension contributes the `threski` MCP server-definition provider. On VS Code 1.101+ the server appears in the MCP servers list and is started on demand in agent mode. The definition is recomputed from live settings on every request, so changes to runtime settings apply to the next session without a reload (LM bridge port and provider registration need a window reload).

- **Spawn (default):** VS Code runs `threski -mode stdio -runtime-mode plugin` with `RUNTIME_MODE=plugin` and the resolved `THRESKI_*` environment.
- **Attach:** VS Code connects to `threskiStudio.runtime.mcpUrl` (Streamable HTTP, e.g. the personal daemon's `/mcp`).

## IDE LM bridge

> **Deprecated since F0.5:** VS Code answers MCP `sampling/createMessage`
> directly, so the runtime reasons through the MCP session by default
> (`ai.reasoning.provider=ide_sampling` in plugin mode). The bridge stays
> functional only as the middle fallback of the `sampling → lm_bridge →
> ollama` chain.

When enabled, the extension registers a `vscode.lm` handler and listens on `127.0.0.1:<threskiStudio.lmBridge.port>` (`/health`, `/v1/generate`). The runtime reaches it via `THRESKI_LM_BRIDGE_URL` (exported automatically) when `ai.reasoning.use_ide_provided_models` is on. Embeddings remain Ollama `nomic-embed-text`; the context-only fallback still applies when the bridge is down.

## Runtime uniqueness

| Setup | Unique process / DB? |
|-------|----------------------|
| VS Code only, default spawn | **Yes** — one stdio child, plugin data dir |
| Cursor plugin + VS Code, both default spawn | **No** — two children; never share one SurrealDB silently |
| Both attach to a personal daemon (`threski run` + `threskiStudio.runtime.mode=attach`) | **Yes** — one personal daemon, the recommended shared setup |

See [docs/runtime_modes.md](https://github.com/fatterdev/threski-studio/blob/master/docs/runtime_modes.md) and [docs/ide_host_and_lm_bridge.md](https://github.com/fatterdev/threski-studio/blob/master/docs/ide_host_and_lm_bridge.md).

## Typed memory

Memory is sharing-first, stored in the SurrealDB owned by the attached runtime: `memory_add` / **Threski: Remember** for durable notes, `set_working_state` / **Threski: Set working state** for branch scratchpads, `load_session_context` once per session, `capture_observation` sparingly, `memory_consolidate` to distill. With isolated plugin children use `memory_export` / `memory_import`, or attach both IDEs to one personal daemon. Operator guide: [docs/memory_layer.md](https://github.com/fatterdev/threski-studio/blob/master/docs/memory_layer.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `threski is not recognized` / spawn fails | Put the binary on `PATH`, or set an absolute `threskiStudio.runtime.binaryPath`. The binary is spawned without the repo as cwd. |
| No Threski MCP server in agent mode | Requires VS Code 1.101+; check **Threski: Status** (`MCP provider: registered`) and reload after changing `threskiStudio.mcp.registerProvider`. |
| `LM bridge not started` in the output channel | `vscode.lm` is unavailable, `threskiStudio.lmBridge.enabled` is off, or the port is busy — change `threskiStudio.lmBridge.port` and reload. |
| **Threski: Open Settings** cannot reach the UI | Start a personal daemon (`threski run`) or let it fall back to the ephemeral loopback URL from `settings_open_ui`; adjust `threskiStudio.settings.probeUrls` for non-default ports. |
| Two IDEs, memories not shared | Attach both to one personal daemon (`threskiStudio.runtime.mode=attach` + `threski run`); do not point two plugin children at one SurrealDB. |

## Documentation

- [Repository README](https://github.com/fatterdev/threski-studio/blob/master/README.md) — product overview and quick start
- [AI & Settings](https://github.com/fatterdev/threski-studio/blob/master/docs/ai_and_settings.md) — hybrid AI, cloud keys, wizard, Settings UI
- [Runtime modes](https://github.com/fatterdev/threski-studio/blob/master/docs/runtime_modes.md) — personal / plugin / server
- [IDE host & LM bridge](https://github.com/fatterdev/threski-studio/blob/master/docs/ide_host_and_lm_bridge.md)
- [Memory layer](https://github.com/fatterdev/threski-studio/blob/master/docs/memory_layer.md)

## License

Apache-2.0 — see [LICENSE](https://github.com/fatterdev/threski-studio/blob/master/LICENSE) and [NOTICE](https://github.com/fatterdev/threski-studio/blob/master/NOTICE).
