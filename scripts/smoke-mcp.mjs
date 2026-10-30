#!/usr/bin/env node
// REVIEW R2 protocol smoke — the five gate items verified over the REAL
// binary on stdio (the same NDJSON surface VS Code drives):
//   1. zero read-only confirmations  -> tools/list annotations (readOnlyHint + title)
//   2. /ibis.* in chat               -> prompts/list + prompts/get + completion/complete
//   3. resources in Add Context      -> resources/list + templates + resources/read
//   4. sampling attivo               -> sampling/createMessage round-trip via ask_project
//   5. MCP App + fallback testuale   -> ui://ibis/memory-curator read + _meta.ui.resourceUri
//                                      on memory tools + text always present
//
// Usage: node scripts/smoke-mcp.mjs [path-to-ibis.exe] [workspace-dir]
// Exit code 0 = all checks green; 1 = at least one check failed.
//
// Environment: isolated IBIS_DATA_DIR + dedicated SurrealDB port, so the run
// never touches the developer's personal store. EMBEDDING_PROVIDER=none
// disables the vector lane: memory_search is expected to degrade (still a
// tool result, never a protocol error); memory_add writes without embeddings.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = path.resolve(process.argv[2] ?? path.join(repoRoot, "target", "debug", "ibis.exe"));
const workspace = path.resolve(process.argv[3] ?? repoRoot);

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const port = srv.address().port;
      srv.close(() => resolve(port));
    });
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const dataDir = mkdtempSync(path.join(os.tmpdir(), "ibis-r2-smoke-"));
const surrealPort = await getFreePort();

const skipEnv = ["IBIS_WORKSPACE", "CURSOR_WORKSPACE_FOLDER", "CURSOR_WORKSPACE", "VSCODE_WORKSPACE_FOLDER"];
const env = {};
for (const [k, v] of Object.entries(process.env)) {
  if (!skipEnv.includes(k)) env[k] = v;
}
Object.assign(env, {
  RUNTIME_MODE: "plugin",
  IBIS_DATA_DIR: dataDir,
  EMBEDDING_PROVIDER: "ollama", // the VS Code reality; ollama serve must be up on :11434
  DB_AUTO_UPDATE: "false",
  LOG_LEVEL: "INFO",
  SURREAL_URL: `ws://127.0.0.1:${surrealPort}/rpc`,
});

const child = spawn(
  exe,
  ["run", "--mode", "stdio", "--runtime-mode", "plugin", "--scan", "false"],
  {
    // Empty cwd (like the e2e + a real plugin spawn): a repo-bound cwd would
    // pre-bind the workspace and skip the roots/list binding flow entirely.
    cwd: os.tmpdir(),
    env,
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  }
);

const stderr = [];
child.stderr.on("data", (d) => stderr.push(d.toString()));

const pending = new Map(); // id -> {resolve, timer}
const serverRequests = new Map(); // method -> {id, params} (last wins)
const notifications = [];
let samplingCalls = 0;
let nextId = 1;

function send(obj) {
  child.stdin.write(JSON.stringify(obj) + "\n");
}

function handleServerRequest(msg) {
  serverRequests.set(msg.method, { id: msg.id, params: msg.params });
  if (msg.method === "roots/list") {
    // The plugin workspace binding: the repo root becomes the workspace.
    const uri = `file:///${workspace.replace(/\\/g, "/")}`;
    send({ jsonrpc: "2.0", id: msg.id, result: { roots: [{ uri, name: "r2-smoke" }] } });
    return true;
  }
  if (msg.method === "sampling/createMessage") {
    samplingCalls += 1;
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        role: "assistant",
        content: { type: "text", text: "FINAL ANSWER: sampled via mcp" },
        model: "vscode-claude",
      },
    });
    return true;
  }
  return false; // kept in serverRequests for waitForServerRequest
}

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
      continue; // non-JSON stdout noise
    }
    if (msg.method && msg.id !== undefined) {
      if (handleServerRequest(msg)) continue;
    } else if (msg.method) {
      notifications.push(msg);
    } else if (msg.id !== undefined && pending.has(msg.id)) {
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
    send({ jsonrpc: "2.0", id, method, params });
  });
}

async function waitForServerRequest(method, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (serverRequests.has(method)) return serverRequests.get(method);
    await sleep(50);
  }
  throw new Error(`server never sent ${method} within ${timeoutMs}ms`);
}

function lastNotification(method) {
  for (let i = notifications.length - 1; i >= 0; i--) {
    if (notifications[i].method === method) return notifications[i];
  }
  return null;
}

// F0.2 — the 15 core read-only tools (ticket_* reads stay hidden while
// ticketing is unconfigured; F0.6 dynamic discovery).
const EXPECTED_READONLY = [
  "analyze_blast_radius",
  "as_of_project",
  "ask_project",
  "find_dead_code",
  "get_rules",
  "get_working_state",
  "knowledge_diff",
  "load_session_context",
  "memory_expand",
  "memory_history",
  "memory_insights",
  "memory_search",
  "recent_failures",
  "settings_get",
  "why_changed",
];

try {
  // ————— MCP handshake (sampling-capable client, like VS Code) —————
  const init = await request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: { sampling: {}, roots: { listChanged: true } },
    clientInfo: { name: "ibis-r2-smoke", version: "0.0.0" },
  });
  check(
    "MCP initialize (stdio)",
    init.result?.serverInfo?.name === "Ibis Assistant",
    JSON.stringify(init.result?.serverInfo)
  );
  send({ jsonrpc: "2.0", method: "notifications/initialized" });
  await waitForServerRequest("roots/list", 15000); // plugin binding hook

  // ————— R2.1 zero read-only confirmations: annotations on tools/list —————
  const tools = await request("tools/list", {});
  const list = tools.result?.tools ?? [];
  const names = list.map((t) => t.name);
  const ticketTools = names.filter((n) => n.startsWith("ticket_"));
  check(
    "F0.6 dynamic discovery: core surface only (no ticket_*)",
    ticketTools.length === 0 && list.length > 0,
    `${list.length} tools`
  );
  const missingAnnotations = list.filter(
    (t) =>
      !t.annotations ||
      typeof t.annotations.title !== "string" ||
      t.annotations.title.length === 0 ||
      typeof t.annotations.readOnlyHint !== "boolean"
  );
  check(
    "F0.2 every tool carries annotations {title, readOnlyHint}",
    missingAnnotations.length === 0,
    missingAnnotations.map((t) => t.name).join(",") || "all annotated"
  );
  const readonlyNames = list
    .filter((t) => t.annotations?.readOnlyHint === true)
    .map((t) => t.name)
    .sort();
  const expected = [...EXPECTED_READONLY].sort();
  const roDiff = readonlyNames
    .filter((n) => !expected.includes(n))
    .concat(expected.filter((n) => !readonlyNames.includes(n)));
  check(
    "F0.2 read-only set matches the pure-read surface",
    roDiff.length === 0,
    roDiff.join(",") || `${readonlyNames.length} read-only tools`
  );
  const badMutators = list
    .filter((t) => !EXPECTED_READONLY.includes(t.name))
    .filter((t) => t.annotations?.readOnlyHint !== false)
    .map((t) => t.name);
  check("F0.2 mutators declare readOnlyHint: false", badMutators.length === 0, badMutators.join(","));

  // ————— R2.2 /ibis.* prompts —————
  const prompts = await request("prompts/list", {});
  const promptNames = (prompts.result?.prompts ?? []).map((p) => p.name).sort();
  check(
    "F0.3 prompts/list = the 5 slash prompts",
    JSON.stringify(promptNames) === JSON.stringify(["blast_radius", "readiness", "remember", "status", "why_changed"]),
    promptNames.join(", ")
  );
  const gotPrompt = await request("prompts/get", { name: "readiness", arguments: {} });
  const promptText = gotPrompt.result?.messages?.[0]?.content?.text ?? "";
  check(
    "F0.3 prompts/get readiness renders the session-contract text",
    promptText.includes("Ibis") && promptText.length > 200,
    `${promptText.length} chars`
  );
  const completion = await request("completion/complete", {
    ref: { type: "ref/prompt", name: "remember" },
    argument: { name: "kind", value: "dec" },
  });
  const completionValues = completion.result?.completion?.values ?? [];
  check(
    "F0.3 argument completion (remember/kind 'dec' -> decision)",
    completionValues.includes("decision"),
    completionValues.join(", ")
  );

  // ————— R2.3 resources in Add Context —————
  const resources = await request("resources/list", {});
  const resList = resources.result?.resources ?? [];
  const uris = resList.map((r) => r.uri);
  check("resources/list announces the guide", uris.includes("ibis://guide"), uris.join(", "));
  const curator = resList.find((r) => r.uri === "ui://ibis/memory-curator");
  check(
    "F0.7 MCP App resource declared (SEP-1865 mime + _meta.ui)",
    curator?.mimeType === "text/html;profile=mcp-app" && !!curator?._meta?.ui,
    curator ? `mime=${curator.mimeType}` : "missing"
  );
  const templates = await request("resources/templates/list", {});
  const templateUris = (templates.result?.resourceTemplates ?? []).map((t) => t.uriTemplate);
  check(
    "F0.4 templates memory/rules/ticket",
    JSON.stringify(templateUris.slice().sort()) ===
      JSON.stringify(["ibis://memory/{kind}/{id}", "ibis://rules/{name}", "ibis://ticket/{id}"]),
    templateUris.join(", ")
  );
  const guide = await request("resources/read", { uri: "ibis://guide" });
  const guideText = guide.result?.contents?.[0]?.text ?? "";
  check(
    "resources/read ibis://guide (markdown)",
    guide.result?.contents?.[0]?.mimeType === "text/markdown" && guideText.includes("ibis"),
    `${guideText.length} chars`
  );
  const curatorDoc = await request("resources/read", { uri: "ui://ibis/memory-curator" });
  const curatorHtml = curatorDoc.result?.contents?.[0]?.text ?? "";
  check(
    "F0.7 resources/read ui://ibis/memory-curator (html + loopback CSP)",
    curatorDoc.result?.contents?.[0]?.mimeType === "text/html;profile=mcp-app" &&
      curatorHtml.includes("Content-Security-Policy") &&
      curatorHtml.length > 500,
    `${curatorHtml.length} chars`
  );

  // ————— bind workspace + wait for the embedded DB —————
  let bound = false;
  const bindDeadline = Date.now() + 150000;
  while (Date.now() < bindDeadline) {
    const resp = await request(
      "tools/call",
      { name: "get_working_state", arguments: { project_name: "r2-smoke", branch: "main" } },
      20000
    );
    const text = resp.result?.content?.[0]?.text ?? "";
    if (text.includes("database not connected")) {
      await sleep(800);
      continue;
    }
    try {
      if (JSON.parse(text)?.found === false) bound = true;
    } catch {
      /* keep waiting */
    }
    if (bound) break;
  }
  check("workspace bind + embedded DB reachable (get_working_state)", bound, "found=false on fresh DB");

  // ————— R2.5 MCP App ride-along + guaranteed textual fallback —————
  const added = await request("tools/call", {
    name: "memory_add",
    arguments: {
      content: "R2 smoke entry — MCP App ui ride-along must never replace the text block",
      kind: "fact",
      title: "R2 smoke entry",
      project_name: "r2-smoke",
    },
  });
  const addText = added.result?.content?.[0]?.text ?? "";
  let addedId = null;
  try {
    addedId = JSON.parse(addText).id;
  } catch {
    /* handled below */
  }
  check(
    "F0.7 memory_add: text JSON present (guaranteed fallback)",
    addedId !== null && JSON.parse(addText).status === "created",
    addText.slice(0, 80)
  );
  check(
    "F0.7 memory_add: _meta.ui.resourceUri rides along",
    added.result?._meta?.ui?.resourceUri === "ui://ibis/memory-curator",
    JSON.stringify(added.result?._meta ?? null)
  );

  // template read of the added memory (what Add Context resolves)
  const memUri = `ibis://memory/fact/${addedId}`;
  const memRead = await request("resources/read", { uri: memUri });
  const memReadText = memRead.result?.contents?.[0]?.text ?? "";
  check(
    "F0.4 resources/read ibis://memory/fact/<id> serves the memory",
    !memRead.error && memReadText.includes("R2 smoke entry"),
    memRead.error?.message ?? `${memReadText.length} chars`
  );

  // F0.4 live update: subscribe to the concrete URI, mutate, expect the push
  const sub = await request("resources/subscribe", { uri: memUri });
  check("F0.4 resources/subscribe accepted", !sub.error, sub.error?.message ?? "subscribed");
  await request("tools/call", {
    name: "memory_update",
    arguments: { id: addedId, content: "R2 smoke entry — updated for the live-update lane" },
  });
  let sawUpdate = false;
  for (let i = 0; i < 40 && !sawUpdate; i++) {
    const n = lastNotification("notifications/resources/updated");
    sawUpdate = n && n.params?.uri === memUri;
    if (!sawUpdate) await sleep(200);
  }
  check("F0.4 notifications/resources/updated pushed on memory_update", !!sawUpdate, lastNotification("notifications/resources/updated")?.params?.uri ?? "none");

  // memory_search degrades cleanly in the embeddings-free smoke env
  const searched = await request("tools/call", {
    name: "memory_search",
    arguments: { query: "R2 smoke entry", project_name: "r2-smoke" },
  });
  const searchText = searched.result?.content?.[0]?.text ?? "";
  check(
    "memory_search stays a tool result (never a protocol error)",
    searched.result !== undefined && searched.error === undefined,
    searched.result?.isError ? `degraded: ${searchText.slice(0, 90)}` : "results served"
  );

  // ————— R2.4 sampling attivo —————
  const asked = await request("tools/call", { name: "ask_project", arguments: { query: "why did init change?" } }, 90000);
  await waitForServerRequest("sampling/createMessage", 1000); // answered inline by the handler
  const askText = asked.result?.content?.[0]?.text ?? "";
  check(
    "F0.5 sampling/createMessage served the agentic lane",
    samplingCalls === 1 && askText.includes("sampled via mcp") && askText.includes("answer"),
    `sampling calls=${samplingCalls}`
  );
  const askTool = list.find((t) => t.name === "ask_project");
  check(
    "R2.1 evidence: ask_project is readOnlyHint: true (no confirmation in VS Code)",
    askTool?.annotations?.readOnlyHint === true
  );

  // ————— clean exit on stdin close —————
  child.stdin.end();
  const exit = await new Promise((resolve) => {
    const t = setTimeout(() => resolve(null), 60000);
    child.on("exit", (code) => {
      clearTimeout(t);
      resolve(code);
    });
  });
  check("clean exit on stdin close (like VS Code closing the session)", exit === 0, `exit=${exit}`);
} catch (err) {
  check("smoke sequence completed", false, err.message);
} finally {
  child.kill();
  try {
    rmSync(dataDir, { recursive: true, force: true });
  } catch {
    /* best effort */
  }
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
if (failed.length && stderr.length) {
  console.log("\n--- stderr tail ---\n" + stderr.join("").slice(-2000));
}
process.exit(failed.length ? 1 : 0);