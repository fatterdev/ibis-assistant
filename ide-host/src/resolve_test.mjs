import assert from "node:assert/strict";
import { resolveBackend, DEFAULT_MCP_URL } from "../src/index.js";

const spawnBackend = resolveBackend({ PATH: "/usr/bin" });
assert.equal(spawnBackend.mode, "spawn");
assert.equal(spawnBackend.command, "threski");
assert.deepEqual(spawnBackend.args, ["-mode", "stdio", "-runtime-mode", "plugin"]);

const httpBackend = resolveBackend({ THRESKI_MCP_URL: "http://127.0.0.1:3030/mcp" });
assert.equal(httpBackend.mode, "http");
assert.equal(httpBackend.url, "http://127.0.0.1:3030/mcp");
assert.equal(DEFAULT_MCP_URL, "http://127.0.0.1:3030/mcp");

console.log("ide-host resolveBackend ok");
