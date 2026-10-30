---
description: Remember a durable decision, fix, or rule in Ibis project memory
---

# Ibis: Remember

Capture a durable project memory the team (or future-you) should reuse.

1. Ask the user which kind to store: `decision`, `fix`, or `rule` (default `decision` if they do not choose).
2. Ask for a short title and the full content (what was decided, what fixed the failure, or the rule to follow).
3. Call MCP `memory_add` on the `ibis` server with:
   - `kind` = the chosen kind
   - `title` = short label
   - `content` = full text
   - `visibility` = `team` unless the user explicitly asks for personal
4. Confirm the returned memory `id` to the user.
5. Do **not** call `load_session_context` or other memory tools unless the user also asked for prior context.
