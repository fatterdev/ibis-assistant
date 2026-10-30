# UI test runner (R3/R4 Parte B) — computer-use validation

Automates the **manual UI verification** of `docs/planning/review-r3r4-checklist.md`
(Parte B) by preparing the environment, generating paste-ready tasks for a
computer-use agent, and collecting evidence into `artifacts/ui-tests/<run>/`.

Under test: VSIX `fatterdev.ibis-assistant` (R3.1–R3.8) + agent plugin `ibis`
(P4.1–P4.6). Task definitions live in [tasks.json](tasks.json) — edit there,
not in the runner.

## Prerequisites

- **UI-TARS Desktop** (v0.2.4, winget `ByteDance.UI-TARS`):
  `%LOCALAPPDATA%\UiTars\UI-TARS.exe`. Configure a VLM provider in its
  Settings (UI-TARS / VolcEngine Doubao / any OpenAI-compatible VLM) — the app
  ships without a model.
- VS Code 1.144+ with the built VSIX
  (`vscode-extension/ibis-assistant-1.0.0.vsix`, built by `npm run package`).
- For R3.3/R3.4 checks: `ibis` binary + Ollama with an embedding model
  (semantic `memory_search`).
- For P4.*: the plugin installed from source (`Chat: Install Plugin from
  Source` → `plugin/`, or `chat.pluginLocations`).

## Workflow

```powershell
# 1. environment
node scripts/ui-test/run-ui-tests.mjs prep --install-vsix --launch

# 2. new evidence run
node scripts/ui-test/run-ui-tests.mjs new --name r3r4

# 3. per task: paste the printed prompt into UI-TARS Desktop (Local Operator)
node scripts/ui-test/run-ui-tests.mjs prompt R3.2
#    -> point the UI-TARS screenshot folder at artifacts/ui-tests/<run>/
#    -> the agent performs the task and drops <taskId>-*.png files

# 4. judge each task (human or the agent itself) and record it
node scripts/ui-test/run-ui-tests.mjs mark R3.2 pass --note "glyphs + click ok"

# 5. refresh mappings + report
node scripts/ui-test/run-ui-tests.mjs collect
node scripts/ui-test/run-ui-tests.mjs report
```

Outputs per run folder: `results.json` (machine-readable) and `results.md`
(table ready to paste into the review checklist / PR description).

## Statuses

- `pass` / `fail` — explicit judgement via `mark`.
- `evidence-only` — files matching `<taskId>*` exist but no judgement yet.
- `pending` — nothing collected.

Evidence naming: the first token before `-`/`_` must equal the task id
(e.g. `R3.2-01-status-bar.png`). Case-insensitive.

## Headless option (optional)

UI-TARS Desktop is interactive by design. For unattended browser-scope tasks
use the Agent TARS CLI from the same stack instead
(`npx @agent-tars/cli@latest --provider <vlm> --model <model> --apiKey ...`)
and drive it with the same `prompt` output; desktop-GUI steps still need the
desktop app (or the experimental `@gui-agent/cli` shipped with the v0.3.x
releases).