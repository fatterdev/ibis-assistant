---
name: status
description: Report the Threski runtime status (backend, AI reasoning provider, ticketing, workspace binding) by calling settings_get once. Use when the user asks for status or diagnostics about Threski itself.
---

# Threski status

Call `settings_get` once and summarize. Do not call other Threski tools for a plain
status check; only if diagnosing a failure add a single `ticket_get_capabilities` call.

Report, in this order:

1. Backend: spawned child (`threski -mode stdio -runtime-mode plugin`) or attached
   personal runtime (URL).
2. AI reasoning: `ai.reasoning.provider` value and whether embeddings are on Ollama.
3. Ticketing: configured providers or "off".
4. Workspace: bound project name.
5. Anything the user explicitly asked about.

Keep it short. End with one actionable line if something is unhealthy; otherwise say
the runtime is healthy and stop.