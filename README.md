<p align="center">
  <img src="docs/assets/social-preview.png" alt="Ibis Assistant - Living project memory for real codebases" width="100%">
</p>

<h1 align="center">Ibis Assistant</h1>

<p align="center">
  <a href="#license"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux-lightgrey.svg" alt="Platform: Windows | Linux">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange.svg?logo=rust" alt="Built with Rust">
  <img src="https://img.shields.io/badge/MCP-compatible-8A2BE2.svg" alt="MCP compatible">
  <img src="https://img.shields.io/badge/PRs-welcome-brightgreen.svg" alt="PRs welcome">
</p>

**One executable that turns a messy, polyglot working tree and production logs into a queryable knowledge graph - then lives on it as project memory.**

Ibis Assistant is an MCP server for real codebases: it ingests git causality, ast-grep structure, AI-synthesized AST rules, logs, and tickets into a single SurrealDB graph, and stores typed team memory *on that same graph* - retrieved with hybrid search (vector + time + keywords + graph). Client-agnostic MCP (Cursor, VS Code, Claude Desktop, CLI). Personal daemon or team server. Local-first hybrid AI (Ollama; cloud optional).

## How it fits together

```text
 git commits  --+        +--> ast-grep AST chunks  --+
 logs / tickets -+-->  ingest lanes  -->  SurrealDB knowledge graph  -->  hybrid ask /
 AST rules      --+        |                   (commits, files, chunks,        blast radius /
                           |                    memory, ticket_* / repo_pr_*)   why_changed
                           +--> AI rule synthesis (validated YAML -> rules/)
                                                        |
                                                        +--> typed memory_* on the same graph
```

1. **Ingest** - git causality (`commit -> changed -> file`), polyglot AST chunks, logs, tickets.
2. **Graph** - one SurrealDB, one truth: code, change meaning, issues, authors, memories.
3. **Serve** - 56-tool MCP surface over Streamable HTTP (`/mcp`) + browser Settings UI.
4. **Remember** - typed, sharing-first memory that keeps the graph useful across sessions.

## Feature matrix

| Capability | What you get | Details |
|------------|--------------|---------|
| **Git causality** | `commit -> changed -> file` graph for "why did this change?" forensics | `why_changed` |
| **Polyglot AST ingest** | ast-grep chunks + `languageGlobs` for mixed/legacy stacks; rule packs for go, py, ts/js, java, kotlin, swift, rust, c, cpp, csharp, php, ruby, lua, scala, css/scss, html | [docs/code_ingest_polyglot_and_rules.md](docs/code_ingest_polyglot_and_rules.md) |
| **AI rule synthesis** | Sparse extensions get AI-proposed, validated YAML rules persisted under `rules/` | [docs/code_ingest_polyglot_and_rules.md](docs/code_ingest_polyglot_and_rules.md) |
| **Log analysis** | Recursive watch; static + ast-grep first, AI residual; file/location/cause report; repeats promote back into rules | [docs/log_analysis_pipeline.md](docs/log_analysis_pipeline.md) |
| **Commit-meaning temporal layer** | Per-commit message+patch embeddings (`commit_chunk`), as-of queries, currentness gates | [docs/commit_meaning_temporal.md](docs/commit_meaning_temporal.md) |
| **Living memory** | Typed `memory_*` (decision/fix/constraint/rule/gotcha/...), supersession, working state, local-first consolidation | [docs/memory_layer.md](docs/memory_layer.md) |
| **Tickets & PRs** | Redmine / Jira / Azure DevOps; `ticket_*`, `repo_pr_*` | [docs/TICKETING_CLIENT.md](docs/TICKETING_CLIENT.md) |
| **Plug-and-play onboarding** | One binary; `config fast`/`full` wizard; SurrealDB, Ollama, `sg` sidecar auto-download/start; Settings in browser or IDE | [docs/ai_and_settings.md](docs/ai_and_settings.md) |
| **Client-agnostic MCP** | Streamable HTTP at `/mcp`; Cursor plugin, VS Code extension, Claude Desktop, CLI | [docs/CLAUDE_INTEGRATION.md](docs/CLAUDE_INTEGRATION.md) |
| **Three runtimes** | `personal` (live tree) / `plugin` (IDE stdio child) / `server` (shared host) | [docs/runtime_modes.md](docs/runtime_modes.md) |

## Quick start

No Rust toolchain required for normal use:

```bash
# 1. Grab the latest binary from Releases and put it on PATH
#    https://github.com/fatterdev/ibis-assistant/releases

# 2. First launch: interactive wizard (probes GPU/CPU, writes keys and paths)
ibis-assistant          # config fast if needed
ibis-assistant run      # MCP POST /mcp + /settings/ UI

# 3. Point your MCP client at it
#    Cursor/VS Code: "Ibis: Open Settings" | others: http://127.0.0.1:3030/mcp
```

Exercise the graph:

```bash
ibis-assistant ingest code --path ./path/to/project
ibis-assistant ask "What breaks if Prices changes?" --branch main
ibis-assistant logs analyze --project MyApp --text "ERROR timeout talking to db"
ibis-assistant memory add --project MyApp --kind decision --title "MCP transport" --text "Prefer Streamable HTTP at /mcp"
```

Build from source:

```bash
git clone https://github.com/fatterdev/ibis-assistant.git
cd ibis-assistant
cargo xtask dist          # dist/ibis-assistant.exe + mcp-bridge + rules + sgconfig
cargo test                # 786+ tests; SurrealDB/Ollama/sg sidecars auto-provision
```

Two IDEs on one machine? Run the personal daemon once and point the second at it with `IBIS_MCP_URL`. Tracked defaults live in `config_master.json`; never commit a local `config.json`.

## Repo layout

```text
crates/              Rust workspace (20+ crates)
  ibis-core            domain model
  ibis-ingest-*        git / code / logs / redmine / dynamic ingest lanes
  ibis-search          hybrid retrieval (vector + time + keywords + graph)
  ibis-server          56-tool MCP surface over Streamable HTTP
  ibis-db              SurrealDB persistence (schema-first, SCHEMAFULL)
  ibis-cli / ibis-wizard  CLI + onboarding wizard
  mcp-bridge           stdio MCP bridge for IDE hosts
ide-host/            Thin Node IDE host (plugin stdio or shared HTTP MCP)
vscode-extension/    VS Code extension (Ibis: Open Settings, Remember, status)
cursor-plugin/       Cursor plugin (binary on PATH, no ${workspaceFolder} wiring)
docs/                Guides: ingest, logs, memory, runtimes, settings, ticketing
rules/               ast-grep rule packs (static + AI-synthesized)
config/              Reference configurations
```

## What it is not

- **Not a SaaS.** No tenancy, no hosted memory service, no signup - keys live in your local `config.json` (ENV is override only).
- **Not an agent OS.** Ibis does not absorb or rewrite your agents; it feeds them context.
- **Not a generic memory API.** Memory here is typed, project-scoped, and lives on the code graph - not chat-transcript storage.
- **Not a SurrealDB replacement or a markdown sync bus.** Surreal *is* the store; markdown projection is optional.
- **No blockchain attestation.** Provenance is git, not ledgers.

## Status

- `v0.1.0` - initial release; pre-1.0, so breaking changes to config and MCP surface are possible.
- Windows-first test estate (786+ tests) with CI on `windows-latest` and `ubuntu-24.04`.
- Release binaries ship with build provenance attestations.

Contributions are welcome - PRs and issues at [github.com/fatterdev/ibis-assistant](https://github.com/fatterdev/ibis-assistant). Prefer ast-grep rules (incl. synthesis) over regex-only ingest, `memory_add`/`capture_observation` for durable notes, and `ticket_*` over legacy tool names.

## License

Licensed under the **Apache License, Version 2.0** - see [LICENSE](LICENSE) and [NOTICE](NOTICE).

Copyright 2026 fatterdev

Redistributions of this software, with or without modification, must retain the copyright, patent, trademark, and attribution notices contained in [NOTICE](NOTICE), as required by Apache-2.0 §4(d). The Ibis Assistant name, logo, and visual identity are excluded from that grant per Apache-2.0 §6 - do not use them to market derived products without permission.