// Scope run (D2) - submits the worker x profile x task matrix and waits for completion.
// Task prompts are parsed from task-prompts/fixture-tasks.md: each "## Tn — Title"
// section contributes its first ```text block, keyed by its Tn id.
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(fileURLToPath(import.meta.url));
const env = Object.fromEntries(
  readFileSync(join(root, ".env"), "utf8").split(/\r?\n/).filter((l) => l.includes("=")).map((l) => [l.slice(0, l.indexOf("=")).trim(), l.slice(l.indexOf("=") + 1).trim()])
);
const SCOPE_URL = process.env.SCOPE_URL ?? env.SCOPE_URL;
const SCOPE_API_KEY = process.env.SCOPE_API_KEY ?? env.SCOPE_API_KEY;
const seedPath = join(root, ".state", "seed-result.json");
if (!existsSync(seedPath)) {
  console.error("Run seed.mjs first");
  process.exit(1);
}

const api = async (path, method, body) => {
  const res = await fetch(`${SCOPE_URL}/api/v1${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${SCOPE_API_KEY}` },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(`${method} ${path} -> ${res.status}: ${await res.text()}`);
  return res.json();
};

function loadTasks() {
  const md = readFileSync(join(root, "task-prompts", "fixture-tasks.md"), "utf8");
  const tasks = {};
  for (const section of md.split(/^## /m).slice(1)) {
    const title = section.split("\n")[0].trim();
    const idMatch = title.match(/^(T\d+)/);
    const blockMatch = section.match(/```text\r?\n([\s\S]*?)```/);
    if (idMatch && blockMatch) tasks[idMatch[1]] = blockMatch[1].trim();
  }
  return tasks;
}

const seed = JSON.parse(readFileSync(seedPath, "utf8"));
const profilesDef = JSON.parse(readFileSync(join(root, "profiles.json"), "utf8"));
const criteriaDef = JSON.parse(readFileSync(join(root, "criteria.json"), "utf8"));
const scopeCriterionId = (localId) => {
  const row = seed.criteria.find((r) => r.localId === localId);
  if (!row || row.scopeId === "?") throw new Error(`No Scope id for criterion ${localId} (check seed output)`);
  return row.scopeId;
};
const criteriaIds = criteriaDef.criteria.map((c) => scopeCriterionId(c.id));
const tasks = loadTasks();
if (Object.keys(tasks).length === 0) throw new Error("No task prompts parsed from fixture-tasks.md");

const REPEATS = Number(process.env.REPEATS ?? 3);
const submitted = [];
for (const worker of profilesDef.workers) {
  for (const p of profilesDef.profiles) {
    const profile = seed.profiles.find((x) => x.name === p.name);
    const versionId = profile?.created?.latestVersionId ?? profile?.created?.id;
    if (!versionId) throw new Error(`No profile version id for ${p.name}`);
    for (const [taskId, prompt] of Object.entries(tasks)) {
      for (let i = 0; i < REPEATS; i++) {
        submitted.push({
          worker,
          profile: p.name,
          task: taskId,
          repeat: i + 1,
          request: await api("/requests", "POST", {
            scenario: { task: prompt, criteria: criteriaIds },
            profileVersionId: versionId,
          }),
        });
        console.log(`submitted ${worker}/${p.name}/${taskId} #${i + 1}`);
      }
    }
  }
}

mkdirSync(join(root, ".state"), { recursive: true });
writeFileSync(join(root, ".state", "runs-submitted.json"), JSON.stringify(submitted, null, 2));
console.log("Matrix submitted. Collect results with analysis.mjs once runs complete.");