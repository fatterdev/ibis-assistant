---
name: threski-readiness
description: First-use and empty-window readiness checklist for Threski. Use when session context is empty, tools fail unexpectedly, or the user asks whether Threski is set up. Produces a structured readiness table and the next concrete step.
---

# Threski readiness

Run this checklist only when the session context is empty or something fails.
In a healthy session you should already have called `load_session_context` once.

## Checklist

1. Probe the MCP surface: is the `threski` MCP server reachable? If spawned, is the
   `threski` binary on PATH? If attached, is `THRESKI_MCP_URL` reachable?
2. Call `load_session_context` once for the workspace project.
3. If AI answers look degraded, check `settings_get` → `ai.reasoning.provider`
   (sampling / lm_bridge / ollama) and report, do not fix silently.
4. If tickets fail, call `ticket_get_capabilities` once — only now, only for diagnosis.

## Report format

End with exactly this table and a single next step:

| Check | Status | Detail |
|---|---|---|
| MCP live | ok / fail | spawn cmdline or attach URL |
| Session context | loaded / empty | project name |
| AI reasoning | provider | sampling / lm_bridge / ollama |
| Ticketing | configured / off | capabilities summary |
| Workspace | bound / missing | folder path |

Next step: one concrete action (e.g. "run `threski run`", "ingest code with
`threski ingest code --path <folder>`", "ask the user for THRESKI_MCP_URL").