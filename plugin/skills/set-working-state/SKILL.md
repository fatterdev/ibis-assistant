---
name: set-working-state
description: Record or update the current working state (what is in progress, blockers, next step) via set_working_state. Use when the user switches tasks, ends a work block, or asks Threski to keep track of the current job.
---

# Threski set working state

Call `set_working_state` with:

- `project_name`: the workspace folder name (when not already bound).
- `content`: 1–3 short lines — current task, blockers (if any), next step.

Guidelines:

- Working state is **ephemeral session context**, not durable memory: never put team
  decisions here — those go to `memory_add` (see `remember`).
- Overwrite the whole state each time; do not append history.
- Keep it under ~50 words; the next session loads this via `load_session_context`.

Example content:

```text
Task: migrate ticketing client to azure_devops provider.
Blocker: waiting on PAT scope confirmation.
Next: implement ticket_transition mapping.
```