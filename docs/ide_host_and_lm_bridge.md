# IDE host, shared runtime, and IDE-provided models

**Date:** 2026-09-20  
**Status:** Implemented across stacked PRs (ephemeral Settings, Cursor plugin surface, `ide-host`, LM bridge, VS Code extension)  

> **F0.5 update (2026-10-29):** the IDE-provided-models reasoning primary is
> now MCP sampling (`sampling/createMessage` over the MCP session); the LM
> bridge is deprecated and only serves as chain fallback
> (`sampling → lm_bridge → ollama`, `THRESKI_LM_BRIDGE_*`).

## Summary

Threski does **not** load Go in-process into Cursor or VS Code (no `c-shared` / FFI). A thin Node **ide-host** optionally sits between the IDE and the Go binary:

- **Default:** each IDE spawns its own `threski -mode stdio -runtime-mode plugin` child.
- **Shared runtime (optional):** both IDEs attach to one **personal** HTTP daemon via `THRESKI_MCP_URL=http://127.0.0.1:3030/mcp`.
- **IDE-provided LLM:** primary via is now **MCP sampling** (`sampling/createMessage` on the MCP session — F0.5, default `ai.reasoning.provider=ide_sampling` in plugin mode). The loopback LM bridge (`THRESKI_LM_BRIDGE_URL`, default `http://127.0.0.1:39201`) is **deprecated but still working**: it remains only as the middle fallback of the `sampling → lm_bridge → ollama` chain.

## Architecture

```mermaid
flowchart TB
  Cursor[Cursor plugin]
  VSCode[VS Code extension]
  Host[ide-host]
  Personal[Personal daemon :3030]
  ChildA[stdio child A]
  ChildB[stdio child B]
  Bridge[LM bridge :39201]
  Cursor -->|default| ChildA
  VSCode -->|default| ChildB
  Cursor -->|THRESKI_MCP_URL| Personal
  VSCode -->|THRESKI_MCP_URL| Personal
  Host --> Bridge
  ChildA --> Bridge
  Personal --> Bridge
```

## Runtime uniqueness

| Scenario | Unique process / DB? | Notes |
|----------|----------------------|-------|
| Cursor only, default `mcp.json` | Yes (one child) | Plugin data dir |
| Cursor + VS Code, both default spawn | **No** | Two children; **never** share one SurrealDB silently |
| Both set `THRESKI_MCP_URL` → personal `/mcp` | **Yes** | Preferred shared path |
| One owns stdio, other uses host-exported HTTP MCP | Stretch / future | Documented only |

Env:

| Variable | Meaning |
|----------|---------|
| `THRESKI_MCP_URL` | When set, ide-host attaches to Streamable HTTP MCP instead of spawning |
| `THRESKI_DATA_DIR` | Plugin data dir (isolated spawn only) |
| `THRESKI_LM_BRIDGE_URL` | Go → host LM RPC (default `http://127.0.0.1:39201`) |
| `THRESKI_BIN` | Override Go executable for spawn |

## Settings UI

- Personal: `http://127.0.0.1:<port>/settings/`
- Plugin stdio: `settings_open_ui` reuses personal when reachable, else **ephemeral** loopback + `?token=`
- Toggle: **Use IDE-provided models when available** → `ai.reasoning.use_ide_provided_models`
- Memory browser + optional **Write memories to `.threski/memory/*.md`** (`memory.projection_enabled`, default off). SurrealDB remains source of truth. Operator guide: [memory_layer.md](memory_layer.md).

## Shared memory with two IDEs

Typed memory (`memory_*`, `load_session_context`, `capture_observation`, …) is stored in whichever SurrealDB the attached runtime owns.

- **Recommended:** personal HTTP daemon + `THRESKI_MCP_URL` so Cursor and VS Code share one memory table (and one consolidation backlog).
- **Isolated plugin children:** each IDE has its own DB; use `memory_export` / `memory_import` to transfer, or promote durable notes via `memory_share` after import.
- Actor attribution: HTTP header `X-Threski-Actor` or tool arg `actor` (stdio/plugin).

## Packages

| Path | Role |
|------|------|
| [`cursor-plugin/`](../cursor-plugin/) | Cursor: MCP, commands, rules, skill, hook, STATUS |
| [`ide-host/`](../ide-host/) | Shared spawn / attach / LM RPC |
| [`vscode-extension/`](../vscode-extension/) | VS Code packaging |
| [`internal/ai/cursor_lm.go`](../internal/ai/cursor_lm.go) | Go bridge client |

## Explicitly rejected / later

| Item | Status |
|------|--------|
| Go in-process via FFI | Rejected |
| Marketplace / Open VSX publish CI | Later (local VSIX first) |
| Native IDE Settings webview | Later (browser ephemeral enough) |
| OAuth MCP, mcp-bridge off SSE | Unchanged out of scope |

## Related

- [runtime_modes.md](runtime_modes.md)
- [ai_and_settings.md](ai_and_settings.md)
- [cursor-plugin/STATUS.md](../cursor-plugin/STATUS.md)
