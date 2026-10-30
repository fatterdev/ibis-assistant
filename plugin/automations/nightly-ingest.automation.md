---
version: 1
id: nightly-ingest
name: Nightly Ibis ingest
description: Refresh the project knowledge graph overnight - git causality, code chunks and log analysis.
schedule:
  kind: cron
  expression: "0 3 * * *"
  timeZone: local
---
Refresh the Ibis knowledge graph for this workspace:

1. Run the Ibis MCP tool `ingest_code` for the workspace folder.
2. Run `analyze_logs` on any new application logs since the last run (skip if none).
3. Run `load_session_context` once and summarize in 3 bullets: what changed in the
   graph since yesterday, any new gotchas worth a memory, and whether ticketing
   sync is healthy.
4. Do not create memories automatically; only report. Do not modify files.