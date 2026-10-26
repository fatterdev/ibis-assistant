<p align="center">
  <img src="https://raw.githubusercontent.com/fatterdev/ibis-assistant/master/vscode-extension/assets/icon.png" alt="Ibis Assistant" width="128" height="128">
</p>

# Ibis Assistant for VS Code

**Living project memory over MCP.** Ibis turns a real codebase — Git history, polyglot AST structure, logs, tickets and PRs — into a queryable knowledge graph in SurrealDB, then keeps team decisions and session state on that same graph.

This extension is the VS Code packaging: it spawns an isolated Ibis Assistant stdio child or attaches to a shared personal runtime, registers the MCP server for VS Code agent mode, and bridges IDE-provided language models back to the runtime. **Cursor remains the primary IDE packaging** ([`cursor-plugin/`](https://github.com/fatterdev/ibis-assistant/tree/master/cursor-plugin)).

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/fatterdev/ibis-assistant/blob/master/LICENSE)
[![VS Code](https://img.shields.io/badge/VS%20Code-%E2%89%A51.101-007ACC.svg)](https://code.visualstudio.com/)

## What you get

- **MCP server provider** — VS Code's MCP integration (`registerMcpServerDefinitionProvider`) starts `ibis-assistant -mode stdio -runtime-mode plugin` automatically, or attaches to a shared runtime over Streamable HTTP.
- **IDE LM bridge** — when `vscode.lm` is available, the runtime can use IDE-provided models (`use_ide_provided_models`); embeddings stay on Ollama.
- **Browser Settings** — **Ibis: Open Settings** reuses the personal `/settings/` UI or falls back to an ephemeral loopback URL with a one-shot token via MCP `settings_open_ui`.
- **Memory commands** — **Ibis: Remember** (`memory_add`) and **Ibis: Set working state** (`set_working_state`) straight from the Command Palette.
- **Diagnostics** — **Ibis: Status** and an **Ibis Assistant** output channel reporting backend, runtime uniqueness, and bridge state.
- **Extensive settings** — every runtime knob exposed as VS Code settings, with environment-variable fallbacks (see tables below).

## Requirements

- VS Code **1.101+** (the MCP server-definition provider API). Other commands work on older versions, but MCP auto-registration does not.
- The `ibis-assistant` binary on `PATH` (or an absolute path in `ibis.runtime.binaryPath`). Download from [Releases](https://github.com/fatterdev/ibis-assistant/releases) — no Rust toolchain required for normal use.

## Install

Local VSIX (recommended while the Marketplace publishing is pending):

```powershell
cd vscode-extension
npm install
npx --yes @vscode/vsce package --no-dependencies
code --install-extension .\ibis-assistant-1.0.0.vsix
```

For iterative development open this folder in an **Extension Development Host** (`F5`) instead of `code --install-extension` on the source directory (that CLI only accepts an extension id or a `.vsix` file).

## Quick start

1. Put `ibis-assistant` on `PATH` (Windows: `ibis-assistant.exe`) and run it once so the wizard writes `config.json` (`config fast`, or `config full` for DB/SMTP/ticketing).
2. Reload VS Code. The extension starts an isolated plugin child bound to the opened folder and registers the Ibis Assistant MCP server.
3. Ask your agent about the repo, or call `init_project` once in a Git checkout.
4. Optional — share **one** brain between Cursor and VS Code:

   ```powershell
   ibis-assistant run                        # personal daemon on 127.0.0.1:3030
   # then set in VS Code settings:
   #   ibis.runtime.mode = attach
   #   ibis.runtime.mcpUrl = http://127.0.0.1:3030/mcp
   ```

   Equivalently, set the `IBIS_MCP_URL` environment variable while leaving `ibis.runtime.mode` at its default `spawn`.

## Commands

| Command | What it does |
|---------|--------------|
| **Ibis: Open Settings** | Opens the Ibis browser Settings UI (hybrid AI, cloud keys, local models, paths, Memory browser). Probes personal HTTP first, then MCP `settings_open_ui`. |
| **Ibis: Open Extension Settings (VS Code)** | Opens VS Code Settings filtered to this extension (`@ext:fatterdev.ibis-assistant`). |
| **Ibis: Show Output** | Reveals the Ibis Assistant output channel with activation and status logs. |
| **Ibis: Status** | Reports the effective backend (spawn vs attach), plugin data dir, bound workspace, LM bridge, and memory defaults. Output destination follows `ibis.status.outputMode`. |
| **Ibis: Remember** | Typed durable memory (`memory_add`) with kind quick pick and configurable default kind/visibility. |
| **Ibis: Set working state** | Branch/session scratchpad (`set_working_state`), optionally with the workspace folder as project name. |

## Settings

All settings live under `ibis.*` in VS Code Settings. Precedence is **explicit VS Code setting > environment variable > default**; empty strings fall back.

### Runtime and connection

| Setting | Default | Description | Env equivalent |
|---------|---------|-------------|----------------|
| `ibis.runtime.mode` | `spawn` | `spawn` an isolated `ibis-assistant -mode stdio -runtime-mode plugin` child, or `attach` to a shared Streamable HTTP runtime. | — |
| `ibis.runtime.mcpUrl` | `http://127.0.0.1:3030/mcp` | Endpoint used when mode is `attach`. | `IBIS_MCP_URL` (honored while mode is untouched) |
| `ibis.runtime.binaryPath` | `ibis-assistant` | Command or absolute path of the binary used when spawning. | `IBIS_BIN` |
| `ibis.runtime.dataDir` | *(empty)* | Overrides the plugin data directory (default `%LOCALAPPDATA%\ibis-assistant\plugin` or platform equivalent). | `IBIS_DATA_DIR` |
| `ibis.runtime.runtimeMode` | `plugin` | Passed to `-runtime-mode`. Keep `plugin` for IDE-owned children. | `RUNTIME_MODE` (forced for the child) |
| `ibis.runtime.extraArgs` | `[]` | Extra CLI arguments appended after the standard spawn arguments. | — |
| `ibis.runtime.workspaceOverride` | *(empty)* | Explicit opened-folder path; empty auto-detects the first workspace folder. | `IBIS_WORKSPACE` |
| `ibis.runtime.autoAttach` | `false` | Probe the URLs below and attach to a reachable personal runtime instead of spawning. | — |
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
| `ibis.settings.url` | *(empty)* | URL opened by **Ibis: Open Settings**; empty probes the list below then falls back to MCP `settings_open_ui`. | `IBIS_SETTINGS_URL` |
| `ibis.settings.probeUrls` | `["http://127.0.0.1:3030/settings/", "http://127.0.0.1:3031/settings/"]` | Personal Settings UI endpoints probed in order. | — |
| `ibis.settings.probeTimeoutMs` | `800` | Probe timeout in milliseconds for settings/auto-attach HTTP checks. | — |

### Memory and status

| Setting | Default | Description |
|---------|---------|-------------|
| `ibis.memory.defaultKind` | `ask` | Kind used by **Ibis: Remember**: `ask` keeps the quick pick, otherwise `decision` / `fix` / `rule` / `constraint` / `gotcha` / `design` / `fact` writes directly. |
| `ibis.memory.defaultVisibility` | `team` | `visibility` for `memory_add`: sharing-first `team`, or `personal` (owner-only). |
| `ibis.workingState.useWorkspaceProject` | `true` | Sends the first workspace folder name as `project_name` to `set_working_state`. |
| `ibis.status.outputMode` | `toast` | **Ibis: Status** destination: `toast`, `output`, or `both`. |
| `ibis.debug.verboseLogging` | `false` | Logs verbose activation and backend-resolution details to the output channel. |

## MCP integration

The extension contributes the `ibis-assistant` MCP server-definition provider. On VS Code 1.101+ the server appears in the MCP servers list and is started on demand in agent mode. The definition is recomputed from live settings on every request, so changes to runtime settings apply to the next session without a reload (LM bridge port and provider registration need a window reload).

- **Spawn (default):** VS Code runs `ibis-assistant -mode stdio -runtime-mode plugin` with `RUNTIME_MODE=plugin` and the resolved `IBIS_*` environment.
- **Attach:** VS Code connects to `ibis.runtime.mcpUrl` (Streamable HTTP, e.g. the personal daemon's `/mcp`).

## IDE LM bridge

When enabled, the extension registers a `vscode.lm` handler and listens on `127.0.0.1:<ibis.lmBridge.port>` (`/health`, `/v1/generate`). The runtime reaches it via `IBIS_LM_BRIDGE_URL` (exported automatically) when `ai.reasoning.use_ide_provided_models` is on. Embeddings remain Ollama `nomic-embed-text`; the context-only fallback still applies when the bridge is down.

## Runtime uniqueness

| Setup | Unique process / DB? |
|-------|----------------------|
| VS Code only, default spawn | **Yes** — one stdio child, plugin data dir |
| Cursor plugin + VS Code, both default spawn | **No** — two children; never share one SurrealDB silently |
| Both attach to a personal daemon (`ibis-assistant run` + `ibis.runtime.mode=attach`) | **Yes** — one personal daemon, the recommended shared setup |

See [docs/runtime_modes.md](https://github.com/fatterdev/ibis-assistant/blob/master/docs/runtime_modes.md) and [docs/ide_host_and_lm_bridge.md](https://github.com/fatterdev/ibis-assistant/blob/master/docs/ide_host_and_lm_bridge.md).

## Typed memory

Memory is sharing-first, stored in the SurrealDB owned by the attached runtime: `memory_add` / **Ibis: Remember** for durable notes, `set_working_state` / **Ibis: Set working state** for branch scratchpads, `load_session_context` once per session, `capture_observation` sparingly, `memory_consolidate` to distill. With isolated plugin children use `memory_export` / `memory_import`, or attach both IDEs to one personal daemon. Operator guide: [docs/memory_layer.md](https://github.com/fatterdev/ibis-assistant/blob/master/docs/memory_layer.md).

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `ibis-assistant is not recognized` / spawn fails | Put the binary on `PATH`, or set an absolute `ibis.runtime.binaryPath`. The binary is spawned without the repo as cwd. |
| No Ibis MCP server in agent mode | Requires VS Code 1.101+; check **Ibis: Status** (`MCP provider: registered`) and reload after changing `ibis.mcp.registerProvider`. |
| `LM bridge not started` in the output channel | `vscode.lm` is unavailable, `ibis.lmBridge.enabled` is off, or the port is busy — change `ibis.lmBridge.port` and reload. |
| **Ibis: Open Settings** cannot reach the UI | Start a personal daemon (`ibis-assistant run`) or let it fall back to the ephemeral loopback URL from `settings_open_ui`; adjust `ibis.settings.probeUrls` for non-default ports. |
| Two IDEs, memories not shared | Attach both to one personal daemon (`ibis.runtime.mode=attach` + `ibis-assistant run`); do not point two plugin children at one SurrealDB. |

## Documentation

- [Repository README](https://github.com/fatterdev/ibis-assistant/blob/master/README.md) — product overview and quick start
- [AI & Settings](https://github.com/fatterdev/ibis-assistant/blob/master/docs/ai_and_settings.md) — hybrid AI, cloud keys, wizard, Settings UI
- [Runtime modes](https://github.com/fatterdev/ibis-assistant/blob/master/docs/runtime_modes.md) — personal / plugin / server
- [IDE host & LM bridge](https://github.com/fatterdev/ibis-assistant/blob/master/docs/ide_host_and_lm_bridge.md)
- [Memory layer](https://github.com/fatterdev/ibis-assistant/blob/master/docs/memory_layer.md)
- [Cursor plugin](https://github.com/fatterdev/ibis-assistant/tree/master/cursor-plugin) — primary IDE packaging

## License

Apache-2.0 — see [LICENSE](https://github.com/fatterdev/ibis-assistant/blob/master/LICENSE) and [NOTICE](https://github.com/fatterdev/ibis-assistant/blob/master/NOTICE).
