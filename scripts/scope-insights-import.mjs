#!/usr/bin/env node
// D-INSIGHTS: import Microsoft Scope insights into Threski typed memory.
// Pull (GET /api/v1/insights) -> classify -> memory_add over Streamable HTTP MCP.
// Dedup via source_hash; --sync reconciles lifecycle (blocked/downvoted -> memory_delete).
// Tool arg names must be verified against the live Threski registry at D4 (see PLAN.md).
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { createHash } from "node:crypto";
import { randomUUID } from "node:crypto";

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i++) {
    const m = argv[i].match(/^--([a-z-]+)(?:=(.*))?$/);
    if (!m) continue;
    args[m[1]] = m[2] ?? argv[++i];
  }
  return args;
}

const args = parseArgs(process.argv);
const env = existsSync("evaluations/scope/.env")
  ? Object.fromEntries(readFileSync("evaluations/scope/.env", "utf8").split(/\r?\n/).filter((l) => l.includes("=")).map((l) => [l.slice(0, l.indexOf("=")).trim(), l.slice(l.indexOf("=") + 1).trim()]))
  : {};
const SCOPE_URL = args["scope-url"] ?? process.env.SCOPE_URL ?? env.SCOPE_URL;
const SCOPE_KEY = args["scope-key"] ?? process.env.SCOPE_API_KEY ?? env.SCOPE_API_KEY;
const MCP_URL = args["mcp-url"] ?? process.env.THRESKI_MCP_URL ?? env.THRESKI_MCP_URL ?? "http://127.0.0.1:3030/mcp";
const MIN_UPVOTES = Number(args["min-upvotes"] ?? 1);
const VISIBILITY = args.visibility ?? "team";
const KIND_OVERRIDE = args.kind ?? null;
const DRY_RUN = args["dry-run"] != null;
const SYNC = args.sync != null;
const STATE_PATH = args.state ?? ".scope-import-state.json";
if (!SCOPE_URL || !SCOPE_KEY || !MCP_URL) {
  console.error("Usage: node scripts/scope-insights-import.mjs --scope-url <url> --scope-key <key> [--mcp-url url] [--min-upvotes 1] [--visibility team] [--kind gotcha] [--dry-run] [--sync]");
  process.exit(1);
}

const scopeGet = async (path) => {
  const res = await fetch(`${SCOPE_URL}/api/v1${path}`, { headers: { Authorization: `Bearer ${SCOPE_KEY}` } });
  if (!res.ok) throw new Error(`GET ${path} -> ${res.status}: ${await res.text()}`);
  return res.json();
};

let sessionId = null;
async function mcp(method, params, isNotification = false) {
  const body = { jsonrpc: "2.0", method, ...(isNotification ? {} : { id: randomUUID() }), ...(params ? { params } : {}) };
  const res = await fetch(MCP_URL, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
      ...(sessionId ? { "Mcp-Session-Id": sessionId } : {}),
    },
    body: JSON.stringify(body),
  });
  const sid = res.headers.get("mcp-session-id");
  if (sid) sessionId = sid;
  const ctype = res.headers.get("content-type") ?? "";
  const raw = await res.text();
  if (!isNotification) {
    let payload = null;
    if (ctype.includes("text/event-stream")) {
      const dataLine = raw.split(/\r?\n/).filter((l) => l.startsWith("data:")).pop();
      payload = dataLine ? JSON.parse(dataLine.slice(5).trim()) : null;
    } else if (raw.trim()) {
      payload = JSON.parse(raw);
    }
    if (!res.ok || payload?.error) throw new Error(`MCP ${method} failed: ${JSON.stringify(payload?.error ?? res.status)}`);
    return payload?.result;
  }
  return null;
}

const callTool = async (name, args2) => {
  const result = await mcp("tools/call", { name, arguments: args2 });
  const text = (result?.content ?? []).filter((c) => c.type === "text").map((c) => c.text).join("\n");
  return { result, text };
};

async function mcpInit() {
  await mcp("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "threski-scope-import", version: "0.1.0" },
  });
  await mcp("notifications/initialized", undefined, true);
}

function classify(text) {
  if (KIND_OVERRIDE) return KIND_OVERRIDE;
  const t = text.toLowerCase();
  if (/(misus|wrong tool|used the wrong|should not have called|called .* incorrectly)/.test(t)) return "gotcha";
  if (/(missing|not supported|should support|would need|capability)/.test(t)) return "design";
  if (/(fix|resolved|workaround)/.test(t)) return "fix";
  if (/(constraint|must not|boundary|requires)/.test(t)) return "constraint";
  return "gotcha";
}

const state = existsSync(STATE_PATH) ? JSON.parse(readFileSync(STATE_PATH, "utf8")) : { imported: {}, removed: [] };
const insights = await scopeGet("/insights");
const list = Array.isArray(insights) ? insights : insights.items ?? insights.insights ?? [];
const usable = list.filter((i) => {
  const upvotes = i.upvotes ?? i.upvoteCount ?? 0;
  const blocked = i.blocked ?? i.isBlocked ?? false;
  return !blocked && upvotes >= MIN_UPVOTES;
});

await mcpInit();
let added = 0;
for (const insight of usable) {
  const id = String(insight.id ?? insight.insightId);
  const text = (insight.text ?? insight.content ?? insight.summary ?? "").trim();
  if (!text) continue;
  if (state.imported[id]) {
    console.log(`skip ${id} (already imported)`);
    continue;
  }
  const sourceHash = createHash("sha256").update(`scope:insight:${id}`).digest("hex");
  const kind = classify(text);
  const title = text.length > 80 ? `${text.slice(0, 77)}...` : text;
  const content = `${text}\n\n[scope:insight:${id} upvotes=${insight.upvotes ?? insight.upvoteCount ?? 0}]`;
  if (DRY_RUN) {
    console.log(`[dry-run] would add ${kind}: "${title}" (${VISIBILITY})`);
    continue;
  }
  const { text: resultText } = await callTool("memory_add", { kind, title, content, visibility: VISIBILITY, source_hash: sourceHash });
  state.imported[id] = { kind, title, upvotes: insight.upvotes ?? 0, importedAt: new Date().toISOString(), result: resultText.slice(0, 200) };
  added += 1;
  console.log(`added ${kind}: "${title}"`);
}

if (SYNC) {
  const byId = new Map(list.map((i) => [String(i.id ?? i.insightId), i]));
  for (const [id, meta] of Object.entries(state.imported)) {
    const insight = byId.get(id);
    const blocked = insight ? insight.blocked ?? insight.isBlocked ?? false : true;
    const upvotes = insight ? insight.upvotes ?? insight.upvoteCount ?? 0 : -1;
    if (!blocked && upvotes >= 0) continue;
    if (DRY_RUN) {
      console.log(`[dry-run] would delete memory for insight ${id} (${blocked ? "blocked" : "downvoted"})`);
      continue;
    }
    const search = await callTool("memory_search", { query: meta.title, limit: 5 });
    const matchLine = (search.result?.content ?? []).map((c) => c.text).join("\n").match(/id["\s:]+([A-Za-z0-9:_-]+)/);
    if (matchLine) {
      await callTool("memory_delete", { memory_id: matchLine[1] });
      delete state.imported[id];
      state.removed.push({ id, reason: blocked ? "blocked" : "downvoted", at: new Date().toISOString() });
      console.log(`deleted memory for insight ${id} (${blocked ? "blocked" : "downvoted"})`);
    } else {
      console.log(`warn: no memory match for insight ${id}, not deleting`);
    }
  }
}

writeFileSync(STATE_PATH, JSON.stringify(state, null, 2));
console.log(`Done: ${added} imported, state in ${STATE_PATH}${DRY_RUN ? " (dry run)" : ""}`);