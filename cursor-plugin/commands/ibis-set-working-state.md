---
description: Set the current Ibis working state for this project and branch
---

# Ibis: Set working state

Upsert what you are doing right now so the next session can resume cleanly.

1. Ask the user for a short description of the current task (open files, next steps, blockers).
2. Call MCP `set_working_state` on the `ibis-assistant` server with:
   - `content` = that description (required)
   - `branch` = current git branch when known
   - `project_name` = workspace/repo name when known
3. Confirm the returned memory `id` and that working state was set.
4. Do **not** call `load_session_context` unless the user also asked to review prior context.
