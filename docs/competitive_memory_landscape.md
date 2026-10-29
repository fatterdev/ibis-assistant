# Competitive landscape: project intelligence for coding agents

**Date:** 2026-10-09  
**Status:** Product positioning (README input)  
**Related:** [code_ingest_polyglot_and_rules.md](code_ingest_polyglot_and_rules.md), [log_analysis_pipeline.md](log_analysis_pipeline.md), [commit_meaning_temporal.md](commit_meaning_temporal.md), [memory_layer.md](memory_layer.md), [superpowers/specs/2026-10-09-memory-absorption-design.md](superpowers/specs/2026-10-09-memory-absorption-design.md)

Comparison used when deciding how Threski should relate to the “agent memory” wave without mistaking memory APIs for a code intelligence product.

## Decision

1. **Moat first:** polyglot **ast-grep** ingest, **AI-validated rule synthesis** for sparse/legacy languages, and a log pipeline that prefers static/AST matches and can **synthesize/promote rules** from residuals — none of the compared memory products own this loop.
2. **Operability is part of the product:** one Go executable; wizard (`config fast` / `config full`); auto-download/start of SurrealDB, Ollama, and `sg`; Settings UI from browser **and** IDE (`settings_open_ui` / **Threski: Open Settings**); keys in `config.json` rather than ENV-first; Windows service install in one UAC pass. Competitors that are “just an MCP memory server” or “just an SDK” leave this stack to the user.
3. **Then living memory** on the same SurrealDB graph (typed, sharing-first, session load, local-first consolidate) — absorb ideas from the memory wave; do not vendor them.
4. **Refuse:** SaaS tenancy, blockchain attestation, second database, markdown-as-sync, becoming an agent OS (Letta-shaped).

## Comparison (summary)

| Product | Best fit | Gap vs Threski DNA |
|---------|----------|-----------------|
| **Recallium** | Local MCP memory for IDEs; typed developer categories | Memory-first; no polyglot AST synth or log→rule loop |
| **mem0** | Drop-in memory API for arbitrary agents; cloud story | General personalization; not a code/log graph builder |
| **RepoMemory** | Citation-first answers from repository evidence | Evidence/index; not AST rule growth or production log enrichment |
| **agentmemory** | Deep Claude Code hooks / capture | Host-skewed memory; not shared graph + rule synthesis |
| **Unibase / Membase** | Ledger / attestation narratives | Rejected: trust-through-blockchain out of DNA |
| **Letta** | Stateful agents with editable memory blocks | Agent *runtime*; wrong shape for attach-MCP-to-existing-IDE |
| **Threski Studio** | **Build the graph** (git, AST, logs, tickets) **then** remember on it; **one-exe** wizard + auto-runtime | One Go binary: personal / plugin / server |

## Weighting (product importance)

| Weight | Capability |
|--------|------------|
| Highest | Polyglot AST + AI rule synthesis (code); log static/AST-first + rule promotion |
| Highest | Plug-and-play: single binary, auto-provision deps, fast/full wizard, IDE/browser Settings |
| High | Unified SurrealDB graph + hybrid ask (`file_chunk` + `commit_chunk` meaning) |
| High | Temporal freshness (commit meaning + memory currentness / path-based stale) |
| High | Sharing-first typed memory + session/consolidate |
| Supporting | Hybrid local AI, live trees, ticketing/PRs |

Canonical operator docs: [code ingest](code_ingest_polyglot_and_rules.md), [logs](log_analysis_pipeline.md), [commit meaning](commit_meaning_temporal.md), [memory](memory_layer.md).
