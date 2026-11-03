#!/usr/bin/env node
// R3.3 seed — writes 3 team memories (decision/fix/gotcha) into the REAL
// plugin data dir (%LOCALAPPDATA%\ibis\plugin, the same store the VSIX tree
// view reads) through the same spawn contract the plugin uses
// (`ibis -mode stdio -runtime-mode plugin`), then verifies with the SAME
// memory_search calls the tree view makes (kind-word query + kind filter).
//
// Why: the memory tree is empty until R3.4 creates memories — the seed makes
// phrases B/C of R3.3 verifiable without doing R3.4 first. Each memory_search
// call is wall-clock timed: a cold/warm datapoint BEFORE the UI run.
//
// Usage:
//   node scripts/seed-memory-r33.mjs [ibis-binary] [workspace-dir]
//     - binary defaults to `ibis` on PATH (what the VSIX spawns)
//     - workspace defaults to the repo root (roots/list binding)
//   env EMBEDDING_PROVIDER=none  → degraded dry-run (rows without embedding;
//     memory_search degrades gracefully — mechanics only, NOT a valid S1.3 datapoint)
//
// Exit code 0 = seeded + verified; 1 = at least one step failed.
// Run with VS Code CLOSED (the plugin runtime must not hold the SurrealDB
// port of the shared data dir). Rows are NOT deduplicated: re-running adds
// duplicates (memory_delete cleans them).

import { spawn } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.argv[2] || "ibis";
const workspace = path.resolve(process.argv[3] || repoRoot);
const degraded = process.env.EMBEDDING_PROVIDER === "none";

const SEEDS = [
  {
    kind: "decision",
    title: "Plugin data dir isolation",
    content:
      "The plugin runtime isolates SurrealDB and plugin.log under the plugin data dir (%LOCALAPPDATA%\\ibis\\plugin) so a VS Code or Cursor child never shares state with a personal ibis install.",
  },
  {
    kind: "fix",
    title: "MCP response timeout during plugin store init",
    content:
      "Timeout waiting for MCP response id=1 during plugin store init: the ide-host response timeout was raised from 15 to 60 seconds so the first memory tree expansion can absorb the store boot.",
  },
  {
    kind: "gotcha",
    title: "math::max arity on SurrealDB 3.x",
    content:
      "math::max takes a single array on SurrealDB 3.x; the two-argument form from the 2.x era fails live.",
  },
];

let pass = 0;
let fail = 0;
function check(name, ok, detail = "") {
  if (ok) pass += 1;
  else fail += 1;
  console.log(`${ok ? "PASS" : "FAIL"} | ${name}${detail ? ` | ${detail}` : ""}`);
}

const child = spawn(exe, ["-mode", "stdio", "-runtime-mode", "plugin"], {
  cwd: os.tmpdir(),
  env: process.env, // passthrough: the REAL plugin config.json + data dir apply
  stdio: ["pipe", "pipe", "pipe"],
  windowsHide: true,
});
child.on("error", (err) => {
  console.error(`FATAL | cannot spawn ${exe}: ${err.message}`);
  console.error("        the binary must be on PATH (where.exe ibis) — see the R3.3 runbook pre-flight");
  process.exit(1);
});
const stderr = [];
child.stderr.on("data", (d) => stderr.push(d.toString()));

const pending = new Map();
let nextId = 1;
let buffer = "";
child.stdout.on("data", (d) => {
  buffer += d.toString();
  let idx;
  while ((idx = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.method === "roots/list" && msg.id !== undefined) {
      const uri = `file:///${workspace.replace(/\\/g, "/")}`;
      child.stdin.write(
        JSON.stringify({
          jsonrpc: "2.0",
          id: msg.id,
          result: { roots: [{ uri, name: "r33-seed" }] },
        }) + "\n",
      );
      continue;
    }
    if (msg.id !== undefined && pending.has(msg.id)) {
      const p = pending.get(msg.id);
      clearTimeout(p.timer);
      pending.delete(msg.id);
      p.resolve(msg);
    }
  }
});

function request(method, params, timeoutMs = 30000) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`timeout waiting for ${method} (id ${id})`));
    }, timeoutMs);
    pending.set(id, { resolve, timer });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
}

function textOf(msg) {
  const blocks = msg?.result?.content ?? [];
  return blocks
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("\n");
}

try {
  const t0 = Date.now();
  const init = await request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "ibis-r33-seed", version: "0.0.0" },
  });
  check(
    "initialize (stdio, plugin contract)",
    init.result?.serverInfo?.name === "Ibis Assistant",
    `${init.result?.serverInfo?.name ?? "?"} in ${Date.now() - t0}ms`,
  );
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");

  // The embedded SurrealDB boots asynchronously (ast-grep + surreal checks
  // first): tool calls before it is up fail with "database not connected".
  // Poll a read-only tool until the DB answers (60s cap — the R3.3 finding
  // measured a >15s first-boot store init).
  const dbReady = Date.now();
  let ready = false;
  for (let i = 0; i < 85 && !ready; i += 1) {
    await new Promise((r) => setTimeout(r, 700));
    try {
      const res = await request("tools/call", { name: "memory_list", arguments: { limit: 1 } }, 5000);
      ready = res.result?.isError !== true && !/database not connected/i.test(textOf(res));
    } catch {
      /* keep polling */
    }
  }
  check("embedded SurrealDB ready", ready, `${Date.now() - dbReady}ms — R3.3 first-boot init is the cold-start cost`);

  if (degraded) {
    console.log("NOTE | EMBEDDING_PROVIDER=none: rows land WITHOUT embedding (mechanics dry-run only)");
  }

  const created = [];
  for (const seed of SEEDS) {
    const t = Date.now();
    try {
      const res = await request("tools/call", {
        name: "memory_add",
        arguments: {
          project_name: "ibis-assistant",
          kind: seed.kind,
          title: seed.title,
          content: seed.content,
        },
      }, 60000);
      const text = textOf(res);
      const isError = res.result?.isError === true;
      check(`memory_add ${seed.kind}`, !isError && /memory:/i.test(text), `${Date.now() - t}ms — ${text.slice(0, 140)}`);
      if (!isError) created.push(seed);
    } catch (err) {
      check(`memory_add ${seed.kind}`, false, String(err.message));
    }
  }

  // The tree view expands a kind with a semantic query seeded by the kind
  // word itself: time the same shape of call (cold = first, warm = second).
  const searchTimes = [];
  for (const seed of created) {
    for (const pass_name of ["cold", "warm"]) {
      const t = Date.now();
      try {
        const res = await request("tools/call", {
          name: "memory_search",
          arguments: { query: seed.kind, kind: seed.kind, limit: 50 },
        }, 60000);
        const text = textOf(res);
        const ms = Date.now() - t;
        if (pass_name === "cold") searchTimes.push(ms);
        const isError = res.result?.isError === true;
        check(
          `memory_search ${seed.kind} (${pass_name})`,
          !isError && text.length > 0,
          `${ms}ms — ${text.split("\n")[0].slice(0, 120)}`,
        );
      } catch (err) {
        check(`memory_search ${seed.kind} (${pass_name})`, false, String(err.message));
      }
    }
  }

  if (searchTimes.length >= 2) {
    console.log(`TIMING | memory_search first call (cold) ${searchTimes[0]}ms — S1.3 target < 2000ms`);
    console.log(`TIMING | memory_search repeat (warm) ${searchTimes[1]}ms — S1.3 target < 500ms`);
    if (degraded) {
      console.log("NOTE | degraded run: timings are NOT a valid S1.3 datapoint (no vector lane)");
    }
  }

  // The kind EXPANSION (phrase B) goes through memory_list (metadata browse,
  // no embedding) — time the exact call shape the tree view makes.
  if (created.length > 0) {
    for (const [pass_name, kind] of [["cold", created[0].kind], ["warm", created[0].kind]]) {
      const t = Date.now();
      try {
        const res = await request("tools/call", {
          name: "memory_list",
          arguments: { kind, limit: 100 },
        }, 60000);
        const ms = Date.now() - t;
        check(`memory_list ${kind} (${pass_name})`, res.result?.isError !== true, `${ms}ms`);
        if (pass_name === "cold") {
          console.log(`TIMING | memory_list cold ${ms}ms — the phrase-B expansion call shape`);
        } else {
          console.log(`TIMING | memory_list warm ${ms}ms`);
        }
      } catch (err) {
        check(`memory_list ${kind} (${pass_name})`, false, String(err.message));
      }
    }
  }

  const localAppData = process.env.LOCALAPPDATA || "~/AppData/Local";
  console.log(`DATA | plugin.log: ${path.join(localAppData, "ibis", "plugin", "plugin.log")}`);
  console.log(`DATA | grep: Select-String -Path "$env:LOCALAPPDATA\\ibis\\plugin\\plugin.log" -Pattern "memory_search:" | Select-Object -Last 10`);
} finally {
  child.stdin.end();
  const code = await new Promise((resolve) => child.on("exit", resolve));
  console.log(`DATA | runtime exit code ${code} (clean exit expected)`);
}

if (fail > 0) {
  console.error(`\n${fail} check(s) failed. Log tail:\n${stderr.slice(-12).join("")}`);
  process.exit(1);
}
console.log(`\n${pass} check(s) passed.`);