# Changelog

All notable changes to the **Ibis Assistant for VS Code** extension are documented here.
The runtime changelog lives in the [repository root](https://github.com/fatterdev/ibis-assistant/blob/main/CHANGELOG.md).

## [Unreleased]

- **Revert product naming to Ibis Assistant** — VSIX id `fatterdev.ibis-assistant`,
  settings prefix `ibis.*`, commands "Ibis Assistant: *", MCP server label
  `Ibis`, vendored `@ibis/ide-host`, fixed `@ext:fatterdev.ibis-assistant`
  settings filter.
- **Engine bump** — `engines.vscode` and `@types/vscode` raised to `^1.144.0`
  (VS Code 1.144, Oct 2026); packaging toolchain updated to `@vscode/vsce` 4.x
  (required for OIDC trusted publishing).
- **Status bar item** — right-aligned item showing the effective backend mode
  (spawn/attach) with LM-bridge and MCP-provider indicators; tooltip carries the
  full status, clicking runs **Ibis Assistant: Status**, and any `ibis.*`
  settings change refreshes it.
- **Memory tree view** — activity-bar "Ibis Assistant" view listing memory
  kinds (decision, fix, rule, constraint, gotcha, design, fact, ask) with
  entries via MCP `memory_search`, free-text semantic search, refresh, and a
  read-only detail editor filled from MCP `memory_expand`.
- **Remember Selection / Remember File** — editor and Explorer context-menu
  capture via MCP `memory_add` with git provenance (`source_path` +
  `sha256:<hex>` `source_hash`, same scheme as HEAD ingest) so memories go
  stale when the linked file changes; the selection variant records the
  `file:start-end` range in the content.
- **Session-contract chat instruction** — `ibis-session-contract`
  (`chatInstructions`) ships the session contract (load session context once,
  tool selection map, memory hygiene) so agent sessions follow it without
  duplicating the plugin's `usage` skill.
- **Onboarding walkthrough** — 4-step "Get Started with Ibis Assistant":
  install the binary, choose spawn/attach, browse memory, remember with
  provenance.
- **Publishing pipeline** — manual `publish-vscode.yml` GitHub Actions workflow:
  VS Marketplace via OIDC trusted publishing (`vsce publish --oidc`), Open VSX
  via `ovsx` (`OPEN_VSX_TOKEN` secret), and the MCP registry listing from root
  `server.json`; `vscode:mcp/install` badge in this README registers the Ibis
  MCP server in one click.

## [0.2.0] — 2026-10-29

- **Rebrand to Threski Studio** — VSIX id `fatterdev.threski-studio`, settings
  prefix `threskiStudio.*`, commands "Threski Studio: *", MCP server label
  `threski`, vendored `@threski/ide-host`. The extension version stays 1.0.0
  until first publish; no functional changes beyond naming.

## [1.0.0] — Initial release

VS Code packaging for the Threski Studio runtime.

- **MCP server provider** — registers `threski` for VS Code agent mode (VS Code 1.101+), spawning an isolated `threski -mode stdio -runtime-mode plugin` child or attaching to a shared personal runtime over Streamable HTTP.
- **IDE LM bridge** — wires `vscode.lm` into the runtime's loopback bridge (`THRESKI_LM_BRIDGE_URL`, default port 39201) with optional model-family filter; embeddings stay on Ollama.
- **Commands** — **Threski Studio: Open Settings**, **Threski Studio: Open Extension Settings (VS Code)**, **Threski Studio: Show Output**, **Threski Studio: Status**, **Threski Studio: Remember**, **Threski Studio: Set working state**.
- **Extensive settings** — 22 `threskiStudio.*` settings covering runtime (spawn/attach, binary, data dir, extra args, auto-attach probes), MCP provider, LM bridge, Settings UI probing, memory defaults, status output, and verbose logging, with environment-variable fallbacks (`THRESKI_MCP_URL`, `THRESKI_BIN`, `THRESKI_DATA_DIR`, `THRESKI_WORKSPACE`, `THRESKI_LM_BRIDGE_PORT`, `THRESKI_LM_BRIDGE_URL`, `THRESKI_SETTINGS_URL`).
- **Diagnostics** — dedicated "Threski Studio" output channel logging activation, backend resolution, and status.
- **Official icon** — the Threski logo (128×128) and gallery banner matching the Cursor plugin.
- **Packaging** — vendored `ide-host` (generated at package time via `vscode:prepublish`), correct `.vscodeignore`, and `vsce package --no-dependencies` so the VSIX is self-contained and installable.
