# Changelog

All notable changes to Ibis Assistant are documented here.

## [Unreleased]

- **fix(ai): usage-flush batch test is load-independent** -
  `usage_flush_batches_updates` tied its assertion to wall-clock time (50
  sequential embeds had to finish within ~1s to stay under the `< 10` save
  bound); under parallel test load the embed window stretches and every
  100ms flusher tick that finds fresh usage saves once (missed-tick
  catch-up included), so the test failed non-deterministically (verified on
  a clean tree, ~1 run in 2). The bound is now derived from the measured
  embed window (`loop_ms/100 + 5`, covering the 250ms tail and the stop
  flush) with a `>= 1` flush guard: the pinned invariant is coalescing
  (per-call persistence would give ~50 saves), not an absolute cadence.
- **chore(search): S1.4 — search timing demoted to debug, list/expand timed** -
  the S1.3 measurement instrumentation on `memory_search` (start / embedded /
  querying db / db query returned / rows / boosting / linked_symbols / done)
  moves `info`→`debug`: invisible at the default INFO log level, back on
  demand with `log_level DEBUG` (verdict landed, numbers registered in the
  S1.3 analysis). `memory_list` and `memory_expand` — the memory-tree
  phrase-B/C paths, previously silent in `plugin.log` — now emit
  `memory_list: done in … entries=N` and `memory_expand: done in … chain=N`
  at `info`, closing the evidence-parity gap (S1.3 analysis §6.3) for the
  next owner run.
- **feat(vsix): 1.0.2 — runtime pre-warm at activation** - `activate()` now
  fires a fire-and-forget pre-warm: the persistent ide-host session child is
  spawned, the MCP handshake paid, and one cheap read-only call
  (`ticket_get_capabilities`) issued while VS Code starts up, so the plugin
  boot tax (SurrealDB sidecar 1–4s + checks + probes; up to ~17s with Ollama
  down) no longer lands on the first memory-tree interaction (S1.3 cold
  target, owner decision 03/11: option 1). Evidence lands in the Output
  channel (`pre-warm: runtime session ready in Nms`); the first real
  interaction reuses the same warm session (same backend identity).

- **feat(search): RRF channel fusion behind `search.fusion` (A2 ddofds)** -
  `ask_project` merges the four channels by per-channel rank with Reciprocal
  Rank Fusion (`rrf_fuse`, k=60, cross-channel contributions sum) instead of
  the fixed-weight additive blend when `search.fusion = "rrf"`; the legacy
  additive path stays the default (safe rollout) and is byte-for-byte pinned
  by the existing unit tests. In RRF mode the "is current" bonus becomes a
  constant epsilon applied after fusion (no longer a fraction of the cosine
  score) and ties break deterministically by record id. Unknown config values
  log a warning and resolve to additive. Validation harness:
  `ibis eval retrieval --fusion additive|rrf` (flag + `fusion` field in the
  JSON report). Evidence
  (`docs/planning/2026-11-03-a2-rrf-validation.md`): parity with the additive
  baseline on the offline embedder (MRR 0.917 / recall@5 0.958 / recall@15
  1.0) and a real-embedder (Ollama) regression (MRR 0.875 / recall@5 0.958 —
  rank-only fusion loses the cosine margin, the documented RRF tradeoff), so
  the default is NOT flipped; an owner decision is logged in the master work
  queue (WQ-5). A1 baseline now also measured with the real embedder
  (additive: MRR 0.917 / recall@5 1.0 / recall@15 1.0).
- **feat(vsix): 1.0.1 — memory tree parent count + refresh feedback** - the
  memory tree parent (a kind or the search root) now shows a **count of the
  loaded entries** as its description (`N+` when the fetch hits the list
  cap); the parent row re-renders only when the count label actually changes
  (convergent fire guard — an unconditional element-scoped fire from
  `getChildren` would refetch an expanded node forever). The view-title
  refresh button shows a **spinner in the view title** while the re-fetch
  cycle runs (before: the refetch happened silently and looked like a dead
  button). Debug builds iterated as `1.1.0→1.1.2` with throwaway timing
  instrumentation during the S1.3 session; the instrumentation is removed in
  this build and the debug-build numbering moves to patch bumps (`1.0.x`),
  keeping `1.1.0` reserved for F4. Supporting artifacts for the R3.3 closure:
  `scripts/seed-memory-r33.mjs` (seeds 3 team memories into the real plugin
  data dir through the same spawn contract the VSIX uses and times
  `memory_search` AND `memory_list` — the phrase-B call shape), the
  failure-first owner runbook
  `docs/planning/2026-11-03-r33-closure-runbook.md` and the S1.3 timing
  analysis `docs/planning/2026-11-03-s13-timing-analysis.md`.
- **feat(ide-host): persistent stdio MCP sessions for the VSIX** - every
  spawn-mode tool call paid a full child boot (workspace bind + embedded
  SurrealDB sidecar) and tore it down right after the reply, so each tree
  expansion / search / detail / Remember stayed multi-second even after the
  `memory_list` and embed-pipeline fixes; tool calls now share one
  initialized child per backend identity (multiplexed request ids, idle
  self-close after 120s via `IBIS_MCP_SESSION_IDLE_MS`, one clean retry when
  a child died between calls, late replies for timed-out ids discarded).
  `keepAliveMs` keeps the legacy one-shot path for the ephemeral settings UI;
  `disposeMcpSessions()` runs on `ibis.runtime.*` config changes and on
  extension dispose. New `ide-host/src/session_test.mjs` (reuse,
  dispose-respawn, dead-child retry) wired into CI.
- **fix(ai): embed cache clears never lose a requested embedding** - the
  batch tail read every input back from the cache with an `expect`, so a
  concurrent batch's wholesale clear (cap 1024) could wipe a just-computed
  entry and panic the caller; fetched embeddings now travel by value
  (`fetch_embeddings` returns the pairs), hits are read in the same lock
  that decides the stragglers, and entries the clear already wiped join one
  bounded re-fetch instead of panicking (stress test with cap 4).

- **fix(search): `memory_search` no longer panics when limit exceeds the
  fetch cap** - `(limit * 3).clamp(limit, 45)` panicked with `min > max` when
  the IDE memory tree sent `limit=50`, killing the handler task and hanging
  the MCP call until the client timeout; the clamp now never drops below the
  requested limit (`(limit * 3).min(45).max(limit)`), regression tests pin
  both directions (50 → `LIMIT 50`, 5 → `LIMIT 15`).
- **feat(server): panics land in the log file and tool calls fail fast** -
  `install_panic_hook` routes panics through `tracing` (payload, thread,
  location, forced backtrace) so stdio/plugin runs record them in the log
  file instead of an unread stderr; `tools/call` dispatch wraps the router in
  `catch_unwind` and returns an `isError` result carrying the panic payload
  instead of leaving the request hanging.
- **feat(server): `memory_list` MCP tool** - embedding-free metadata browse
  (kind/status/visibility filters, most recent first) backing the memory tree
  kind nodes; tool surface is now 57 (contract tests, read-only annotation
  set and discovery fixture updated).
- **feat(ai): local embed pipeline guards** - `keep_alive` on `/api/embed`
  (default `"30m"`, `IBIS_OLLAMA_KEEP_ALIVE` override; a cold model reload
  costs ~4s and the API parameter overrides the server `OLLAMA_KEEP_ALIVE`),
  in-process query-embedding cache (cap 1024) and a process-wide FIFO
  queue/limiter (default 1 permit, `IBIS_OLLAMA_EMBED_CONCURRENCY`) that
  removes the concurrent-request transport failures observed on the AMD/ROCm
  Ollama bundle (`OLLAMA_MAX_QUEUE` was never the cause: those were
  connection-level errors, not 503s).
- **feat(vscode): instant memory tree + structured status** - kind nodes
  browse via `memory_list` (~2 ms, no embedding) instead of one semantic
  search per kind (8 concurrent Ollama embeds at ~4s, source of the
  "ollama API request failed" errors); the Status toast shows the compact
  summary with **Details**/**Open Settings** actions while the full
  structured report lands in the output channel (the toast cannot render
  line breaks).
- **chore(vscode): activity bar icon refresh** - pre-existing uncommitted
  asset swap committed with the rest of the pending work.
- **docs: memory layer tool table + local embedding pipeline** -
  `memory_list` documented in `docs/memory_layer.md`; the Ollama embed guards
  (keep_alive / cache / queue, env overrides) documented in
  `docs/ai_and_settings.md`.
- **chore(ui-test): SendKeys driver (Parte B)** - `scripts/ui-test/keys.mjs`
  drives deterministic palette/tree/chat flows for the R3/R4 computer-use
  pack (window activate via WScript.Shell, clipboard paste, optional wait).

- **build(vscode): oidc publishing, open vsx and mcp registry listing** (F1.6)
  — manual `publish-vscode.yml` workflow (dispatch toggles per target): VSIX
  packaging, VS Marketplace publish via OIDC trusted publishing (`id-token:
  write`), Open VSX via `ovsx` + `OPEN_VSX_TOKEN` secret, MCP registry listing
  via `mcp-publisher` GitHub OIDC from root `server.json`
  (`io.github.fatterdev/ibis`, loopback Streamable HTTP personal runtime);
  `vscode:mcp/install` badge in the VSIX README.
- **feat(plugin): marketplace and workspace recommendations** (F2.6) —
  `marketplace/` scaffold pushed later as the `fatterdev/ibis-marketplace`
  repo (owner action): Claude-Code-compatible `marketplace.json` listing the
  `ibis` plugin from `fatterdev/ibis-assistant@plugin` (single source of
  truth), install badge `vscode://chat-plugin/install`, and the team
  `.github/copilot/settings.json` snippet. F2.1–F2.5 were already satisfied
  by the migrated plugin scaffold (plugin.json/mcp.json, 5 skills, session
  hooks, forensics/curator agents, ingest/consolidate automations).
- **fix(plugin): conditional agent-stop capture reminder** — verified the hook
  scripts against the Copilot hooks reference: `sessionStart` consumes only
  `additionalContext` (output cleaned of ignored keys), while `agentStop`
  consumes **only** the stop decision (`decision: "block"` + `reason`) — the
  previous `additionalContext` reminder was silently dropped. `agent-stop.mjs`
  now forces exactly one continuation (`stop_hook_active` guard) only when the
  turn did editing work (`Write`/`Edit`/… signals in the transcript tail)
  without any `capture_observation`/`memory_add`; otherwise `{}` (fail-open,
  never blocks blindly). New `scripts/smoke-plugin.mjs` (Parte A R3/R4):
  26 checks on plugin scaffold, hook contract, VSIX contributions and
  distribution manifests — 26/26 PASS.
- **docs: review r3/r4 checklist** — `docs/planning/review-r3r4-checklist.md`:
  gate runbook for the VSIX (install, status bar, tree view, remember with
  provenance, chat instruction, walkthrough, MCP provider) and the plugin
  (install from source, skills, MCP server, hooks, agents, automations),
  plus the publish-gate steps (②③⑤, workflow dispatch, marketplace push).
- **chore(evals): computer-use ui test runner for r3/r4** —
  `scripts/ui-test/` (tasks.json + `run-ui-tests.mjs` + README): machine
  readable task pack (R3.1–R3.8, P4.1–P4.6) driven by paste-ready prompts for
  UI-TARS Desktop (installed via winget, `ByteDance.UI-TARS` 0.2.4), with
  evidence runs under `artifacts/ui-tests/` (gitignored) and
  results.json/results.md collection — Parte B of the R3/R4 checklist can now
  be executed (and evidenced) by a GUI agent instead of hand-only clicks.

## [0.2.0] — 2026-10-30

- **chore(release): runtime 0.2.0** - workspace version `0.1.0` -> `0.2.0`
  (Fase 0 complete; gate R1 29/10, gate R2 30/10). The VSIX stays `1.0.0`
  until first publish, the agent plugin stays `0.1.0`.

- **fix(server): batch-manager pending rows decode record ids** - the
  `BatchManager` pending chunk/commit projections typed the `id` column as a
  string while SurrealDB returns native record ids, so every 10s poll failed
  with `Expected string, got record` once ingest produced pending rows
  (plugin.log error loop). The rows now decode `Option<RecordId>` and
  normalize via `record_id_to_plain` (Go `CoerceRecordID` parity); regression
  tests pin chunk/commit decode and the write-guard round-trip, and document
  that driver `to_sql` renders string keys backticked while the project
  canonical form comes from `format_record`. Verify: `cargo test -p
  ibis-server` green, `clippy -D warnings` clean.

- **test: gate R2 closed with the MCP protocol smoke and review checklist** -
  `scripts/smoke-mcp.mjs` drives the real `ibis` binary over stdio and
  verifies the whole F0 surface in one run (handshake, F0.2 annotations with
  the exact read-only set, F0.3 prompts + argument completions, F0.4
  resources/templates/live updates, F0.5 sampling round-trip, F0.7 MCP App
  resource + `_meta.ui` + guaranteed text fallback): 23/23 PASS against
  `target/debug` and `dist`. Owner-side smoke in VS Code 1.144 evidenced in
  `docs/planning/review-r2-checklist.md`: zero confirmations on read-only
  tools, `/ibis.*` prompts in chat, resources in Add Context, sampling served
  by the chat model (`chat.mcp.serverSampling` authorized; 15 runtime
  rule-generation requests answered by `copilot/auto`), Memory Curator app
  rendered inline with the text fallback. Non-blocking findings recorded:
  VS Code renders prompt-argument completions without suggestions (protocol
  verified), AI-generated sg rules declare non-built-in `language: markdown`
  (ingest-lane quality, D-CLAIMS candidate).

- **fix: post-revert release workflow, vsix extension id and docs alignment** -
  `.github/workflows/release.yml` now packages the actual `ibis` (`ibis.exe`)
  and `mcp-bridge` (`mcp-bridge.exe`) binaries per target with `set -euo
  pipefail` instead of silently skipping a non-existent `ibis-assistant*`
  binary; `vscode-extension/extension.js` fixes `EXTENSION_ID` from
  `fatterdev.ibis` to `fatterdev.ibis-assistant` (so **Ibis Assistant: Open
  Extension Settings (VS Code)** filters `@ext:fatterdev.ibis-assistant`) and
  aligns command UI titles; `vscode-extension/README.md`,
  `vscode-extension/CHANGELOG.md`, `cursor-plugin/*` and `docs/planning/*`
  fix leftover `Threski Studio`, `MIT`, `master`-branch and Go-binary
  references; `.gitignore` ignores local `.cursor/` and `.r2-smoke-data/`.

- **refactor!: revert rebrand to ibis assistant** - the product naming reverts
  to **Ibis Assistant** (owner decision 30/10/2026): crates `threski-*` ->
  `ibis-*` (`mcp-bridge` unchanged), env `THRESKI_*` -> `IBIS_*`, resource
  scheme `threski://` -> `ibis://`, binary `ibis` (short), data dir
  `%LOCALAPPDATA%\ibis`, VSIX `fatterdev.ibis-assistant` with settings
  `ibis.*` and commands "Ibis Assistant: *", ide-host `@ibis/ide-host`
  (clientInfo `ibis-ide-host/1.0.0`), agent plugin `ibis` with the short
  skills (`/ibis usage`, ...), MCP label "Ibis" (`/ibis <prompt>`), MCP App
  "Ibis Assistant Memory Curator" (`ui://ibis/memory-curator`). The retired
  `threski://` scheme remains only as the foreign-scheme example in the
  resource-parse test; *Threskiornis aethiopicus* stays in the lore (the
  bird's genus). All F0.2-F0.7 features and this session's fixes carry over
  unchanged (mechanical rename). Tool: `scripts/migrate/rename-back-ibis.ps1`
  (in-place inverse of the F0.1 migration); historical documents
  (CHANGELOG, PLAN, migrate scripts) keep the threski-era record. Verify:
  `cargo test` green, `cargo xtask dist` -> `ibis.exe`, `vsce package` ->
  `ibis-assistant-1.0.0.vsix`, ide-host framing test, live stdio handshake
  (`ibis -mode stdio -runtime-mode plugin` answers `id=1`).

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