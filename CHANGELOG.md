# Changelog

All notable changes to Threski Studio are documented here.

## [Unreleased]

- **feat(naming): short /threski slash names across plugin and vsix** - the
  agent plugin id drops from `threski-studio` to `threski` and the five
  skills drop the `threski-` prefix (`usage`, `readiness`, `status`,
  `remember`, `set-working-state`), so VS Code renders them as
  `/threski usage`, `/threski status`, … instead of the doubled
  `/threski-studio threski-usage`; the duplicate `commands/threski-status.md`
  and `commands/threski-remember.md` are removed (the skills are the single
  slash surface, same names and descriptions). The VSIX MCP server label
  becomes `Threski` (naming map), so MCP prompts surface as short
  `/threski <prompt>` entries. The `sessionStart` hook fixes a leftover
  "Ibis Studio Status" reference; Track D profiles and docs follow the new
  skill paths. Verify: plugin.json/mcp.json/profiles.json parse, hook
  scripts run and emit `source: threski`, `vsce package` green.
- **fix(cli): accept go-style single-dash flags for ide spawns** - the spawn
  contract used by every IDE surface (ide-host, the VSIX MCP provider,
  `plugin/mcp.json`, `cursor-plugin/mcp.json`, evaluation profiles) passes
  Go-flavored long flags (`threski -mode stdio -runtime-mode plugin`), which
  clap rejected with a usage error and exit code 2 - the child died before
  the MCP `initialize` response ("child exited before MCP response id=1").
  The server launcher now normalizes the known server long flags from the
  single-dash spelling (and `-flag=value`) to clap's form before parsing;
  unknown dashed values pass through untouched. Verify: flag-parity unit
  tests (exact IDE spawn vectors, equals/double-dash forms, all nine flags),
  live stdio handshake with `-mode stdio -runtime-mode plugin` answering
  `id=1`.

- **feat(server): memory curator mcp app with text fallback** - the MCP App
  "Threski Studio Memory Curator" (SEP-1865, extension
  `io.modelcontextprotocol/ui`): the httpserver embeds a self-contained app
  document and serves it as the `ui://threski/memory-curator` resource with
  MIME `text/html;profile=mcp-app` and loopback-only CSP (the document
  carries the same policy as a meta tag and as `_meta.ui.csp` metadata; the
  app talks to the host through the MCP Apps postMessage bridge -
  `ui/initialize`, `ui/notifications/tool-input`, `ui/notifications/tool-result`
  - and never to the network). `memory_search`, `memory_expand` and
  `memory_add` now link the app on both layers: the tool objects declare
  `_meta.ui.resourceUri` (host-side prefetch) and their successful results
  carry the same `_meta` key, while the textual output stays untouched and
  guaranteed - hosts without MCP Apps support simply ignore the metadata and
  render the text (error results never carry the linkage). The app itself
  degrades to a static description panel when the bridge handshake never
  completes. Verify: F0.7 protocol gate over the in-memory duplex (listed
  with app MIME + CSP metadata, readable document, foreign `ui://` stays an
  unknown resource, curator linkage on exactly the three memory tools),
  unit gates for the CSP pinning and the text-fallback wire shapes,
  `cargo test` green.

- **feat(server): dynamic tool discovery for unconfigured ticketing** — the
  `ticket_*` group is config-gated on `tools/list`: installs without a
  ticketing provider announce only the 41 core tools, and the `tools`
  capability now advertises `listChanged`. Calling a hidden tool answers with
  a protocol error naming the fix (`ticketing is not configured — add a
  provider to config/ticketing_config.json`) instead of a bare "tool not
  found"; `tools/get`-style lookups (the Streamable HTTP `Mcp-Param-*`
  validation) treat hidden tools as absent. The ticketing service is now
  hot-reloadable (interior-mutable config/provider set): a config change —
  the `settings_apply` MCP tool or a settings UI POST — re-reads
  `config/ticketing_config.json`, swaps the live provider set and, when the
  visibility gate flips, broadcasts `notifications/tools/list_changed` to
  every initialized session (registered on `initialized`, dead peers pruned
  on failed delivery); 2026-07-28 discover clients rely on the `ttl_ms: 0`
  cache hint and simply re-list. Verify: F0.6 protocol gate over the
  in-memory duplex (closed/open gate tool lists, hidden-call error, flip
  broadcast with re-list), e2e plugin handshake asserts the 41-tool surface,
  `cargo test` green.
- **feat(ai): reasoning via mcp sampling with lm-bridge fallback** — the
  runtime reasons through the MCP client's own model: the new
  `mcp_sampling` provider sends `sampling/createMessage` on the current
  session (`SamplingSessionRegistry` — tool dispatch attaches the client
  peer, clients that declared the `sampling` capability answer). New
  `ai.reasoning.provider` values `ide_sampling`/`mcp_sampling` assemble the
  ordered fallback chain `sampling → lm_bridge → ollama`
  (`FallbackChain`): legs that are down (no sampling-capable client, dead
  `THRESKI_LM_BRIDGE_URL`, no Ollama daemon) are skipped, failures fold
  into one joined error, and `context_only_fallback` still degrades to the
  context-only pack. In plugin runtime mode `hybrid`/empty now defaults to
  `ide_sampling` (the IDE is the primary reasoning via); `settings_get`
  reports the effective provider plus `sampling_available`. The IDE LM
  bridge is deprecated but kept working as chain fallback. Verify: unit
  tests for provider mapping and chain ordering, protocol gate over the
  in-memory duplex (sampling-capable client drives the `ask_project`
  agentic lane; a client without the capability degrades to the chain
  error), `cargo test` green.
- **feat(server): memory, rules and tickets as mcp resources (templates +
  live updates)** — `resources/templates/list` with the three F0.4
  templates (`threski://memory/{kind}/{id}`, `threski://rules/{name}`,
  `threski://ticket/{id}`), each read as JSON: a memory returns the
  `memory_expand` projection with the URI kind pinned against the record,
  a rule resolves by title (case-insensitive) or record id through the
  `get_rules` shape, a ticket through the configured providers. Live
  updates: `resources.subscribe` is now advertised and both subscription
  mechanisms are served — legacy `resources/subscribe`/`unsubscribe` peers
  (≤ 2025-11-25) and 2026-07-28 `subscriptions/listen` sinks (registered
  for the accepted URIs, removed on cancellation). Memory mutations
  (`memory_add`/`update`/`delete`/`share`, `store_rule`,
  `set_working_state`, `capture_observation`, `ingest_design_note`) fire
  `notifications/resources/updated` with the exact mutated URI (rules twin
  their `threski://rules/{name}` URI) on the transport notification lane;
  fires are best-effort and never fail the tool result. Protocol gate:
  capability, template table, degraded reads and both subscription flows
  over the in-memory duplex.
- **feat(server): mcp prompts (/threski.*)** — `prompts/list` + `prompts/get`
  with five session-contract prompts (`readiness`, `status`, `why_changed`,
  `blast_radius`, `remember`): each renders a single user message instructing
  exactly which Threski tools to call, with the hygiene rules attached.
  Argument completions (`completion/complete`): static kind/visibility values
  plus config-backed project names; unknown refs degrade to empty. Capability
  advertisement: `prompts` + `completions`.
- **feat(server): read-only tool annotations (readonlyhint, titles)** — every
  one of the 56 MCP tools now declares MCP annotations: a human-readable
  `title` (top-level and inside the annotations) and an explicit
  `readOnlyHint` — `true` for the 22 pure-read tools (never prompt for
  confirmation), `false` for mutators. Completeness test pins the bijection
  with the tool registry and the exact read-only set.

## [0.2.0] — 2026-10-29

### Rebrand: Ibis Assistant → Threski Studio

- **refactor!: rename crates `ibis-*` to `threski-*`** — core, logger, git, db,
  ai, ingest lanes, search, ticketing, repopr, settings, wizard, cli,
  httpserver, server, optimization (`mcp-bridge` and `xtask` unchanged), with
  workspace, fixtures and tests.
- **refactor!: binary, env vars and data dirs to threski** — `threski.exe`,
  `THRESKI_*` environment variables, data dir `%LOCALAPPDATA%\threski\`,
  resource schema `threski://`, MCP server label "Threski".
- **refactor: ide surfaces to threski studio** — `@threski/ide-host`, Cursor
  plugin (`mcp.json`, env, skills/commands `threski-*`), VSIX
  `fatterdev.threski-studio` with `threskiStudio.*` settings and commands
  "Threski Studio: *".
- **docs: rebrand to threski studio with sacred-ibis lore** — README, docs,
  CHANGELOGs and social assets (rebuilt from a new vector source); repo moved
  to `fatterdev/threski-studio`.
- **Breaking (pre-release)** — actor header `X-Ibis-Actor` → `X-Threski-Actor`;
  SurrealDB namespace `ibisassistant` → `threski`; VS Code settings prefix
  `ibis.*` → `threskiStudio.*`. Local data from 0.1.0 must be re-ingested.

## [0.1.0] — Initial release

MCP knowledge graph for real codebases: living project memory on
git/code/tickets/the SurrealDB knowledge graph, polyglot ast-grep ingest with
AI rule synthesis, log analysis that learns structure, git-bound
commit-meaning temporal layer, and plug-and-play single-binary onboarding
(personal / plugin / server runtimes).

- **20 crates**: core, logger, git, db, ai, ingest lanes (code, logs, git,
  redmine, dynamic), search, ticketing (jira/redmine/azure_devops), repopr,
  settings (+web SPA), wizard, cli, httpserver, server (56-tool MCP surface),
  optimization, mcp-bridge, xtask.
- **MCP contract gate**: 56 tools, Streamable HTTP at `/mcp`, no legacy tool
  names on the surface.
- **SurrealDB 3.x-native persistence**: schema-first SCHEMAFULL DDL, typed
  bindings, final-state DDL with no legacy migration code.
- **Polyglot rule packs** for go, py, ts/js, java, kotlin, swift, rust, c,
  cpp, csharp, php, ruby, lua, scala, css/scss, html (+ template hosts) with
  AI-synthesized rule generation for sparse extensions.
- **Windows-first test estate** (786+ tests), `cargo xtask dist` layout gate,
  e2e gate (plugin-mode stdio launch → handshake → roots bind → embedded DB →
  memory tool → clean exit).