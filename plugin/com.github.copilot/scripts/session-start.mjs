import { readFileSync } from "node:fs";

const CONTRACT = [
  "Ibis session contract:",
  "1. Call load_session_context ONCE for this project now, if you have not already.",
  "2. Do not call memory_* tools every turn; they are for durable facts only.",
  "3. Prefer the status skill / Ibis Assistant: Status command over settings_get or ticket_get_capabilities unless diagnosing.",
  "4. Prefer why_changed / analyze_blast_radius / ask_project before guessing about the codebase.",
].join("\n");

// Drain the event payload from stdin (required before writing output).
try {
  readFileSync(0, "utf8");
} catch {
  /* empty payload */
}

process.stdout.write(JSON.stringify({ additionalContext: CONTRACT }));