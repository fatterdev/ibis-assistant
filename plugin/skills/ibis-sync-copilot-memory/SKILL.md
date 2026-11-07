---
name: ibis-sync-copilot-memory
description: Experimental. Port useful entries from Copilot Memory (when the operator has access to them) into Ibis typed memory via memory_add - choose kind and visibility, write an honest title and actionable content, note the provenance, skip session-scoped chatter, and never delete Copilot-side data. Use when the user asks to migrate or sync Copilot Memory entries into Ibis.
---

# Ibis sync — Copilot Memory (experimental)

One-directional port: Copilot Memory → Ibis. The Copilot side stays read-only.

## When to run

Only when the operator explicitly asks and they have access to the Copilot
Memory entries (e.g. repository facts surfaced in the Copilot UI). Do not run
speculatively.

## How to port an entry

1. Select only durable project knowledge: decisions, fixes, constraints,
   conventions. Skip session-scoped chatter, task bookkeeping, and anything
   the code already contradicts or that looks stale.
2. Deduplicate first: `memory_search` for the same fact. If Ibis already holds
   it, update or supersede that row (`memory_update`) instead of adding a
   near-duplicate.
3. For each entry worth keeping, call `memory_add` with:
   - `kind`: the narrowest fit — `decision`, `fix`, `rule`, `constraint`,
     `gotcha`, `design`, `fact`.
   - `visibility`: `team` by default (shared knowledge); `personal` only for
     owner-specific notes.
   - `title` + `content`: honest and self-contained — rephrase from the entry
     so a future agent can act on it without the original. No secrets, no
     verbatim prompt text.
   - Provenance: append a line like `Synced from Copilot Memory (<date>)` to
     the content so the source stays traceable.
4. Report what was ported and what was skipped (with the reason), then stop.

## Hard rules

- Never delete, edit, or vote on the Copilot-side entries: this skill is
  read-only toward Copilot Memory.
- Never store secrets or credentials found in the entries.
- Do not re-run blindly: the same entry ported twice becomes a duplicate in
  Ibis. Review each run's report before another pass.
