# WQ-5 fase 5 — log di esecuzione

**Spec:** [2026-11-10-dailydoseofds-improvement-analysis-l3-fase-5.md](./2026-11-10-dailydoseofds-improvement-analysis-l3-fase-5.md)
**Data inizio:** 2026-11-10
**HEAD iniziale:** `efa0c89` (`docs(planning): specify WQ-5 phase 5 (E16, F17, G19, G20)`)
**Branch:** `main` (unico branch, worktree unico `C:\devsrc\ibis-assistant`)

## Stato iniziale (pre-flight)

- Worktree pulito; consolidamento dei worktree precedenti completato lo stesso giorno.
- `cargo 1.98.1`; `surreal 4.0.0` su `ws://127.0.0.1:38000` (sidecar in-memory, root/root, `--allow-all`).
- Spec fase 5 committata in `efa0c89`; coda WQ-5 aggiornata con “prossimo: E16 step 1–3”.
- Nessun log precedente: questo è il primo run.

## Piano degli step

| # | Step | Stato |
|---|---|---|
| 1 | E16 — schema `task` + modello puro + config `tasks.*` + probe live UNIQUE/NONE | done (`ba89f4c`) |
| 2 | E16 — `DbTaskStore` + runner + `PassCheckpoint` + recovery/prune + shutdown | da fare |
| 3 | E16 — tool `memory_consolidate` + risorse `ibis://tasks` + notifiche + fixture | da fare |
| 4 | F17 — feature `otel`, config, registry, shutdown, CI | da fare |
| 5 | F17 — span `mcp.tool`, `ai.generate`, `ai.embed`, `memory.consolidate` | da fare |
| 6 | G19 — renderer + merge marker + scrittura atomica + `--check`/`--dry-run` | da fare |
| 7 | G19 — `MemoryAction::Project` + seam CLI + README | da fare |
| 8 | G20 — `harness/profiles.json` + test conformità manifest | da fare |
| 9 | G20 — config `harness.*` + `clientInfo` + `TOOL_GROUPS` + sampling | da fare |
| 10 | Docs, CHANGELOG, coda WQ-5 + gate completo finale | da fare |

## Note di esecuzione

- Gate per step: `cargo fmt --all` → `cargo clippy --workspace --all-targets -- -D warnings` → test mirati → test workspace → commit atomico.
- Gate completo dopo step 3 e dopo step 10; live gate con `IBIS_LIVE_REQUIRED=1`.
- Fuori perimetro: approvazioni con expiry E16, metriche/log OTLP F17, altri target G19, codegen manifest G20, WQ-4 D1.

## Step 1 — E16 schema + modello + config (2026-11-10)

- **Commit:** `ba89f4c` `feat(tasks): durable task schema, pure model and tasks config (E16 ddofds)`.
- **Schema:** `task_status` + tabella `task` (17 campi) + 4 indici (`task_active_key` UNIQUE, `task_status_kind`, `task_project`, `task_finished_at`), tutto `IF NOT EXISTS`.
- **Config:** blocco root `tasks.*` (30/10/3/30) con overlay; i clamp vivono in `TaskPolicy::new` (lease [10,600], heartbeat [1, lease/2], recoveries [1,10], retention [1,365]).
- **Modello puro:** `crates/ibis-optimization/src/tasks.rs` — `active_key`, id 32-hex + `format_record_id`, `TaskStatus`/`recovery_decision`, `TaskCheckpoint`, `remaining_summaries`, SQL builder claim/heartbeat/checkpoint/finish/requeue/recovery/prune.
- **Probe live:** `unique_active_key_blocks_second_active_task` verde su 4.0.0 — il secondo task attivo viola `task_active_key`; quattro terminali con `active_key=NONE` coesistono. **Piano A confermato, niente sentinel `done:{id}`.**
- **Gate:** clippy workspace `-D warnings` verde; `cargo test --workspace --no-fail-fast` 83 suite, 0 failure; unit `ibis-optimization` 12/12; config test verde.
- **Deviazioni minori (review):** id reso `task:⟨32 hex⟩` e validato 32-hex; builder claim/heartbeat prendono `&TaskPolicy` (clamp non bypassabile); `finish_task_sql` accetta solo `TaskStatus` terminale (niente `$terminal` libero); aggiunti shape test recovery/prune e pin cross-crate default config↔policy.

