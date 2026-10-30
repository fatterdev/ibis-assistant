# Code ingest: polyglot AST and AI rule synthesis

> **Status: IMPLEMENTED** (September 2026). See [changelog_20260918.md](changelog_20260918.md).

## Goals

- Chunk and call-graph extract via **ast-grep** (`sg`), not ad-hoc regex, whenever rules exist.
- Support **mixed / legacy** stacks (server pages hosting JS/CSS, SPA templates, ERB, …) through `languageGlobs`.
- When rules are sparse for an extension or pattern family, **synthesize validated YAML** and persist it under `rules/`.

## Components

| Piece | Location | Role |
|-------|----------|------|
| Ingest orchestration | `internal/ingest/code/ingest.go` | Git tree as source of truth; prune obsolete chunks |
| AST chunker | `ast_chunker.go` | Invokes `sg` with project `sgconfig.yml` + `rules/` |
| Sidecar install | `install.go` | Ensures `sg` is available |
| Language globs | `lang_globs.go`, root `sgconfig.yml` | Map extensions → host language for injection |
| Polyglot regions | `polyglot.go` | Bootstrap embedded script/style regions (e.g. server-page templates) |
| Sparse detection | `sparse_ast.go` | Decide when rule synthesis is needed |
| AI rule synth | `rule_synth.go` | Propose → validate → `PersistValidatedRule` |

## `sgconfig.yml` (repo root)

```yaml
ruleDirs:
  - rules
languageGlobs:
  html:
    - "*.jsp"
    - "*.erb"
    - "*.svelte"
    # … see file for full list
```

AI synthesis may call `EnsureLanguageGlob` at runtime for unfamiliar extensions.

## Generated rules

Validated outputs land as:

- `rules/ai-generated-*-chunk.yml`
- `rules/ai-generated-*-calls.yml`
- `rules/ai-generated-*-logs.yml` (from the log Path-D path)

Keep hand-written rules in `rules/` for core languages; treat AI files as additive.

## MCP / CLI

- MCP: `ingest_code` (path / project context as registered in `cmd/server/main.go`)
- CLI: `ibis ingest code --path ./path/to/project`

## Verification

- Unit tests: `rule_synth_test.go`, `ast_chunker_test.go`
- Smoke: `ingest code` on a polyglot tree (e.g. `.jsp` + script blocks) and confirm chunks without falling back to whole-file grep-only paths when rules exist
