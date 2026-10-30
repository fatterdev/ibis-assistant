# Typed memory layer

**Date:** 2026-10-09  
**Status:** Implemented (PR #28)  
**Design:** [superpowers/specs/2026-10-09-memory-absorption-design.md](superpowers/specs/2026-10-09-memory-absorption-design.md)  
**Plan:** [superpowers/plans/2026-10-09-memory-absorption.md](superpowers/plans/2026-10-09-memory-absorption.md)

Operator guide for the sharing-first project memory layer: kinds, visibility, capture, consolidation, Settings viewer, CLI, and optional markdown projection.

**Temporal north star:** typed memory is the team/session “what is true **now** (HEAD)” layer — wall-clock `valid_*`, `status`, supersession, visibility. That is **not** “true at arbitrary git commit R”. Per-commit change meaning (Δ) lives in `commit_chunk` — [commit_meaning_temporal.md](commit_meaning_temporal.md). Proposed git-indexed Σ (`as_of` / `knowledge_diff`) — [superpowers/specs/2026-10-24-git-indexed-world-state.md](superpowers/specs/2026-10-24-git-indexed-world-state.md). Prefer `load_session_context` / `memory_search` for durable HEAD truth; use `ask_project` commit hits and `why_changed` for why/when code changed.

## What it is

SurrealDB remains the **source of truth**. Memories are typed rows on the existing `memory` table (plus `workstream` and `supersedes` / `derived_from` / `part_of_workstream` edges). They are searchable via `ask_project` / `memory_search` with vector + time decay + keyword/entity boosts + graph context. Default ask/session paths require **current** status, open `valid_until`, and **team** visibility (personal only for a trusted actor).

This is **not** a third-party agent-memory product and **not** a SaaS multi-tenant store. Trust boundary is the same as the rest of Ibis: whoever can reach that SurrealDB instance.

## Sharing-first model

| Field | Default | Notes |
|-------|---------|-------|
| `visibility` | `team` | Personal rows are opt-in (`visibility=personal`) |
| `owner` | actor id | Attribution only; not an ACL wall for team rows |
| `status` | `current` | Also `stale`, `superseded`, `archived` |
| `kind` | `fact` | See kinds below |

Actor identity:

- HTTP loopback (`127.0.0.1` / `::1`): `X-Ibis-Actor` header is trusted for personal-memory authorization
- HTTP remote (non-loopback): claimed actor is **ignored** for personal reads/share/update; only `visibility=team` memories are authorized (attribution on new team writes may still use an explicit `actor` arg)
- MCP/stdio (local process): optional `actor` tool argument is trusted

Personal memories are visible only to a **trusted** matching actor; team memories are visible to everyone on the instance. Use `memory_share` to flip personal ↔ team. `memory_export` / `.ibis/memory` projection default to **team** so other owners' personal rows are never dumped.

## Kinds

| Kind | Typical use |
|------|-------------|
| `decision` | Product/architecture choice to keep |
| `fix` | Proven remediation |
| `constraint` / `rule` / `gotcha` | Hard limits and durable guidance |
| `design` | Design notes (`ingest_design_note`) |
| `fact` | General durable note (legacy collaborative writes land here) |
| `working_state` | Latest-wins branch/task scratchpad |
| `observation` | Cheap raw capture; consolidated later |
| `reasoning` | Linked from `save_reasoning_outcome` |

## Agent workflow (do not spam tools)

1. When prior context helps: call **`load_session_context` once** (working state, rules, recent fixes/failures, open decisions).
2. Capture durable items with **`memory_add`** (or IDE **Ibis: Remember** / **Ibis: Set working state**).
3. Call **`capture_observation`** only for a new reusable lesson — not routine tool output.
4. Let **`memory_consolidate`** (local Ollama, extends the RAFT optimizer) distill observations into typed memories and supersede stale ones. Duplicate candidates match the observation visibility (team→team, personal→same-owner personal); personal observations use a `local_only` route hint so content never falls back to cloud.

Legacy tools stay registered and feed the same model:

- `provide_collaborative_memory` → typed `kind=fact` (team)
- `save_reasoning_outcome` → reinforces graph paths + typed `kind=reasoning`

## MCP tools

Source of truth: `cmd/server/main.go`.

| Group | Tools |
|-------|-------|
| Capture / session | `capture_observation`, `load_session_context`, `store_rule`, `get_rules`, `set_working_state`, `get_working_state` |
| CRUD / search | `memory_add`, `memory_search`, `memory_expand`, `memory_update`, `memory_delete`, `memory_history` |
| Forensics | `recent_failures`, `why_changed` |
| Lifecycle | `memory_consolidate`, `memory_share`, `memory_export`, `memory_import` |
| Design / insights | `ingest_design_note`, `memory_insights` |
| Legacy | `provide_collaborative_memory`, `save_reasoning_outcome` |

Client paste guides: [CLAUDE_INTEGRATION.md](CLAUDE_INTEGRATION.md), [../copilot-instructions.md](../copilot-instructions.md).

## CLI

Same binary talks to a running server (`POST /mcp`):

```bash
ibis memory add --project MyApp --text "Prefer Streamable HTTP" --kind decision --title "MCP transport"
ibis memory search "auth timeout" --project MyApp --kind fix
ibis memory session --project MyApp
ibis memory capture --project MyApp --text "Flaky test needs sleep on Windows"
ibis memory state set --project MyApp --content "Migrating memory layer"
ibis memory state get --project MyApp
ibis memory rule --project MyApp --title "No force-push main" --content "..."
ibis memory rules --project MyApp
ibis memory share <id> --visibility team
ibis memory expand <id>
ibis memory history <id>
ibis memory bootstrap-git-store [--dry-run]   # render durable claims into .ibis/claims/ (one commit)
ibis memory rebuild-git-notes [--dry-run]     # rebuild refs/notes/ibis-claims pointers from the claim index
ibis optimize   # RAFT; consolidation also via MCP memory_consolidate
```

`memory add` without typed flags still maps to `provide_collaborative_memory`. Any typed flag (`--kind`, `--title`, `--visibility`, …) routes to `memory_add`.

## Git claim store and degraded reads

Durable claims (`decision` / `rule` / `fix`) can live as files under `.ibis/claims/` in the project repo (durable, PR-reviewable); SurrealDB stays a derived index. `memory bootstrap-git-store` renders existing claims into one commit; later claims are committed automatically when added with a git context. Personal claims follow the `memory.git_store.personal_claims` policy (`db_only` default on server, `stub` default in personal mode, `in_repo` optional).

Reading history stays honest about what git alone can answer:

- `as_of_project`, `ask_project` (`at_ref`), `knowledge_diff` work **without SurrealDB** (fresh clone, plugin child, DB down): claims are parsed from `.ibis/claims/` at the queried ref and labeled `source_kind=claim_file`; `stub` files show **title only**. Degraded replies carry `degraded: true` (or `degraded: true` inside the diff payload), and the server logs a `[degraded] … served N claim(s)` line when a degraded read actually serves claims (zero-claim reads stay silent).
- Semantic channels (vector search, agentic ask, `memory_search`) still require SurrealDB — degraded mode never pretends git can answer those.
- **Shallow clones** (CI checkouts, depth-1 fetches) cannot prove ancestry: ancestry-based reads fail closed with a `shallow_history` error — deepen the clone (`git fetch --unshallow`) instead of trusting an empty world slice. This applies to personal mode and plugin children too, not only the server.
- In `ask_project at_ref` replies, tree claim files keep a floor of 3 slots at the shared result cap, so a full ranked board cannot evict them; the floor is a minimum, not a maximum — tree claims beyond it fill capacity ranked hits leave unused (a tree-only store still fills the reply). Ranked hits keep score order and tree claims trail unranked.
- Notes on `refs/notes/ibis-claims` are optional commit↔claim cross-links (`memory rebuild-git-notes`); they are pointers, never knowledge, and are rebuilt from the claim index. Claim commits also annotate themselves best-effort (a post-claim-commit notes hook writes the pointer note right after the commit lands), so notes stay fresh between explicit rebuilds; a failed notes write never fails the claim write.
- `as_of` code reads at resolved refs go through an optional **hot-ref byte cache** (`file_at_commit`, world-state spec §5.3/§6.1 strategy B): keyed by full commit OID + path — both immutable — so a hit is always the exact bytes git would return, while misses fall through to git and populate the cache best-effort. Storage stays bounded (reads older than 30 days are pruned after ingest). Kill-switch: `world_state.code_cache_disabled: true` in config.json leaves every read on the direct git path (strategy A) — correctness never depends on the cache.
- Keep `.ibis/` **not** in `.gitignore`: the claim store commits into that path, and an ignored directory makes the claim commit fail with a clear error.

## Settings UI

At `http://127.0.0.1:<port>/settings/` (or **Ibis: Open Settings**):

- Browse / filter by kind, status, visibility
- Toggle personal ↔ team per row
- Export JSON
- Optional checkbox: write regenerable `.ibis/memory/*.md` (`memory.projection_enabled`, default **off**)

## Config

```json
{
  "memory": {
    "projection_enabled": false
  }
}
```

Tracked default: `config_master.json`. Projection never becomes a sync mechanism — regenerate from SurrealDB only.

## Runtimes and sharing

| Runtime | Where memory lives | Share with another runtime |
|---------|--------------------|----------------------------|
| `personal` | Personal data dir / SurrealDB | Shared by default for that host |
| `plugin` (isolated spawn) | `%LOCALAPPDATA%\ibis\plugin` (or `IBIS_DATA_DIR`) | `memory_export` → `memory_import`, or attach via `IBIS_MCP_URL` |
| `server` | Server SurrealDB | Same instance = shared team memory |

Prefer one personal HTTP daemon + `IBIS_MCP_URL` when Cursor and VS Code both need the same memory table. Details: [runtime_modes.md](runtime_modes.md), [ide_host_and_lm_bridge.md](ide_host_and_lm_bridge.md).

## IDE commands

- Cursor: **Ibis: Remember**, **Ibis: Set working state** (plus hooks that guide `load_session_context` / sparse `capture_observation`)
- VS Code: same command titles via the extension

## Related

- Hybrid retrieval / AI: [ai_and_settings.md](ai_and_settings.md)
- Historical article mapping (pre–typed layer): [report_memory_evaluation.md](report_memory_evaluation.md)
- Historical cognitive notes: [spec_cognitive_architecture.md](spec_cognitive_architecture.md)
- Changelog: [changelog_20261009.md](changelog_20261009.md)
