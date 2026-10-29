# Changelog

All notable changes to the **Threski Studio for VS Code** extension are documented here.
The runtime changelog lives in the [repository root](https://github.com/fatterdev/threski-studio/blob/master/CHANGELOG.md).

## [0.2.0] — 2026-10-29

- **Rebrand to Threski Studio** — VSIX id `fatterdev.threski-studio`, settings
  prefix `threskiStudio.*`, commands "Threski Studio: *", MCP server label
  `threski`, vendored `@threski/ide-host`. The extension version stays 1.0.0
  until first publish; no functional changes beyond naming.

## [1.0.0] — Initial release

VS Code packaging for the Threski Studio runtime (Cursor remains the primary IDE packaging).

- **MCP server provider** — registers `threski` for VS Code agent mode (VS Code 1.101+), spawning an isolated `threski -mode stdio -runtime-mode plugin` child or attaching to a shared personal runtime over Streamable HTTP.
- **IDE LM bridge** — wires `vscode.lm` into the runtime's loopback bridge (`THRESKI_LM_BRIDGE_URL`, default port 39201) with optional model-family filter; embeddings stay on Ollama.
- **Commands** — **Threski Studio: Open Settings**, **Threski Studio: Open Extension Settings (VS Code)**, **Threski Studio: Show Output**, **Threski Studio: Status**, **Threski Studio: Remember**, **Threski Studio: Set working state**.
- **Extensive settings** — 22 `threskiStudio.*` settings covering runtime (spawn/attach, binary, data dir, extra args, auto-attach probes), MCP provider, LM bridge, Settings UI probing, memory defaults, status output, and verbose logging, with environment-variable fallbacks (`THRESKI_MCP_URL`, `THRESKI_BIN`, `THRESKI_DATA_DIR`, `THRESKI_WORKSPACE`, `THRESKI_LM_BRIDGE_PORT`, `THRESKI_LM_BRIDGE_URL`, `THRESKI_SETTINGS_URL`).
- **Diagnostics** — dedicated "Threski Studio" output channel logging activation, backend resolution, and status.
- **Official icon** — the Threski logo (128×128) and gallery banner matching the Cursor plugin.
- **Packaging** — vendored `ide-host` (generated at package time via `vscode:prepublish`), correct `.vscodeignore`, and `vsce package --no-dependencies` so the VSIX is self-contained and installable.
