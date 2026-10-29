Capture a durable project memory from this conversation with `memory_add`.

1. Identify the single most durable fact the team should keep from the current
   context. If nothing durable emerged, say so and stop.
2. Pick the narrowest kind: decision | fix | rule | constraint | gotcha | design | fact.
3. Title: short, nominal. Content: one actionable fact, with file paths or commands
   when they make it verifiable.
4. Visibility: `team` by default; `personal` only if the user asked for a private note.
5. If a similar memory may already exist, `memory_search` first and supersede it
   (`memory_update`) instead of duplicating.

Confirm in one line: kind, title, visibility. Never store secrets or task state.