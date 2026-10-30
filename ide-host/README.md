# Ibis IDE host

Thin Node host shared by **Cursor** and **VS Code**.

It does **not** load Go in-process. Default behavior matches plugin `mcp.json`: spawn

```text
ibis -mode stdio -runtime-mode plugin
```

## Shared vs isolated runtime

| Mode | How | Process / DB |
|------|-----|----------------|
| **Default (isolated)** | Unset `IBIS_MCP_URL` → spawn stdio child | One child per IDE; default data dir `%LOCALAPPDATA%\ibis\plugin`. **Do not** run two children on the same SurrealDB. |
| **Shared (preferred)** | Run `ibis run` (personal) on `127.0.0.1:3030`, set `IBIS_MCP_URL=http://127.0.0.1:3030/mcp` | Both IDEs attach to one HTTP MCP; single process and DB. |
| **Stretch** | One IDE owns stdio; host exposes loopback MCP HTTP for the other | Future; not implemented here. |

## Usage

```bash
# Isolated (default)
node bin/ibis-mcp-host.mjs

# Shared personal runtime
set IBIS_MCP_URL=http://127.0.0.1:3030/mcp
node bin/ibis-mcp-host.mjs
```

Optional `IBIS_BIN` overrides the Go executable name/path.

### Cursor

Keep [`cursor-plugin/mcp.json`](../cursor-plugin/mcp.json) spawning `ibis` directly (zero Node dependency). To use this host (shared-runtime attach), point the MCP `command` at `node` and `args` at this `bin/ibis-mcp-host.mjs` (absolute path), and set `IBIS_MCP_URL` when desired. See `cursor-plugin/mcp-host.example.json`.

### VS Code

The VS Code extension (PR5) depends on this package for spawn/attach and the LM bridge stub.

## LM bridge

`src/lm-bridge.js` + `src/lm-rpc.js`:

1. Extension host calls `attachVscodeLm(vscode)` then `startLmRpcServer()`.
2. Go sets `use_ide_provided_models` (or `provider=cursor`) and calls `http://127.0.0.1:39201` (`IBIS_LM_BRIDGE_URL`).
3. Embeddings remain Ollama `nomic-embed-text`; context-only still applies when bridge is down.

## License

MIT
