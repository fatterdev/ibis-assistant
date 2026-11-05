# Changelog

All notable changes to the **Ibis Assistant for VS Code** extension are documented here.
The runtime changelog lives in the [repository root](https://github.com/fatterdev/ibis-assistant/blob/main/CHANGELOG.md).

## [Unreleased]

- **Actor identity + personal-claims knob (F4-A5, 1.0.15)** — new setting
  `ibis.identity.actor` (or the `IBIS_ACTOR` environment variable): in
  attach/daemon mode every MCP request the extension issues — initialize
  included — carries the `X-Ibis-Actor` header, so curation writes attribute
  correctly on shared servers and the **Update Content** guard (F4-A4) now
  matches the very identity the runtime sees. The ide-host attach path and
  its http stdio bridge forward the header; a LAN team server still
  attributes remote clients via the tools' `actor` argument (the header is
  trusted from loopback peers only). New setting `ibis.claims.personalMode`
  (`stub`, the default, | `db_only`) is forwarded to the runtime as
  `IBIS_CLAIMS_PERSONAL_MODE` for spawned children and daemons started with
  **Ibis Assistant: Start Daemon** (only when explicitly configured, so a
  runtime `config.json` choice survives; a daemon started elsewhere keeps
  its boot-time policy). **Status** reports the effective actor and claims
  mode. Requires the F4-A5 runtime (the `IBIS_CLAIMS_PERSONAL_MODE`
  override).
- **Workstreams view + inline curation menu (F4-A4, 1.0.14)** — the
  activity-bar container grows a second tree, **Workstreams**: roots come
  from MCP `memory_workstreams` (name + current-member count, ordered by
  most recently touched), children from `memory_list(workstream=…)` under
  the shared `ibis.memory.statusFilter` (both view titles show `all` while
  the unfiltered browse is active). Entry rows of both views carry an inline
  curation menu: **Share** to team (`memory_share`), **Archive**
  (`memory_update status=archived`, confirmed), **Supersede…**
  (`memory_add supersedes=…`, kind/visibility prefilled — the governance
  route for rows you cannot write), and **Update Content…**
  (`memory_update content`, re-embeds; offered on owner-matching rows, with
  a "create a superseding memory instead" fallback on governance errors).
  Every action toasts the compact result and logs the full tool output; the
  runtime's rejections (personal-row ownership) surface verbatim. Git-owned
  claims (the `store` marker projected by `memory_list`) are excluded from
  the menu entirely with the badge `git` and the tooltip "repo is source of
  truth — supersede via `.ibis/claims/` commit". Both trees **refresh on
  focus** — kind badges and expanded
  parents re-derive from embedding-free `memory_list` calls; search results
  are never auto-refreshed (rerun the search instead). Requires the F4-A3
  runtime plus the `store` marker in the `memory_list` projection.
- **Memory status filter (F4-A2, 1.0.13)** — the Memory tree browses only
  still-valid memories by default: new setting `ibis.memory.statusFilter`
  (`current`, the default, or `all`) is passed as `status` to MCP
  `memory_list` (kind browse) and `memory_search`. **Ibis Assistant: Toggle
  Memory Status Filter** (view-title filter button) flips the setting; while
  the unfiltered browse is active the view title shows `all`. Entry rows
  keep the status badge (`superseded` / `archived` / `stale`) in `all`
  mode. Requires the F4-A2 runtime (the `all` status marker on
  `memory_list`/`memory_search`).
- **Daemon attach by default (F4-A1)** — `ibis.runtime.mode` gains `auto`
  (new default): the extension attaches to the always-on personal daemon
  (`http://127.0.0.1:3030/mcp`, overridable via `#ibis.runtime.mcpUrl#` /
  `IBIS_MCP_URL`) instead of spawning a plugin child per window. The status
  bar shows **Ibis attach** when the daemon is reachable and
  **Ibis daemon offline** when it is not; the new command
  **Ibis Assistant: Start Daemon** launches `ibis start` detached and
  re-probes the endpoint (falling back to a manual-start hint). Explicit
  `spawn` keeps the per-window isolated child bit-for-bit; the MCP
  server-definition provider now advertises the HTTP definition of the
  daemon. `warnDualPluginChildren` targets the plugin-spawn + daemon
  combination with the documented team fix (plugin `mcp.json` http variant).
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
