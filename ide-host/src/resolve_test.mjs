import assert from "node:assert/strict";
import { resolveBackend, DEFAULT_MCP_URL } from "../src/index.js";

const spawnBackend = resolveBackend({ PATH: "/usr/bin" });
assert.equal(spawnBackend.mode, "spawn");
assert.equal(spawnBackend.command, "ibis");
assert.deepEqual(spawnBackend.args, ["-mode", "stdio", "-runtime-mode", "plugin"]);

const httpBackend = resolveBackend({ IBIS_MCP_URL: "http://127.0.0.1:3030/mcp" });
assert.equal(httpBackend.mode, "http");
assert.equal(httpBackend.url, "http://127.0.0.1:3030/mcp");
assert.equal(DEFAULT_MCP_URL, "http://127.0.0.1:3030/mcp");

// Actor identity (F4 §B.4): IBIS_ACTOR rides the http backend as `actor`.
const httpActor = resolveBackend({
  IBIS_MCP_URL: "http://127.0.0.1:3030/mcp",
  IBIS_ACTOR: " alice ",
});
assert.equal(httpActor.actor, "alice");
const httpNoActor = resolveBackend({ IBIS_MCP_URL: "http://127.0.0.1:3030/mcp" });
assert.equal(httpNoActor.actor, undefined);

console.log("ide-host resolveBackend ok");
