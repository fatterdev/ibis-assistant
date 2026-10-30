#!/usr/bin/env node
/**
 * R3/R4 Parte A — plugin + VSIX packaging smoke (no VS Code required).
 *
 * Checks the Agent Plugins 1.0 scaffold, the hook scripts against the verified
 * Copilot hooks contract (sessionStart -> additionalContext; agentStop ->
 * decision control only), the VSIX contribution cross-references, and the
 * distribution manifests (server.json, marketplace.json, badges).
 *
 * Usage: node scripts/smoke-plugin.mjs [repoRoot]
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = process.argv[2] || join(here, "..");
const pluginDir = join(root, "plugin");

let pass = 0;
let fail = 0;

function check(name, ok, detail = "") {
  if (ok) {
    pass += 1;
    console.log(`PASS | ${name}${detail ? ` | ${detail}` : ""}`);
  } else {
    fail += 1;
    console.log(`FAIL | ${name}${detail ? ` | ${detail}` : ""}`);
  }
}

function parseJson(file) {
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch (err) {
    return { __error: String(err && err.message ? err.message : err) };
  }
}

function runNode(script, payload, extraFiles = {}) {
  const res = spawnSync(process.execPath, [script], {
    input: JSON.stringify(payload),
    encoding: "utf8",
    timeout: 15_000,
    cwd: pluginDir,
  });
  return res;
}

// ---------- plugin.json ----------
const pluginJson = parseJson(join(pluginDir, "plugin.json"));
check(
  "plugin.json parses with name ibis + schema",
  !pluginJson.__error && pluginJson.name === "ibis" &&
    String(pluginJson.$schema || "").includes("agent-plugins.org/schemas/1.0.0"),
  pluginJson.__error || `v${pluginJson.version}`,
);

// ---------- mcp.json ----------
const mcpJson = parseJson(join(pluginDir, "mcp.json"));
const ibisServer = mcpJson.mcpServers && mcpJson.mcpServers.ibis;
check(
  "mcp.json spawn contract (ibis -mode stdio -runtime-mode plugin)",
  !mcpJson.__error && ibisServer && ibisServer.command === "ibis" &&
    JSON.stringify(ibisServer.args) === JSON.stringify(["-mode", "stdio", "-runtime-mode", "plugin"]),
  mcpJson.__error || ibisServer ? "spawn ok" : "missing ibis server entry",
);

// ---------- skills ----------
const expectedSkills = ["usage", "readiness", "status", "remember", "set-working-state"];
for (const skill of expectedSkills) {
  let body = "";
  try {
    body = readFileSync(join(pluginDir, "skills", skill, "SKILL.md"), "utf8");
  } catch {
    /* handled below */
  }
  const fmName = /^name:\s*(\S+)\s*$/m.exec(body || "");
  const hasDescription = /^description:\s*\S/m.test(body || "");
  check(
    `skill ${skill}: SKILL.md frontmatter`,
    Boolean(body) && fmName && fmName[1] === skill && hasDescription,
    fmName ? `name=${fmName[1]}` : "missing or malformed",
  );
}

// ---------- hooks ----------
const hooksJson = parseJson(join(pluginDir, "com.github.copilot", "hooks", "hooks.json"));
const sessionStartEntries = (hooksJson.hooks && hooksJson.hooks.sessionStart) || [];
const agentStopEntries = (hooksJson.hooks && hooksJson.hooks.agentStop) || [];
check(
  "hooks.json: sessionStart + agentStop command entries with ${PLUGIN_ROOT} + timeoutSec",
  !hooksJson.__error && sessionStartEntries.length === 1 && agentStopEntries.length === 1 &&
    sessionStartEntries[0].command.includes("${PLUGIN_ROOT}") &&
    agentStopEntries[0].command.includes("${PLUGIN_ROOT}") &&
    typeof sessionStartEntries[0].timeoutSec === "number" &&
    typeof agentStopEntries[0].timeoutSec === "number",
  hooksJson.__error || "v1 contract ok",
);

// ---------- hook scripts (verified Copilot contract) ----------
const sessionStart = runNode(
  join(pluginDir, "com.github.copilot", "scripts", "session-start.mjs"),
  { sessionId: "s1", cwd: root, source: "startup", timestamp: Date.now() },
);
let sessionOut = null;
try {
  sessionOut = JSON.parse(sessionStart.stdout || "{}");
} catch {
  /* parse fail */
}
check(
  "session-start.mjs: emits non-empty additionalContext (only consumed output)",
  sessionStart.status === 0 && typeof sessionOut.additionalContext === "string" &&
    sessionOut.additionalContext.includes("load_session_context"),
  sessionOut.additionalContext ? `${sessionOut.additionalContext.length} chars` : sessionStart.stderr,
);

const tmp = mkdtempSync(join(tmpdir(), "ibis-hook-smoke-"));
const transcriptNoCapture = join(tmp, "no-capture.jsonl");
writeFileSync(transcriptNoCapture, [
  JSON.stringify({ toolName: "bash", toolArgs: { command: "npm test" }, toolResult: { resultType: "success" } }),
  JSON.stringify({ toolName: "view", toolArgs: { path: "src/x.ts" } }),
].join("\n"), "utf8");
const transcriptEditNoCapture = join(tmp, "edit-no-capture.jsonl");
writeFileSync(transcriptEditNoCapture, [
  JSON.stringify({ toolName: "view", toolArgs: { path: "src/x.ts" } }),
  JSON.stringify({ toolName: "Write", toolArgs: { path: "src/x.ts" } }),
].join("\n"), "utf8");
const transcriptCaptured = join(tmp, "captured.jsonl");
writeFileSync(transcriptCaptured, [
  JSON.stringify({ toolName: "Write", toolArgs: { path: "src/x.ts" } }),
  JSON.stringify({ toolName: "capture_observation", toolArgs: { content: "lesson" } }),
].join("\n"), "utf8");

function agentStopOutcome(payload) {
  const res = runNode(join(pluginDir, "com.github.copilot", "scripts", "agent-stop.mjs"), payload);
  let out = null;
  try {
    out = JSON.parse(res.stdout || "∅");
  } catch {
    /* non-JSON */
  }
  return { res, out };
}

let oc = agentStopOutcome({ sessionId: "s", transcriptPath: transcriptCaptured });
check(
  "agent-stop.mjs: memory captured this turn -> {} (stop allowed)",
  oc.res.status === 0 && oc.out && Object.keys(oc.out).length === 0,
  JSON.stringify(oc.out),
);

let od = agentStopOutcome({
  sessionId: "s",
  transcriptPath: transcriptEditNoCapture,
  stop_hook_active: true,
});
check(
  "agent-stop.mjs: stop_hook_active=true -> {} (single forced turn guard)",
  od.res.status === 0 && od.out && Object.keys(od.out).length === 0,
  JSON.stringify(od.out),
);

let ob = agentStopOutcome({ sessionId: "s", transcriptPath: transcriptEditNoCapture });
check(
  "agent-stop.mjs: edit work without capture -> decision block + reason reminder",
  ob.res.status === 0 && ob.out && ob.out.decision === "block" &&
    typeof ob.out.reason === "string" && ob.out.reason.includes("capture_observation"),
  ob.out && ob.out.reason ? `${ob.out.reason.length} chars` : JSON.stringify(ob.out),
);

let on = agentStopOutcome({ sessionId: "s", transcriptPath: transcriptNoCapture });
check(
  "agent-stop.mjs: no durable-work signal -> {} (silent end)",
  on.res.status === 0 && on.out && Object.keys(on.out).length === 0,
  JSON.stringify(on.out),
);

let ou = agentStopOutcome({ sessionId: "s" });
check(
  "agent-stop.mjs: missing transcript -> {} (fail-open, never blocks blindly)",
  ou.res.status === 0 && ou.out && Object.keys(ou.out).length === 0,
  JSON.stringify(ou.out),
);
rmSync(tmp, { recursive: true, force: true });

// ---------- agents ----------
for (const [file, expectReadonly] of [
  ["ibis-forensics.agent.md", true],
  ["ibis-curator.agent.md", false],
]) {
  let body = "";
  try {
    body = readFileSync(join(pluginDir, "com.github.copilot", "agents", file), "utf8");
  } catch {
    /* below */
  }
  const fmName = /^name:\s*(\S+)\s*$/m.exec(body || "");
  const tools = (body || "").split(/^tools:\s*$/m)[1] || "";
  const mutating = /\b(memory_add|memory_update|memory_delete|ingest_code|settings_apply)\b/.test(tools);
  check(
    `agent ${file}: frontmatter name + tools ${expectReadonly ? "all read-only" : "(curator may mutate)"}`,
    Boolean(body) && fmName && tools.trim().length > 0 && (expectReadonly ? !mutating : true),
    fmName ? `name=${fmName[1]}` : "missing",
  );
}

// ---------- automations ----------
for (const automation of ["nightly-ingest", "weekly-consolidate"]) {
  let body = "";
  try {
    body = readFileSync(join(pluginDir, "automations", `${automation}.automation.md`), "utf8");
  } catch {
    /* below */
  }
  const ok = /^id:\s*nightly-ingest|^id:\s*weekly-consolidate/m.test(body || "") &&
    /kind:\s*cron/.test(body || "") && /expression:\s*"/.test(body || "");
  check(`automation ${automation}: id + cron schedule frontmatter`, Boolean(body) && ok);
}

// ---------- VSIX contributions ----------
const vsixPkg = parseJson(join(root, "vscode-extension", "package.json"));
check(
  "vscode-extension: engines ^1.144.0 + vsce 4.x + id fatterdev.ibis-assistant",
  !vsixPkg.__error && vsixPkg.engines && vsixPkg.engines.vscode === "^1.144.0" &&
    /^4\./.test(String(vsixPkg.devDependencies["@vscode/vsce"])) &&
    `${vsixPkg.publisher}.${vsixPkg.name}` === "fatterdev.ibis-assistant",
  vsixPkg.__error || "ok",
);

const commandIds = (vsixPkg.contributes && vsixPkg.contributes.commands || []).map((c) => c.command);
const menuCommands = Object.values(vsixPkg.contributes && vsixPkg.contributes.menus || {})
  .flat()
  .map((m) => m.command);
check(
  "vscode-extension: every menu command is contributed",
  menuCommands.length > 0 && menuCommands.every((c) => commandIds.includes(c)),
  `${menuCommands.length} menu refs`,
);

const instructions = (vsixPkg.contributes && vsixPkg.contributes.chatInstructions) || [];
const instructionFile = instructions.length === 1
  ? join(root, "vscode-extension", instructions[0].path.replace("./", ""))
  : null;
let instructionBody = "";
try {
  instructionBody = instructionFile ? readFileSync(instructionFile, "utf8") : "";
} catch {
  /* below */
}
check(
  "vscode-extension: chatInstructions ibis-session-contract file exists with session contract",
  instructions.length === 1 && instructions[0].name === "ibis-session-contract" &&
    instructionBody.includes("load_session_context"),
  instructionBody ? `${instructionBody.length} chars` : "missing",
);

const walkthroughSteps = (((vsixPkg.contributes || {}).walkthroughs || [])[0] || {}).steps || [];
check(
  "vscode-extension: onboarding walkthrough has 4 steps",
  walkthroughSteps.length === 4,
  walkthroughSteps.map((s) => s.id).join(","),
);

// ---------- distribution manifests ----------
const serverJson = parseJson(join(root, "server.json"));
check(
  "server.json: io.github.fatterdev/ibis with streamable-http remote",
  !serverJson.__error && serverJson.name === "io.github.fatterdev/ibis" &&
    serverJson.remotes && serverJson.remotes[0] && serverJson.remotes[0].type === "streamable-http",
  serverJson.__error || `v${serverJson.version}`,
);

const marketplaceJson = parseJson(join(root, "marketplace", "marketplace.json"));
const ibisPluginEntry = (marketplaceJson.plugins || [])[0] || {};
check(
  "marketplace.json: ibis plugin listed (github source, path plugin/)",
  !marketplaceJson.__error && ibisPluginEntry.name === "ibis" &&
    ibisPluginEntry.source && ibisPluginEntry.source.repo === "fatterdev/ibis-assistant" &&
    ibisPluginEntry.source.path === "plugin",
  marketplaceJson.__error || "ok",
);

// ---------- badges / deep links ----------
const vsixReadme = readFileSync(join(root, "vscode-extension", "README.md"), "utf8");
check(
  "vsix README: vscode:mcp/install badge present",
  vsixReadme.includes("vscode:mcp/install?%7B%22name%22%3A%22ibis%22"),
);
const marketplaceReadme = readFileSync(join(root, "marketplace", "README.md"), "utf8");
check(
  "marketplace README: vscode://chat-plugin/install deep link present",
  marketplaceReadme.includes("vscode://chat-plugin/install?source=fatterdev/ibis-marketplace&plugin=ibis"),
);

// ---------- summary ----------
console.log("");
console.log(`R3/R4 Parte A smoke: ${pass} PASS, ${fail} FAIL`);
process.exit(fail === 0 ? 0 : 1);