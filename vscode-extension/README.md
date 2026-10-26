# Ibis Assistant for VS Code

Secondary IDE packaging. **Cursor remains primary** ([`cursor-plugin/`](../cursor-plugin/)).

This extension:

- Reuses [`ide-host`](../ide-host/) to **spawn** `ibis-assistant -mode stdio -runtime-mode plugin` or **attach** to a shared personal runtime via `IBIS_MCP_URL=http://127.0.0.1:3030/mcp`
- Registers an MCP server-definition provider when the VS Code API is available
- Starts the IDE LM bridge RPC when `vscode.lm` is available (`use_ide_provided_models` on the Go side)
- Commands: **Ibis: Open Settings** (calls MCP `settings_open_ui` when personal HTTP is down), **Ibis: Status**, **Ibis: Remember**, **Ibis: Set working state**
- Typed memory: same SurrealDB as the attached runtime; prefer shared personal via `IBIS_MCP_URL`. Guide: [docs/memory_layer.md](../docs/memory_layer.md)

## Install (local VSIX)

1. Put `ibis-assistant` on `PATH`.
2. From the repo:

   ```powershell
   cd vscode-extension
   npm install
   npx --yes @vscode/vsce package
   code --install-extension .\ibis-assistant-1.0.0.vsix
   ```

   For iterative development, open this folder in an **Extension Development Host** (`F5`) instead of `code --install-extension` on the source directory (that CLI only accepts an extension id or a `.vsix` file).

3. Optional shared runtime:

   ```powershell
   ibis-assistant run   # personal on 127.0.0.1:3030
   $env:IBIS_MCP_URL = "http://127.0.0.1:3030/mcp"
   ```

## Runtime uniqueness

| Setup | Unique process? |
|-------|-----------------|
| Cursor plugin + VS Code extension, both default | **No** — two stdio children; do not share one SurrealDB |
| Both with `IBIS_MCP_URL` → personal `/mcp` | **Yes** — one personal daemon |

## Marketplace / Open VSX

Publishing to the VS Marketplace and Open VSX is **optional later**. Ship local VSIX installs first.

## Settings UI

**Ibis: Open Settings** probes personal `http://127.0.0.1:3030/settings/` when reachable; otherwise it invokes MCP `settings_open_ui` for the ephemeral tokenized loopback URL.

## License

MIT
