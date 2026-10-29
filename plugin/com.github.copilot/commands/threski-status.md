Read STATUS-style readiness for Threski in this workspace, then report:

1. Run `load_session_context` once for the project (skip if already loaded this session).
2. Backend: spawned `threski` child or attached runtime URL.
3. AI reasoning provider (`settings_get` only if the answer is not already known).
4. Ticketing: configured or off.
5. Workspace: bound project name.

End with the readiness table (MCP live | AI | Ticketing | Workspace | Next step)
and exactly one next step if anything is unhealthy. Do not call Threski tools again
after the table.