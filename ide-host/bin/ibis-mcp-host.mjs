#!/usr/bin/env node
/**
 * MCP entry for Cursor/VS Code: spawn plugin stdio or attach via THRESKI_MCP_URL.
 */
import { runStdioPassthrough, resolveBackend, warnDualPluginChildren } from "../src/index.js";

const backend = resolveBackend();
if (backend.mode === "spawn") {
  warnDualPluginChildren();
}
await runStdioPassthrough(backend);
