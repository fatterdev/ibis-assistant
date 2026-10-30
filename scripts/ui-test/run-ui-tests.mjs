#!/usr/bin/env node
/**
 * R3/R4 Parte B runner — drives UI validation of the Ibis Assistant VSIX and
 * agent plugin with a computer-use agent (UI-TARS Desktop or any GUI agent).
 *
 * Workflow:
 *   1. node scripts/ui-test/run-ui-tests.mjs prep --install-vsix --launch
 *   2. node scripts/ui-test/run-ui-tests.mjs new
 *   3. node scripts/ui-test/run-ui-tests.mjs prompt R3.2        # paste into UI-TARS
 *   4. (agent performs the task; screenshots land in the run folder)
 *   5. node scripts/ui-test/run-ui-tests.mjs mark R3.2 pass --note "..."
 *   6. node scripts/ui-test/run-ui-tests.mjs collect && ... report
 *
 * Zero dependencies (Node >= 18).
 */

import { spawn, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(join(here, "..", ".."));
const manifestPath = join(here, "tasks.json");
const evidenceRoot = join(root, "artifacts", "ui-tests");
const vsixPath = join(root, "vscode-extension", "ibis-assistant-1.0.0.vsix");
const uiTarsExe = process.env.UI_TARS_EXE || join(process.env.LOCALAPPDATA || "", "UiTars", "UI-TARS.exe");
const vscodeExe = process.env.VSCODE_EXE || "code";

const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const taskById = new Map(manifest.tasks.map((t) => [t.id, t]));

const [, , command = "help", ...rest] = process.argv;
const flag = (name) => rest.includes(name);
const optValue = (name) => {
  const i = rest.indexOf(name);
  return i >= 0 && rest[i + 1] ? rest[i + 1] : undefined;
};

function latestRun() {
  if (!existsSync(evidenceRoot)) return undefined;
  const runs = readdirSync(evidenceRoot).filter((d) => /^\d{8}-\d{4}/.test(d)).sort();
  return runs.at(-1);
}

function runDir() {
  const explicit = optValue("--run");
  if (explicit) {
    const dir = resolve(explicit);
    if (!existsSync(dir)) {
      console.error(`run folder not found: ${dir}`);
      process.exit(1);
    }
    return dir;
  }
  const latest = latestRun();
  if (!latest) {
    console.error(`no run folder under ${evidenceRoot} — create one with: new`);
    process.exit(1);
  }
  return join(evidenceRoot, latest);
}

function runResultsFile(dir) {
  return join(dir, "results.json");
}

function loadResults(dir) {
  try {
    return JSON.parse(readFileSync(runResultsFile(dir), "utf8"));
  } catch {
    return {};
  }
}

function saveResults(dir, results) {
  writeFileSync(runResultsFile(dir), `${JSON.stringify(results, null, 2)}\n`);
}

function resultsMarkdown(dir, results) {
  const runName = dir.split(/[\\/]/).filter(Boolean).pop();
  const lines = [
    `# UI validation evidence — ${runName}`,
    "",
    `Source: [tasks.json](scripts/ui-test/tasks.json) · checklist: [review-r3r4-checklist.md](docs/planning/review-r3r4-checklist.md)`,
    "",
    "| Task | Status | Evidence | Note |",
    "|---|---|---|---|",
  ];
  for (const task of manifest.tasks) {
    const entry = results[task.id] || {};
    const status = entry.status || (entry.evidence?.length ? "evidence-only" : "pending");
    const evidence = (entry.evidence || []).map((e) => `\`${e}\``).join(" ") || "—";
    lines.push(`| **${task.id}** ${task.title} | ${status} | ${evidence} | ${entry.note || ""} |`);
  }
  lines.push("", "Statuses: pass / fail / evidence-only (files present, not yet judged) / pending.");
  return lines.join("\n");
}

function refreshArtifacts(dir) {
  const files = readdirSync(dir).filter((f) => /\.(png|jpg|jpeg|gif|mp4)$/i.test(f));
  const results = loadResults(dir);
  for (const task of manifest.tasks) {
    const norm = task.id.replace(/\./g, "");
    const evidence = files.filter((f) => {
      const stem = f.replace(/[-_\s].*$/, "");
      return stem.replace(/\./g, "") === norm || f.toLowerCase().startsWith(`${task.id.toLowerCase()}`);
    });
    results[task.id] = { ...(results[task.id] || {}), evidence: evidence.sort() };
  }
  saveResults(dir, results);
  writeFileSync(join(dir, "results.md"), `${resultsMarkdown(dir, results)}\n`);
  return results;
}

function printPrompt(id) {
  const task = taskById.get(id);
  if (!task) {
    console.error(`unknown task ${id}; available: ${manifest.tasks.map((t) => t.id).join(", ")}`);
    process.exit(1);
  }
  const run = latestRun();
  const evidenceList = task.evidence
    .split(",")
    .map((e) => e.trim())
    .filter(Boolean);
  console.log(
    [
      `=== ${task.id} — ${task.title} ===`,
      "Paste this into UI-TARS Desktop (Local Operator):",
      "",
      `TASK: ${task.instruction}`,
      "",
      "WHEN DONE, VERIFY THESE EXPECTATIONS:",
      ...task.expected.map((e, i) => `${i + 1}. ${e}`),
      "",
      `EVIDENCE: save screenshots as ${evidenceList.join(", ")}`,
      ...(run
        ? [`Run folder: ${join(evidenceRoot, run)} (UI-TARS screenshot folder can be set to it)`]
        : ["Create a run folder first: node scripts/ui-test/run-ui-tests.mjs new"]),
      "",
      "Then record the outcome:",
      `node scripts/ui-test/run-ui-tests.mjs mark ${id} pass|fail --note "one line"`,
    ].join("\n"),
  );
}

function cmdPrep() {
  console.log("[prep] VSIX:", existsSync(vsixPath) ? `found ${vsixPath}` : "MISSING — build with: npm run package (vscode-extension/)");
  console.log("[prep] UI-TARS Desktop:", existsSync(uiTarsExe) ? uiTarsExe : "not found (set UI_TARS_EXE)");
  if (!existsSync(uiTarsExe)) {
    console.log("       install: winget install --id ByteDance.UI-TARS -e");
  }

  if (flag("--install-vsix")) {
    if (!existsSync(vsixPath)) {
      console.error("[prep] cannot install: VSIX missing");
      process.exit(1);
    }
    const res = spawnSync(vscodeExe, ["--install-extension", vsixPath, "--force"], {
      stdio: "inherit",
      shell: process.platform === "win32",
    });
    console.log(`[prep] code --install-extension exited with ${res.status}`);
  }

  if (flag("--launch")) {
    if (existsSync(uiTarsExe)) {
      spawn(uiTarsExe, { detached: true, stdio: "ignore" }).unref();
      console.log("[prep] UI-TARS Desktop launched");
    }
    spawn(vscodeExe, [root], { detached: true, stdio: "ignore", shell: process.platform === "win32" }).unref();
    console.log("[prep] VS Code launched on the workspace");
  }

  console.log(
    [
      "",
      "[prep] reminders for the UI-TARS app settings:",
      "- configure a VLM provider (UI-TARS / VolcEngine Doubao / any OpenAI-compatible VLM endpoint)",
      "- single-monitor setup only; grant screen-capture permissions if asked",
      "- the plugin under test must be installed from source (see checklist P4 pre-flight)",
    ].join("\n"),
  );
}

function cmdNew() {
  const stamp = new Date().toISOString().slice(0, 16).replace(/[-:T]/g, "").slice(0, 12);
  const name = optValue("--name") || "r3r4";
  const dir = join(evidenceRoot, `${stamp}-${name}`);
  mkdirSync(dir, { recursive: true });
  console.log(`run folder: ${dir}`);
  console.log(`set the UI-TARS screenshot folder to it, then run: node scripts/ui-test/run-ui-tests.mjs prompt R3.1`);
}

function cmdMark() {
  const id = rest[0];
  const status = rest[1];
  const task = taskById.get(id);
  if (!task) {
    console.error(`unknown task ${id}`);
    process.exit(1);
  }
  if (!["pass", "fail"].includes(status)) {
    console.error("usage: mark <taskId> <pass|fail> [--note \"...\"] [--run <dir>]");
    process.exit(1);
  }
  const dir = runDir();
  const results = refreshArtifacts(dir);
  const noteIdx = rest.indexOf("--note");
  results[id] = {
    ...(results[id] || {}),
    status,
    note: noteIdx >= 0 && rest[noteIdx + 1] ? rest[noteIdx + 1] : (results[id]?.note || ""),
  };
  saveResults(dir, results);
  writeFileSync(join(dir, "results.md"), `${resultsMarkdown(dir, results)}\n`);
  console.log(`[mark] ${id} -> ${status} (${dir})`);
}

function cmdCollect() {
  const dir = runDir();
  const results = refreshArtifacts(dir);
  const withEvidence = manifest.tasks.filter((t) => results[t.id]?.evidence?.length).length;
  console.log(`[collect] ${withEvidence}/${manifest.tasks.length} tasks have evidence files in ${dir}`);
  for (const task of manifest.tasks) {
    const e = results[task.id]?.evidence || [];
    if (e.length) console.log(`  ${task.id}: ${e.join(", ")}`);
  }
}

function cmdReport() {
  const dir = runDir();
  const results = refreshArtifacts(dir);
  console.log(resultsMarkdown(dir, results));
  const counts = { pass: 0, fail: 0, "evidence-only": 0, pending: 0 };
  for (const task of manifest.tasks) {
    const status = results[task.id]?.status || (results[task.id]?.evidence?.length ? "evidence-only" : "pending");
    counts[status] += 1;
  }
  console.log(`\nSummary: ${counts.pass} pass, ${counts.fail} fail, ${counts["evidence-only"]} evidence-only, ${counts.pending} pending`);
}

function cmdList() {
  const dir = existsSync(evidenceRoot) ? latestRun() : undefined;
  const results = dir ? loadResults(join(evidenceRoot, dir)) : {};
  for (const task of manifest.tasks) {
    const status = results[task.id]?.status || (results[task.id]?.evidence?.length ? "evidence-only" : "pending");
    console.log(`${task.id.padEnd(5)} ${status.padEnd(13)} ${task.title}`);
  }
  console.log(`\nrun folder: ${dir ? join(evidenceRoot, dir) : "none yet (new)"}`);
}

function cmdHelp() {
  console.log(
    [
      "Usage: node scripts/ui-test/run-ui-tests.mjs <command> [options]",
      "",
      "Commands:",
      "  list                 show tasks and their current status",
      "  prompt <taskId>      print the paste-ready instruction for UI-TARS Desktop",
      "  new [--name <slug>]  create an evidence run folder under artifacts/ui-tests/",
      "  prep [--install-vsix] [--launch]",
      "                       verify/build environment; optionally install the VSIX",
      "                       and launch VS Code + UI-TARS Desktop",
      "  mark <taskId> <pass|fail> [--note \"...\"] [--run <dir>]",
      "                       record the judgement for a task (refreshes results.md)",
      "  collect [--run <dir>]  map evidence files to tasks and refresh results.md",
      "  report [--run <dir>]   print the full results table",
      "",
      `Tasks source: ${manifestPath}`,
      `Checklist:    docs/planning/review-r3r4-checklist.md`,
    ].join("\n"),
  );
}

const commands = { list: cmdList, prompt: () => printPrompt(rest[0]), prep: cmdPrep, new: cmdNew, mark: cmdMark, collect: cmdCollect, report: cmdReport, help: cmdHelp };
(commands[command] || cmdHelp)();