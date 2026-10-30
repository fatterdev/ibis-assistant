---
version: 1
id: weekly-consolidate
name: Weekly Ibis memory consolidation
description: Weekly pass over project memory - deduplicate, supersede stale entries and propose missing high-impact memories.
schedule:
  kind: cron
  expression: "30 10 * * 5"
  timeZone: local
---
Weekly memory curation for this workspace:

1. Search memory per kind (decision, fix, rule, constraint, gotcha, design, fact).
2. List duplicates and near-duplicates with proposed supersession or deletion -
   present the table and wait for confirmation before applying changes.
3. Run the Ibis MCP tool `memory_consolidate` for the project.
4. Run `optimize_knowledge` once.
5. Report: kept/updated/deleted counts per kind and the single highest-impact
   memory the team is still missing.
6. Do not create new memories automatically.