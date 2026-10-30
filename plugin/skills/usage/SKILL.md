---
name: threski-usage
description: How to use Threski correctly in a session: the session contract (load session context once), the tool selection map (why_changed vs ask_project vs memory_search), and memory hygiene rules (kinds, capture_observation, visibility). Use at session start and whenever unsure which Threski tool fits the task.
---

# Threski usage — session contract

Threski is the project memory and knowledge graph. Follow this contract; it keeps
contexts small and memory trustworthy.

## Session contract

1. At session start, call `load_session_context` **exactly once** for the project.
   It returns working state, recent decisions and readiness hints.
2. Do **not** call `memory_*` tools every turn. Memory tools are for durable facts,
   not for chat transcripts or task bookkeeping.
3. Do **not** call `settings_get` or `ticket_get_capabilities` unless diagnosing a
   problem or the user asks about configuration. Prefer the readiness summary from
   `load_session_context` or the `threski-readiness` skill when the window is empty.
4. When a genuinely new, durable lesson emerges at the end of real work, use
   `capture_observation` (or `memory_add` with an explicit kind). If the lesson is
   not durable, say nothing and move on.

## Tool selection map

| Need | Tool |
|---|---|
| Why does this code exist / what changed it | `why_changed` |
| What breaks if I change X | `analyze_blast_radius` |
| Where is dead or unused code | `find_dead_code` |
| Open question about the project | `ask_project` (hybrid retrieval over the graph) |
| Recall a decision, gotcha, constraint | `memory_search`, then `memory_expand` |
| Store a durable lesson the team must keep | `memory_add` (typed, see hygiene) |
| Current task / plan for this session | `get_working_state`, `set_working_state` |
| Ticket facts | `ticket_get`, `ticket_search` (only if ticketing is configured) |
| Diagnose a production error | `analyze_logs` |

## Memory hygiene

- One fact per memory; a clear imperative title; content a future agent can act on.
- Prefer the narrowest kind that fits: `decision`, `fix`, `rule`, `constraint`,
  `gotcha`, `design`, `fact`.
- Team first: default `visibility: team` so the whole team benefits; use
  `visibility: personal` only for owner-specific notes.
- Supersede instead of duplicating: if an existing memory is outdated, update or
  supersede it (`memory_update`), never add a near-duplicate.
- Never store secrets, credentials, or ephemeral task state.