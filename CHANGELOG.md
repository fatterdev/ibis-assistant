# Changelog

All notable changes to Ibis Assistant are documented here.

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