# IDE and client integrations

**Date:** 2026-11-07
**Status:** F3 positioning docs — verified against the repo surfaces listed below

Every surface talks to the same Ibis MCP server: the full tool registry (58
tools) over Streamable HTTP at `/mcp`, or the same registry through a stdio
child (`ibis -mode stdio -runtime-mode plugin`). What differs is how a surface
connects, what extra UI it gets, and how it attributes shared memory rows.

## Integration matrix

| Surface | Connection mode | What it gets | Team server attach | Actor identity | Doc |
|---|---|---|---|---|---|
| **VS Code extension** (`vscode-extension/`) | `ibis.runtime.mode`: `auto` (default — always-on personal daemon at `http://127.0.0.1:3030/mcp`, Start/Stop Daemon UX) · `attach` (custom `ibis.runtime.mcpUrl`) · `spawn` (isolated stdio child, back-compat) | Full 58-tool MCP registry; **memory tree + Workstreams tree views** with inline curation (Share / Archive / Supersede / Update, owner-guarded); session-contract chat instructions; Settings UI | Yes: `attach` + `ibis.runtime.mcpUrl: http://<server>:3030/mcp` | Setting `ibis.identity.actor` → `X-Ibis-Actor` header on every attach request; on a LAN server the header is ignored (loopback-only trust), so agent flows also pass the tools' `actor` argument | [vscode-extension/README.md](../vscode-extension/README.md) · [team_server.md §5](team_server.md) |
| **Cursor plugin** (`cursor-plugin/`) | stdio spawn `ibis -mode stdio -runtime-mode plugin` from `mcp.json` (default); attach via `IBIS_MCP_URL` (shared personal runtime) or the mcp.json http variant | Full MCP registry through the spawned/attached runtime; 4 commands (Open Settings, Status, Remember, Set working state); readiness rule + skill; session hooks | Yes: switch `mcp.json` to the http variant `{ "url": "http://<server>:3030/mcp" }` (also printed by the discovery document on `GET /`) | No client header — the agent passes the tools' `actor` argument (the header is honored from loopback peers only) | [cursor-plugin/README.md](../cursor-plugin/README.md) · [team_server.md §5](team_server.md) |
| **Copilot CLI / chat plugin** (`plugin/`, Agent Plugins 1.0) | `plugin/mcp.json`: stdio spawn (default, daemon-less) or http variant | Skills (`/ibis usage`, readiness, status, remember, set-working-state, plus experimental `ibis-sync-copilot-memory`); `sessionStart` / `agentStop` hooks; `ibis-forensics` / `ibis-curator` agents; `nightly-ingest` / `weekly-consolidate` automations | Yes: `plugin/mcp.json` → the http variant above | Tool `actor` argument from the agent harness; no client header | [plugin/README.md](../plugin/README.md) · [team_server.md §5](team_server.md) |
| **MCP App — "Ibis Assistant Memory Curator"** | Same Streamable HTTP `/mcp`; served by the runtime as the SEP-1865 app resource `ui://ibis/memory-curator` | Interactive view over typed memory rendered inline by MCP Apps-capable clients (experimental in VS Code chat, `chat.mcp.apps.enabled`); the text tool result is always present (guaranteed fallback) | Inherits the attach mode of the client that renders it | Inherited from that client | Served by `crates/ibis-httpserver/src/curator.rs` (no operator doc yet) |
| **Claude Desktop** | stdio via the bundled `mcp-bridge` binary, forwarding to Streamable HTTP | Full MCP registry (the bridge is transport-only) | Yes: `mcp-bridge -url http://<server>:3030/mcp` | Tool `actor` argument | [CLAUDE_INTEGRATION.md](CLAUDE_INTEGRATION.md) |
| **Plain CLI** (`ibis memory …`, `ibis ask …`) | Resolves `127.0.0.1:<port>` from the local `config.json` | Memory and graph commands without an IDE | Server host itself only — pointing the CLI at a remote team server is a documented gap, not a supported path | `IBIS_ACTOR` env / `actor` argument | [team_server.md §5](team_server.md) |

Notes on what is verified where:

- The **Copilot CLI** harness is not separately documented in-repo: the
  `plugin/` package is Agent Plugins 1.0 and its install/verification steps
  target VS Code chat plugins; the hooks were checked against the GitHub
  Copilot hooks reference ([plugin/README.md](../plugin/README.md)). Treat the
  CLI wiring as expected-to-work, not documented here.
- The **actor header** is trusted only from loopback peers (`X-Forwarded-For`
  is ignored, so proxies cannot mint an actor); remote/LAN clients attribute
  through the tools' `actor` argument, and requests without one land as
  `unknown`. Personal-memory authorization stays team-only for remote peers.
- Client surfaces (stdio children, `spawn` mode) never start the consolidation
  loop; consolidation is daemon-only (`--consolidate` or
  `ai.consolidation.enabled`).

## Harness profiles (G20)

The wiring matrix above is committed as
[harness/profiles.json](../harness/profiles.json) (version 1) and pinned by
`crates/ibis-server/tests/harness_matrix_test.rs`: changing `plugin/mcp.json`,
`cursor-plugin/mcp.json` or the VSIX defaults without updating the descriptor
fails CI.

Per-host runtime behavior lives in `config.harness` (server-side): `match` on
`clientInfo.name`, `sampling: auto|never`, `hidden_tool_groups`
(`ticketing`, `settings`, `optimize`). Default = no hidden groups, sampling
`auto`, 58 tools unchanged. Hiding `settings` also hides `settings_open_ui`,
which the VS Code **Ibis: Open Settings** command, the Cursor Status/Settings
commands and the Copilot status skill call - do not apply that profile to
those surfaces. See
[ai_and_settings.md](ai_and_settings.md#harness-profiles-g20).

## Team server

One shared runtime per team store ([docs/team_server.md](team_server.md)):
`ibis serve --team --bind <addr> --data-dir <path> [--consolidate]`, or the
official compose image (`deploy/docker-compose.yml`). Onboarding per surface:

- **VS Code**: `ibis.runtime.mode: attach` +
  `ibis.runtime.mcpUrl: http://<server>:3030/mcp`; set `ibis.identity.actor`
  so the extension's own calls attribute on shared rows.
- **Cursor / Copilot plugin**: switch the respective `mcp.json` to the http
  variant (`{ "url": "http://<server>:3030/mcp" }`).
- **Claude Desktop**: `mcp-bridge -url http://<server>:3030/mcp`.
- **CLI**: works on the server host (loopback resolution from `config.json`).

Trust boundary: no authentication — anyone who can reach port 3030 can read
team memory and write with any actor. The `X-Ibis-Actor` header is honored
only from loopback peers; LAN clients send the tools' `actor` argument
instead. Publish the port on a trusted network only.

## Related

- [runtime_modes.md](runtime_modes.md) — personal / plugin / server processes
- [ide_host_and_lm_bridge.md](ide_host_and_lm_bridge.md) — ide-host spawn/attach internals
- [team_server.md](team_server.md) — team server runbook
