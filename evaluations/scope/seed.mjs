// Scope seed (D1) - creates MCP server, imports skills, seeds criteria and profiles.
// Field names must be verified against the live Scope REST API (see README "Da validare a D1").
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(fileURLToPath(import.meta.url));
const env = Object.fromEntries(
  readFileSync(join(root, ".env"), "utf8").split(/\r?\n/).filter((l) => l.includes("=")).map((l) => [l.slice(0, l.indexOf("=")).trim(), l.slice(l.indexOf("=") + 1).trim()])
);
const SCOPE_URL = process.env.SCOPE_URL ?? env.SCOPE_URL;
const SCOPE_API_KEY = process.env.SCOPE_API_KEY ?? env.SCOPE_API_KEY;
if (!SCOPE_URL || !SCOPE_API_KEY) {
  console.error("Missing SCOPE_URL / SCOPE_API_KEY (set .env or environment)");
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

const criteriaDef = JSON.parse(readFileSync(join(root, "criteria.json"), "utf8"));
const profilesDef = JSON.parse(readFileSync(join(root, "profiles.json"), "utf8"));
const result = { criteria: [], mcpServer: null, skills: [], profiles: [] };

for (const c of criteriaDef.criteria) {
  const created = await api("/criteria", "POST", { name: c.name, prompt: c.prompt });
  result.criteria.push({ localId: c.id, scopeId: created.id ?? created.criteriaId ?? "?", gate: c.gate });
}

result.mcpServer = await api("/mcp-servers", "POST", {
  name: profilesDef.mcpServers[0].label,
  transport: profilesDef.mcpServers[0].transport,
  command: profilesDef.mcpServers[0].command,
  args: profilesDef.mcpServers[0].args,
  env: profilesDef.mcpServers[0].env,
});

for (const s of profilesDef.skills) {
  result.skills.push({ slug: s.slug, created: await api("/skills", "POST", { source: s.source }) });
}

for (const p of profilesDef.profiles) {
  result.profiles.push({
    name: p.name,
    created: await api("/profiles", "POST", {
      name: p.name,
      workerType: p.workerType,
      model: p.model,
      mcpServers: p.mcpServers,
      skillRevisions: p.skillRevisions,
      extensions: p.extensions,
    }),
  });
}

mkdirSync(join(root, ".state"), { recursive: true });
writeFileSync(join(root, ".state", "seed-result.json"), JSON.stringify(result, null, 2));
console.log(`Seeded: ${result.criteria.length} criteria, 1 mcp server, ${result.skills.length} skills, ${result.profiles.length} profiles.`);
console.log("Ids written to .state/seed-result.json");