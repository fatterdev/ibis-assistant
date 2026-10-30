# Threski IDE host

Thin Node host shared by **Cursor** and **VS Code**.

It does **not** load Go in-process. Default behavior matches plugin `mcp.json`: spawn

```text
threski -mode stdio -runtime-mode plugin
```

## Shared vs isolated runtime

| Mode | How | Process / DB |
|------|-----|----------------|
| **Default (isolated)** | Unset `THRESKI_MCP_URL` → spawn stdio child | One child per IDE; default data dir `%LOCALAPPDATA%\threski\plugin`. **Do not** run two children on the same SurrealDB. |
| **Shared (preferred)** | Run `threski run` (personal) on `127.0.0.1:3030`, set `THRESKI_MCP_URL=http://127.0.0.1:3030/mcp` | Both IDEs attach to one HTTP MCP; single process and DB. |
| **Stretch** | One IDE owns stdio; host exposes loopback MCP HTTP for the other | Future; not implemented here. |

## Usage

```bash
# Isolated (default)
node bin/threski-mcp-host.mjs

# Shared personal runtime
set THRESKI_MCP_URL=http://127.0.0.1:3030/mcp
node bin/threski-mcp-host.mjs
```

Optional `THRESKI_BIN` overrides the Go executable name/path.

### Cursor

Keep [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json) spawning `threski` directly (zero Node dependency). To use this host (shared-runtime attach), point the MCP `command` at `node` and `args` at this `bin/threski-mcp-host.mjs` (absolute path), and set `THRESKI_MCP_URL` when desired. See `cursor-plugin/mcp-host.example.json`.

### VS Code

The VS Code extension (PR5) depends on this package for spawn/attach and the LM bridge stub.

## LM bridge

`src/lm-bridge.js` + `src/lm-rpc.js`:

1. Extension host calls `attachVscodeLm(vscode)` then `startLmRpcServer()`.
2. Go sets `use_ide_provided_models` (or `provider=cursor`) and calls `http://127.0.0.1:39201` (`THRESKI_LM_BRIDGE_URL`).
3. Embeddings remain Ollama `nomic-embed-text`; context-only still applies when bridge is down.

## License

MIT
