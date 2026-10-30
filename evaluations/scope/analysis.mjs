// Scope analysis (D2/D3) - pass rate per profile, deltas vs base, token/duration averages.
// Optional per-run tool hygiene stats are read from downloaded tool-calls JSONL if present.
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(fileURLToPath(import.meta.url));
const env = Object.fromEntries(
  readFileSync(join(root, ".env"), "utf8").filter((l) => l.includes("=")).map((l) => [l.slice(0, l.indexOf("=")).trim(), l.slice(l.indexOf("=") + 1).trim()])
);
const SCOPE_URL = process.env.SCOPE_URL ?? env.SCOPE_URL;
const SCOPE_API_KEY = process.env.SCOPE_API_KEY ?? env.SCOPE_API_KEY;
const submittedPath = join(root, ".state", "runs-submitted.json");
if (!existsSync(submittedPath)) {
  console.error("No submitted runs found (run run.mjs first)");
  process.exit(1);
}

const api = async (path) => {
  const res = await fetch(`${SCOPE_URL}/api/v1${path}`, { headers: { Authorization: `Bearer ${SCOPE_API_KEY}` } });
  if (!res.ok) throw new Error(`GET ${path} -> ${res.status}: ${await res.text()}`);
  return res.json();
};

const submitted = JSON.parse(readFileSync(submittedPath, "utf8"));
const summary = {};
for (const row of submitted) {
  const key = `${row.worker}/${row.profile}`;
  summary[key] ??= { total: 0, passed: 0, tokens: 0, durationMs: 0 };
  const req = await api(`/requests/${row.request.id ?? row.request.requestId}`);
  const runs = req.runs ?? [];
  summary[key].total += runs.length;
  for (const run of runs) {
    if (run.passed) summary[key].passed += 1;
    summary[key].tokens += run.tokens ?? run.usage?.totalTokens ?? 0;
    summary[key].durationMs += run.durationMs ?? 0;
  }
}

const baseKey = Object.keys(summary).find((k) => k.endsWith("/base-no-ibis"));
const baseRate = baseKey && summary[baseKey].total ? summary[baseKey].passed / summary[baseKey].total : null;
const rows = Object.entries(summary).map(([key, s]) => {
  const passRate = s.total ? s.passed / s.total : 0;
  return {
    key,
    passRate: (passRate * 100).toFixed(1) + "%",
    deltaVsBase: baseRate == null ? "-" : ((passRate - baseRate) * 100).toFixed(1) + "pp",
    avgTokens: s.total ? Math.round(s.tokens / s.total) : 0,
    avgDurationMs: s.total ? Math.round(s.durationMs / s.total) : 0,
  };
});

console.table(rows);
writeFileSync(join(root, ".state", "analysis.json"), JSON.stringify({ generatedAt: new Date().toISOString(), rows }, null, 2));
console.log("Written .state/analysis.json");