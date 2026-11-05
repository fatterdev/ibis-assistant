The activity-bar **Ibis Assistant** view lists memory kinds (decision, fix, rule, …) with entries from MCP `memory_list`; click an entry for the full detail. Kind browse and search show only still-valid memories — the title-bar filter button (or **Ibis Assistant: Toggle Memory Status Filter**) widens the browse to every status for curation. The **Workstreams** view below it browses the same memories grouped by workstream (`memory_workstreams` roots, current-member counts on the roots).

Right-click an entry to curate it inline: **Share** to team, **Archive**, **Supersede…** (replaces it with a new memory), or **Update Content…** on rows you own. Both trees refresh when they gain focus; search results stay put until you run the search again.

Open the view, then attach the **ibis-session-contract** chat instruction to agent sessions so they call `load_session_context` once and keep memory hygiene.

[Open the memory view](command:workbench.view.extension.ibis)