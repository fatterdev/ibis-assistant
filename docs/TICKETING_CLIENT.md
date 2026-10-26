# Ticketing + PR Client Guide

Operational guide for Ibis Assistant multi-provider ticketing and PR automation.

## 1) Server configuration

Ticketing config file:

- `config/ticketing_config.json`

Use `config/ticketing_config.example.json` as the full template.

Main knobs:

- `default_provider`
- `project_provider_map`
- `providers.redmine|jira|azure_devops`
- `workflow.actions.resolve|close|reopen`
- `reference_patterns`
- `default_create_type`
- `pr.default_target_branch`, `pr.project_target_branch`

## 2) Runtime credential headers

The server uses defaults from config; override per request with:

- Redmine: `X-Redmine-API-Key`
- Jira: `X-Jira-Email`, `X-Jira-API-Token`
- Azure DevOps: `X-Azure-DevOps-PAT`

Optional Azure DevOps routing headers:

- `X-Azure-DevOps-Org`
- `X-Azure-DevOps-Project`
- `X-Azure-DevOps-Repo`

Security policy:

- request credentials are not persisted
- secrets are masked in logs

## 3) MCP ticketing tools

### Discovery

- `ticket_get_capabilities`

### Search and read

- `ticket_search`
- `ticket_search_my`
- `ticket_get`
- `ticket_list_statuses`
- `ticket_search_users`
- `ticket_list_projects`

### Write and workflow

- `ticket_create`
- `ticket_update` (supports `workflow_action=resolve|close|reopen`)
- `ticket_add_comment`
- `ticket_assign`
- `ticket_transition`
- `ticket_mark_resolved`
- `ticket_mark_closed`
- `ticket_reopen`

## 4) Suggested Dev → Tester flow

### Developer

1. Find a ticket: `ticket_search` or `ticket_search_my`
2. Implement the fix on a feature branch
3. Mark resolved: `ticket_mark_resolved` (optional technical `notes`)
4. Open a PR: `repo_pr_create`

### Tester

1. Verify PR / build
2. If OK, close: `ticket_mark_closed`
3. If not OK, reopen: `ticket_reopen`

## 5) MCP PR tools

- `repo_pr_create`
- `repo_pr_complete`

Behavior:

- provider routing matches ticketing (`provider` > project map > default)
- can fall back to an `init_project` session for `project_name` / `origin_url` / `source_branch`
- optional `ticket_ids` plus auto-detect from branch/title/description
- auto-generated title/description with overrides
- optional reviewers on create

## 6) Quick examples

### Mark resolved

```json
{
  "provider": "redmine",
  "id": "123",
  "project_key": "core",
  "notes": "Fix applied, waiting for QA"
}
```

### Create PR

```json
{
  "provider": "azure_devops",
  "project_name": "platform",
  "source_branch": "feature/AB#456-fix-timeout",
  "reviewer_ids": "user-guid-1,user-guid-2",
  "auto_complete": "false"
}
```

## 7) Compatibility

- Legacy `redmine_*` tools are removed.
- Use `ticket_*` only.
- Unified issue graph nodes include provider metadata (`provider`, `external_id`, `external_key`).
