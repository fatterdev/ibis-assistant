import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { callMcpTool, disposeMcpSessions } from "./index.js";

/**
 * Fake MCP stdio server (spawned through the real backend path):
 * - appends one "spawn" line per child start to the spawn marker;
 * - appends one line per request (method name) to the request log;
 * - `die-first` mode: the very first spawned child exits before answering
 *   initialize, later children serve normally (dead-child retry scenario).
 */
const SCRIPT = `
const fs = require("node:fs");
// node -e puts the extra CLI args at argv[1..] (no script path slot).
const [marker, log, mode] = process.argv.slice(1);
const spawnsBefore = fs.existsSync(marker)
  ? fs.readFileSync(marker, "utf8").trim().split(/\\r?\\n/).filter(Boolean).length
  : 0;
fs.appendFileSync(marker, "spawn\\n");
if (mode === "die-first" && spawnsBefore === 0) {
  process.exit(0);
}
let buf = "";
const logLine = (s) => fs.appendFileSync(log, s + "\\n");
function reply(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id, result }) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buf += chunk.toString("utf8");
  let idx;
  while ((idx = buf.indexOf("\\n")) !== -1) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.id == null) continue;
    logLine(msg.method);
    if (msg.method === "initialize") {
      reply(msg.id, { protocolVersion: "2024-11-05", capabilities: {}, serverInfo: { name: "fake", version: "0" } });
    } else if (msg.method === "tools/call") {
      reply(msg.id, { content: [{ type: "text", text: "pong:" + msg.params.name }] });
    }
  }
});
`;

function backendFor(dir, mode) {
  return {
    mode: "spawn",
    command: process.execPath,
    args: [
      "-e",
      SCRIPT,
      join(dir, `spawns-${mode}.txt`),
      join(dir, `log-${mode}.jsonl`),
      mode,
    ],
    env: { ...process.env },
  };
}

function readLines(file) {
  try {
    return readFileSync(file, "utf8").split(/\r?\n/).filter(Boolean);
  } catch {
    return [];
  }
}

const WAIT = { timeoutMs: 20_000 };

// 1. Two calls share one child and one handshake.
{
  const dir = mkdtempSync(join(tmpdir(), "ibis-session-"));
  try {
    const backend = backendFor(dir, "serve");
    const first = await callMcpTool(backend, "memory_list", { kind: "decision" }, WAIT);
    const second = await callMcpTool(backend, "memory_search", { query: "x" }, WAIT);
    assert.equal(first, "pong:memory_list");
    assert.equal(second, "pong:memory_search");
    assert.equal(
      readLines(join(dir, "spawns-serve.txt")).length,
      1,
      "two calls must spawn exactly one child",
    );
    const reqs = readLines(join(dir, "log-serve.jsonl"));
    assert.equal(
      reqs.filter((l) => l === "initialize").length,
      1,
      "two calls must pay one handshake",
    );
    assert.equal(reqs.filter((l) => l.startsWith("tools/call")).length, 2);
  } finally {
    disposeMcpSessions();
    rmSync(dir, { recursive: true, force: true });
  }
}

// 2. disposeMcpSessions forces a fresh child on the next call.
{
  const dir = mkdtempSync(join(tmpdir(), "ibis-session-"));
  try {
    const backend = backendFor(dir, "serve2");
    await callMcpTool(backend, "memory_list", {}, WAIT);
    disposeMcpSessions();
    await callMcpTool(backend, "memory_list", {}, WAIT);
    assert.equal(
      readLines(join(dir, "spawns-serve2.txt")).length,
      2,
      "dispose must respawn on the next call",
    );
  } finally {
    disposeMcpSessions();
    rmSync(dir, { recursive: true, force: true });
  }
}

// 3. A child dying during initialize is retried once on a fresh child.
{
  const dir = mkdtempSync(join(tmpdir(), "ibis-session-"));
  try {
    const backend = backendFor(dir, "die-first");
    const text = await callMcpTool(backend, "memory_list", {}, WAIT);
    assert.equal(text, "pong:memory_list", "the retry child must answer");
    assert.equal(
      readLines(join(dir, "spawns-die-first.txt")).length,
      2,
      "dying first child must be retried on a fresh one",
    );
  } finally {
    disposeMcpSessions();
    rmSync(dir, { recursive: true, force: true });
  }
}

console.log("session_test: 3/3 PASS");