import { readFileSync } from "node:fs";

const REMINDER = [
  "Ibis end-of-turn check:",
  "If this turn produced a genuinely new durable lesson (a decision, a fix, a constraint, a gotcha),",
  "capture it ONCE with capture_observation or memory_add using the narrowest kind.",
  "If nothing durable emerged, capture nothing. Do not restate existing memories.",
].join("\n");

let event = {};
try {
  event = JSON.parse(readFileSync(0, "utf8"));
} catch {
  event = {};
}

process.stdout.write(JSON.stringify({ additionalContext: REMINDER, source: "ibis", event: event.event ?? "agentStop" }));