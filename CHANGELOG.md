# Changelog

All notable changes to Threski Studio are documented here.

## [Unreleased]

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