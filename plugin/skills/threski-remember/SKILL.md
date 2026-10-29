---
name: threski-remember
description: Capture a durable project memory with the correct type using memory_add. Use when the user says "remember this", when a decision is made, when a fix resolves a failure, or when a constraint or gotcha must be kept for the team.
---

# Threski remember

Store one durable fact with `memory_add`. Ask which type only if genuinely ambiguous;
otherwise pick the narrowest fit and say so.

## Kind guide

| Kind | Use when |
|---|---|
| `decision` | A choice the team should keep (approach, trade-off, direction) |
| `fix` | What actually resolved a failure (root cause + remedy) |
| `rule` | A durable convention or convention-check the project follows |
| `constraint` | A hard boundary the project must respect (versions, licenses, SLAs) |
| `gotcha` | A trap future agents should avoid (non-obvious failure mode) |
| `design` | Design rationale worth keeping (why the architecture is so) |
| `fact` | A verifiable statement about the project (paths, commands, owners) |

## Content rules

- Title: short, imperative or nominal ("Use Streamable HTTP at /mcp").
- Content: one fact, actionable by a future agent; include file paths or commands
  when they make the memory verifiable.
- `visibility`: `team` by default; `personal` only if the user asks for a private note.
- If a similar memory already exists (`memory_search` first when unsure), supersede it
  with `memory_update` instead of adding a duplicate.
- Never store secrets, tokens, or temporary task state.

After writing, confirm in one line: kind, title, visibility.