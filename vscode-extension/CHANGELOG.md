# Changelog

All notable changes to the **Ibis Assistant for VS Code** extension are documented here.
The runtime changelog lives in the [repository root](https://github.com/fatterdev/ibis-assistant/blob/master/CHANGELOG.md).

## [1.0.0] — Initial release

VS Code packaging for the Ibis Assistant runtime (Cursor remains the primary IDE packaging).

- **MCP server provider** — registers `ibis-assistant` for VS Code agent mode (VS Code 1.101+), spawning an isolated `ibis-assistant -mode stdio -runtime-mode plugin` child or attaching to a shared personal runtime over Streamable HTTP.
- **IDE LM bridge** — wires `vscode.lm` into the runtime's loopback bridge (`IBIS_LM_BRIDGE_URL`, default port 39201) with optional model-family filter; embeddings stay on Ollama.
- **Commands** — **Ibis: Open Settings**, **Ibis: Open Extension Settings (VS Code)**, **Ibis: Show Output**, **Ibis: Status**, **Ibis: Remember**, **Ibis: Set working state**.
- **Extensive settings** — 22 `ibis.*` settings covering runtime (spawn/attach, binary, data dir, extra args, auto-attach probes), MCP provider, LM bridge, Settings UI probing, memory defaults, status output, and verbose logging, with environment-variable fallbacks (`IBIS_MCP_URL`, `IBIS_BIN`, `IBIS_DATA_DIR`, `IBIS_WORKSPACE`, `IBIS_LM_BRIDGE_PORT`, `IBIS_LM_BRIDGE_URL`, `IBIS_SETTINGS_URL`).
- **Diagnostics** — dedicated "Ibis Assistant" output channel logging activation, backend resolution, and status.
- **Official icon** — the Ibis logo (128×128) and gallery banner matching the Cursor plugin.
- **Packaging** — vendored `ide-host` (generated at package time via `vscode:prepublish`), correct `.vscodeignore`, and `vsce package --no-dependencies` so the VSIX is self-contained and installable.
