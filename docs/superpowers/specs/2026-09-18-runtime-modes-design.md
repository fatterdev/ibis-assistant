# Runtime modes and HTTP MCP surface

**Date:** 2026-09-18  
**Status:** Implemented (PRs #7–#10; product rename #6). Operator guide: [runtime_modes.md](../../runtime_modes.md).

## Summary

Ibis Assistant is one Go binary with three **runtimes** (`runtime_mode`). Personal and plugin use the developer working tree **directly**. Server keeps a clone under `discovery_root/dynamic`. OS symlink/junction aliases are rejected. HTTP MCP uses Streamable HTTP (`/mcp`) as the product default, keeps legacy SSE, and exposes agent autoconfig plus a human install/usage guide. Cursor plugin is an in-repo `mcpServers` host that spawns the same binary on stdio.

`mode` in `config.json` remains the **listener kind** (`sse` | `stdio`). Do not overload it.

## Runtimes

| Runtime | Process | Workspace | Knowledge store |
|---------|---------|-----------|-----------------|
| `personal` | `run` / daemon / Windows service on the same PC as the repos | Working-tree path (`projects[].working_repo_path`, else `git_repos`) | Exe-relative / `config.json` SurrealDB |
| `server` | Same binary on a dedicated host | Git clone at `discovery_root/dynamic/<name>` | Host-local SurrealDB |
| `plugin` | Cursor-owned **child**: `ibis -mode stdio` (no Windows service) | Opened Cursor folder | Isolated Cursor/user-data DB; do not silently share with a personal install |

- One artifact: `make dist` → `ibis` (+ `mcp-bridge` for Claude Desktop stdio→SSE).
- Plugin is a thin TypeScript host that **spawns** that binary. Go is not loaded in-process into Cursor.
- If personal HTTP and plugin stdio both run on one PC: **warn** (port/DB collision); do not auto-attach.
- Destructive git (`fetch` + `reset --hard`) only on **server-owned clones**. Forbidden on personal/plugin working trees.
- `sync_local_patch` is server-oriented (unpushed tips against a clone). Live trees do not need it for ordinary dirty work.

Workspace path **resolver** is implemented (`internal/workspace`, `SyncWorkspace`, log enrich, `sync_local_patch`). Live trees skip destructive git.

## Config

```json
{
  "runtime_mode": "personal",
  "bind_address": "",
  "port": 3030,
  "mode": "sse"
}
```

- `runtime_mode`: `personal` | `server` | `plugin`. Empty = unspecified (backward compatible).
- `bind_address`: optional host. Empty + `personal` → `127.0.0.1`. Empty + `server` → `0.0.0.0`. Empty + unspecified → all interfaces (`:port`, current behavior).
- Env: `RUNTIME_MODE`, `BIND_ADDRESS`.
- `projects[]` with `working_repo_path` (personal/plugin) or `url`/`branch` (server). `git_repos` remains a basename fallback.

## MCP transports

When `mode` is `sse`, the HTTP listener serves:

- **Primary:** Streamable HTTP at `/mcp` (MCP 2025-03-26 through 2026-07-28, via current mcp-go).
- **Legacy:** HTTP+SSE at `/sse` + `/message` (2024-11-05). Mark `Deprecation` on those routes. Keep them for `mcp-bridge` and old clients.
- **stdio:** `mode=stdio` (plugin and local stdio clients). No HTTP MCP.

New clients use `http://127.0.0.1:<port>/mcp`. The SDK must accept **legacy initialize** sessions and **2026-07-28** stateless/`server/discover` clients.

Security (Streamable HTTP spec):

- If `Origin` is present and not localhost (or the configured bind host), respond **403**. Missing `Origin` is allowed (CLI, many MCP clients).
- Personal default bind is loopback.

No OAuth in this revision. No stream resumability (`Last-Event-ID`).

## Long tools

Use MCP `notifications/progress` when the client sends `progressToken`. Coarse phases (not per-embedding). Rate-limit (~1s or every 25 files).

- `ingest_code` / `analyze_logs`: already blocking; add progress.
- `init_project`: **blocking**. Wait for git sync **and** code ingest on the same call. Do not `Enqueue` and return `"Ingestion started in background"`. `credentials_required` and `requires_patch` still return **before** ingest. Cancel via request `ctx` (no 3-minute git-only timer). Startup auto-index and `update_project_status` may stay queued.

## HTTP surface

Keep: `GET /`, `POST /api/v1/logs/upload`, `/sse`, `/message`, `/mcp`, auth header injection (`X-Git-Token`, ticketing headers).

Remove: `POST /api/v1/cli/call` after the CLI speaks JSON-RPC `/mcp`.

Mux lives in `internal/httpserver`. `cmd/server` keeps tool registration and process lifecycle.

### Agent autoconfig + human guide

One English source, live port/URLs interpolated.

- `GET /` + `Accept: application/json`: enough to attach (Streamable HTTP URL, legacy SSE URL, protocol versions, `runtime_mode`, bind, auth header names, tools + schemas). Also MCP `server/discover` after the SDK upgrade.
- `GET /` HTML: complete install and usage guide (run/start/service, config, personal vs server bind, Cursor/Claude snippets, CLI, log upload, main tools).
- Same guide as markdown: `Accept: text/markdown` and MCP resource `ibis://guide` so a **connected** agent can show it to a human.

Do not add a `get_setup_guide` tool unless resources are unavailable. Long tool results stay operational (progress + result), not chapters of this guide.

## CLI

The CLI is an MCP Streamable HTTP client to `/mcp` (`tools/call`, `progressToken`). Subcommands stay. Delete `cliHandlers` / `cliCallHandler`. English errors. Timeout follows request cancel, not a hard 5-minute cap that cuts off `init_project`.

## Toolchain

Language version and `toolchain` directive track the current stable Go (1.27 / `go1.27.1` at spec time). README prerequisites must match.

## Out of scope

- OAuth
- Moving `mcp-bridge` off SSE
- OS symlink/junction
- In-process Go inside Cursor
