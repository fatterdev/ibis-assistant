# Naming

**Product brand:** Ibis Assistant  
**Slug:** `ibis-assistant` (binary, Windows service, PID file, MCP client key)

| Surface | Name |
|---------|------|
| Display / docs | Ibis Assistant |
| Executable | `ibis-assistant` / `ibis-assistant.exe` |
| GitHub | `https://github.com/terenzif/ibis-assistant` |
| Future IDE plugin | Ibis Assistant for Cursor (stdio child of the same binary) |

**Runtimes** (`runtime_mode` in config; not the MCP listener `mode`):

| Runtime | Meaning |
|---------|---------|
| `personal` | App/daemon/Windows service on the same PC as the working trees |
| `server` | Dedicated host; clones under `discovery_root/dynamic` |
| `plugin` | Cursor-owned stdio child of the same binary |

`mode` (`sse` \| `stdio`) is the MCP **listener**. HTTP clients should use Streamable HTTP at `/mcp`; `/sse` is legacy. See [docs/superpowers/specs/2026-09-18-runtime-modes-design.md](superpowers/specs/2026-09-18-runtime-modes-design.md).

**AI / settings:** hybrid reasoning + Settings UX — [ai_and_settings.md](ai_and_settings.md).

**Typed memory:** sharing-first `memory_*` MCP tools (default `visibility=team`); SurrealDB source of truth; optional `memory.projection_enabled` → `.ibis/memory/*.md`. Operator guide: [memory_layer.md](memory_layer.md).

The MCP process is an implementation detail (“the MCP server”), not a separate product name.

## Remotes

- **origin:** `terenzif/ibis-assistant` (canonical)

## Local workspace folder

Prefer naming the checkout directory to match the slug (e.g. `…/ibis-assistant`). The folder name is cosmetic only.
