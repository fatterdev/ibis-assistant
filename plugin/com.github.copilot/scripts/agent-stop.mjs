import { readFileSync } from "node:fs";

/**
 * agentStop hook (Copilot hooks reference): the only consumed outputs are the
 * stop decision fields (`decision: "block"` with `reason`). `additionalContext`
 * is NOT an agentStop output. Conditional capture reminder: force exactly ONE
 * continuation turn when the turn did real editing work and captured no memory;
 * `stop_hook_active` guards the runaway cap (8 blocks) by self-limiting.
 */

const REMINDER = [
  "Before ending this turn, check the Ibis end-of-turn rule:",
  "If this turn produced a genuinely new durable lesson (a decision, a fix, a constraint, a gotcha),",
  "capture it ONCE with capture_observation or memory_add using the narrowest kind and a clear imperative title.",
  "If nothing durable emerged, end the turn without capturing anything. Do not restate existing memories.",
].join("\n");

const TRANSCRIPT_TAIL_CHARS = 200_000;
const DURABLE_WORK_SIGNAL =
  /"(?:tool_name|toolName)"\s*:\s*"(?:Write|Edit|Create|MultiEdit|write|edit|create|multi_edit|apply_patch|str_replace_editor)"/;

function readStdinJson() {
  try {
    return JSON.parse(readFileSync(0, "utf8"));
  } catch {
    return {};
  }
}

function readTranscriptTail(event) {
  const path = event.transcriptPath || event.transcript_path;
  if (!path) {
    return "";
  }
  const raw = readFileSync(path, "utf8");
  return raw.length > TRANSCRIPT_TAIL_CHARS ? raw.slice(-TRANSCRIPT_TAIL_CHARS) : raw;
}

const event = readStdinJson();

// No-op when output is not actionable: print {} so the hook ends cleanly.
function noop() {
  process.stdout.write("{}");
}

// One forced continuation per turn maximum; the reminder already fired.
if (event.stop_hook_active === true) {
  noop();
} else {
  let transcript = "";
  try {
    transcript = readTranscriptTail(event);
  } catch {
    transcript = "";
  }

  if (!transcript) {
    // Transcript unavailable (surface-dependent): fail-open, never block blindly.
    noop();
  } else if (/capture_observation|memory_add/.test(transcript)) {
    // A capture already happened this turn: nothing to remind.
    noop();
  } else if (DURABLE_WORK_SIGNAL.test(transcript)) {
    // Real editing work without any capture: block once with the reminder.
    process.stdout.write(JSON.stringify({ decision: "block", reason: REMINDER }));
  } else {
    // No durable-work signal: end silently.
    noop();
  }
}