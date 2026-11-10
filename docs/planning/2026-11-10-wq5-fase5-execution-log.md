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
| 2 | E16 — `DbTaskStore` + runner + `PassCheckpoint` + recovery/prune + shutdown | done (`f9b00d5`) |
| 3 | E16 - tool `memory_consolidate` + risorse `ibis://tasks` + notifiche + fixture | done (`90bf10e`) |
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

## Step 2 — E16 store, runner e checkpoint (2026-11-10)

- **Commit:** `f9b00d5` `feat(tasks): task store, runner, checkpoint seam and shutdown (E16 ddofds)`.
- **Store:** `crates/ibis-server/src/tasks.rs` — `TaskStore` seam + `DbTaskStore` (create-or-get con corsa UNIQUE, claim, heartbeat, checkpoint, finish, requeue, recovery, prune, get/active).
- **Runner:** lease owner per processo, heartbeat tokio indipendente, checkpoint writer per item/scope (budget base sui tentativi precedenti), stop al confine item, shutdown requeue senza recovery, fail-open su store/errori di bookkeeping, errore troncato a 2 KB.
- **Seam:** `PassCheckpoint` in `ibis-optimization::tasks`; `consolidate_loop`/`observations_pass` accettano `Option<&dyn PassCheckpoint>`; cap sintesi residuo dal task; `items_total` scritto subito; `scope_done` per scope.
- **Wiring:** `AppState.tasks` (Mutex + accessor), runner creato al boot e su `bind_db` (plugin), recovery/prune al boot indipendenti dal loop opt-in, queued-first prima della discovery, `consolidation::start` ritorna il JoinHandle e lo shutdown attende (abort a 10s) prima di chiudere il DB.
- **Live:** `crates/ibis-server/tests/live_tasks.rs` 4/4 su 4.0.0 — claim/reclaim/fencing/checkpoint, recovery 3 reclaim poi failed, shutdown requeue, prune `RETURN BEFORE`.
- **Unit:** 7/7 runner con fake store/pass; `cargo test --workspace --no-fail-fast` 84 suite, 0 failure; clippy `-D warnings` verde.
- **Aperto (follow-up dichiarati):** test loop-level “item N+1 non parte dopo lease lost” (oggi test diretto su `should_continue`), test `observations_pass` con budget residuo 0, asserzione della proiezione `maybe_project_memory`; prossimo step 3.

## Step 3 — E16 superfici: tool, risorse, notifiche (2026-11-10)

- **Commit:** `90bf10e` `feat(tasks): tool, task resources and notifications (E16 ddofds)`.
- **Tool `memory_consolidate`:** stessi argomenti; `begin` create+claim restituisce subito; testi “started” / “Resumed … attempt N” / “already running … status running, attempt N” con id; `database not connected` prima dello spawn; actor in `requested_by`; il pass usa il progetto salvato nel task.
- **Risorse:** `ibis://tasks` (attivi, JSON) e `ibis://tasks/{id}`; template `ibis://tasks/{id}` (4 totali); collection `ibis://tasks` in `resources/list`; id JSON reso `task:<hex>`; DB assente = resource not found.
- **Notifiche:** `ResourceTaskEvents` su create/claim/finish/requeue (non checkpoint), fire di `ibis://tasks` + varianti plain/hex/guillemet del per-id; collegato al boot e al `bind_db` plugin.
- **Fixture:** rigenerata solo per la descrizione di `memory_consolidate` (diff 1 riga); contract 58 invariato.
- **Test:** `resources_protocol_test` 5/5 (template 4, list 3), `curator_app_test` 3/3, `discovery_snapshot_test` verde, `live_tasks` 5/5 (begin claim-once + JSON row).
- **Gate completo:** fmt ok; clippy `-D warnings` ok; workspace **84 suite, 0 failure**; `cargo build -p ibis-server` ok; retrieval **21/21** MRR 0.952 / r@5 0.976 / r@15 1.0; observations **8/8** (primo run 7/8 per errore transitorio di transazione sul sidecar in-memory, rerun verde); live 4.0.0: `live_surreal` 12/12, `live_memory_estate` 13/13, `live_observations` (search) 12/12 + (optimization) 1/1, `live_commit_txn` 2/2, `live_tasks` 5/5.
- **Aperto:** test live diretto di `read_parsed` (`task_resources_read_rows`) e della notifica `resource_updated_fires_on_state_change`; oggi coperti parse/JSON unit + live row serialization. Prossimo step 4 (F17).




## Handoff 2026-11-10 22:14
- **Stato:** step 2/10 completato; step 3 non iniziato.
- **Ultimo commit:** `b591a80` `docs(planning): record E16 step 2 completion` (codice `f9b00d5`).
- **Gate:** fmt ok · clippy `-D warnings` ok · workspace test ok (84 suite, 0 failure) · eval non eseguita in questo step · live `live_tasks` 4/4, `live_surreal` 12/12, `live_observations`/`live_memory_estate` incluse nel workspace test con sidecar attivo.
- **Fatto:** `task` schema+modello+config (step 1, `ba89f4c`); `DbTaskStore`+`TaskRunner`+`PassCheckpoint`+heartbeat/fencing/checkpoint/recovery/prune/shutdown (step 2, `f9b00d5`); wiring `AppState.tasks` (Mutex + accessor + `bind_db` plugin), recovery/prune al boot, queued-first, shutdown ordinato con abort a 10s; live 4.0.0 per claim/reclaim/fencing/checkpoint/recovery/prune; unit runner 7/7.
- **Aperto:** step 3 (tool `memory_consolidate` con task id/no doppio spawn, risorse `ibis://tasks[/{id}]`, notifiche `resources/updated`, fixture discovery); poi gate completo (eval retrieval/observations) e live gate. Follow-up dichiarati: test loop-level lease-lost, test budget residuo observations, asserzione proiezione; notifiche task ancora da collegare al runner (serve un sink eventi).
- **Prossimo comando:** `git status --short; cargo test -p ibis-server tasks::tests` (verifica ripresa), poi aprire `docs/planning/2026-11-10-dailydoseofds-improvement-analysis-l3-fase-5.md` §4.5 e implementare lo step 3.
- **Decisioni richieste:** nessuna.
