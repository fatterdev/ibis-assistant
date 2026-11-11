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

## Handoff 2026-11-10 23:05
- **Stato:** step 3/10 completato; **E16 chiuso** (step 1-3). Step 4 (F17) non iniziato.
- **Ultimo commit:** `0e177ba` `docs(planning): record E16 step 3 completion` (codice `90bf10e`; E16 completo `ba89f4c`+`f9b00d5`+`90bf10e`).
- **Gate:** fmt ok · clippy `-D warnings` ok · workspace test **84 suite, 0 failure** · build `ibis-server` ok · retrieval **21/21** (MRR 0.952 / r@5 0.976 / r@15 1.0) · observations **8/8** · live 4.0.0 ok (`live_surreal` 12, `live_memory_estate` 13, `live_observations` 12+1, `live_commit_txn` 2, `live_tasks` 5).
- **Fatto:** E16 completo — schema `task`+modello+config; store/runner/lease/fencing/checkpoint/recovery/prune/shutdown; tool `memory_consolidate` con task id e no doppio spawn; risorse `ibis://tasks[/{id}]`; notifiche create/claim/finish; fixture solo descrizione; 58 tool invariati.
- **Aperto:** F17 step 4 (feature `otel`, `TelemetryConfig`, registry, `shutdown_telemetry`, CI `otel-build`), poi step 5 span. Follow-up E16: test live diretto `read_parsed`/`resource_updated_fires_on_state_change`, test loop-level lease-lost, budget residuo observations, proiezione.
- **Prossimo comando:** aprire la spec F17 §4.1 (righe 555-617) e implementare feature+config; verifica rapida `cargo test -p ibis-logger`.
- **Decisioni richieste:** nessuna.

## Ripresa 2026-11-11 — pre-flight

- **HEAD:** `3f70975` `docs(planning): handoff E16 completion`; branch `main`; worktree pulito.
- **Ambiente:** `cargo 1.98.1`; `surreal 4.0.0` già in ascolto su `ws://127.0.0.1:38000` (sidecar in-memory, root/root, `--allow-all`, PID 35908).
- **Stato:** E16 completo (step 1-3). Riparto dallo **step 4 (F17)**: feature `otel` default off, `TelemetryConfig`, composizione registry, `shutdown_telemetry`, CI `otel-build`.
- **Piano:** step 4 → 5 (span) → 6 → 7 (G19) → 8 → 9 (G20) → 10 (docs, CHANGELOG, coda) → gate completo + live gate.
- **Vincoli:** nessun nuovo tool MCP (58), A1 invariato, `otel` default off e stdio intatto, live su SurrealDB 4.0.0.

## Step 4 - F17 feature, config e shutdown (2026-11-11)

- **Commit:** `ac20039` `feat(telemetry): opt-in OTLP exporter, config and shutdown (F17 ddofds)`.
- **Feature:** `ibis-logger` `otel` (default off) con `opentelemetry` 0.33.1, `opentelemetry_sdk` 0.33.1 (`rt-tokio`), `opentelemetry-otlp` 0.33.1 (`http-proto`, `reqwest-client`, `reqwest-rustls`, `grpc-tonic`), `tracing-opentelemetry` 0.34.0, `tonic` 0.14 (solo tipi metadata). `ibis-server` espone la pass-through `otel = ["ibis-logger/otel"]`.
- **Config:** `TelemetryConfig`/`OtlpConfig` vivono in `ibis-logger` (fonte unica, embedded in `ibis_core::Config.telemetry`): `enabled=false`, endpoint loopback `4318`, `http/protobuf`, `ibis`, ratio 0.1, headers `{}`. Normalizzazione con warn: protocollo ignoto → default, ratio clampata [0,1] (NaN → default), endpoint vuoto → default, service name trim/cap 64. `Debug` redige i valori degli header.
- **Subscriber:** registry + `LevelFilter` + fmt layer identico (ansi/target off, sink invariato); layer OTLP opzionale con filtro `meta.is_span()` (gli eventi dentro gli span non diventano OTel events → nessun contenuto in telemetria); `init` passa da 3 a 4 parametri (unico call-site `main.rs`); feature off + config on → warn una volta dopo l'installazione; `shutdown_telemetry(5s)` in `main` prima della chiusura del DB.
- **Test:** `ibis-logger` 13/13 default e 17/17 `--features otel` (config defaults/overlay, protocol fallback, ratio clamp, endpoint/service name, fmt shape, in-memory exporter, sampled-out, span events filtrati, build di entrambi i transport, flush batch allo shutdown); `ibis-core` test `telemetry_config_defaults`.
- **Gate:** fmt ok; clippy workspace `-D warnings` ok; clippy logger `--features otel` ok; workspace **84 suite, 0 failure**; build `ibis-server` e `ibis-server --features otel` ok; `cargo tree -p ibis-server` senza feature non contiene `opentelemetry`/`tonic`.
- **Deviazioni:** `TelemetryConfig` in `ibis-logger` + nuova dipendenza `ibis-core → ibis-logger` (evita la dipendenza inversa del logger da ibis-core); feature `otel` include `tonic` e i feature `reqwest-client`/`reqwest-rustls`/`internal-logs` oltre ai due della spec (metadata gRPC, client async, errori OTel su tracing); `shutdown_with_timeout` invece di `shutdown`; aggiunti al job CI `live_tasks` (E16 era assente) e clippy OTel.
- **Flaky osservato:** un run workspace ha visto fallire `ibis-ingest-logs::tailer::tests::restart_before_retry_does_not_skip`; il singolo test e il run workspace successivo sono verdi (test pre-esistente timing-sensitive, non toccato).
- **Aperto:** step 5 (span `mcp.tool`/`ai.generate`/`ai.embed`/`memory.consolidate` + whitelist). `fmt_output_is_unchanged` verifica la forma (ansi/target off), non un byte-diff con timestamp.

## Step 5 - F17 span e attributi (2026-11-11)

- **Commit:** `41cdc01` `feat(telemetry): instrument tool, AI and consolidation spans (F17 ddofds)`.
- **Span:** `mcp.tool` in `call_tool` (future instrumentato, non `enter()` attraverso await; actor registrato in `dispatch`; project solo da `args.project_name`; `read_only` da `TOOL_ANNOTATIONS`; `runtime_mode`; outcome `ok`/`error`/`panic` con status Error; nomi fuori registry → `<unknown>`); `ai.generate`/`ai.embed` nei choke point di `Client` (provider, route_hint, temperature, max_output_tokens, finish_reason, token clampati ≥0, `ai.error.kind` senza messaggio); `memory.consolidate` su `execute_project` (path tool), `run_task` claimed, `run_degraded` e path daemon senza task store (project, task id, recoveries, items, summaries, deferred, fallbacks, failed, outcome completed/failed/requeued). Gli span sono creati solo con OTLP attivo (`ibis_logger::otel_active`): default build/config → `Span::none()`, output `fmt` pre-F17 invariato.
- **Attributi:** whitelist chiuse e testate; gli eventi dentro gli span restano filtrati dal layer OTel; layer configurato con `location`/`threads`/`target`/`tracked_inactivity` off (niente `code.*`, `thread.*`, target, `busy_ns`/`idle_ns`); sanitizzazione project/actor (cap 120), provider/finish_reason/route_hint (cap 64), runtime_mode (cap 32, vuoto → `unknown`); `otel.status_code` usato solo per lo status, non esportato come attributo.
- **Exporter:** `reqwest-blocking-client` (il `BatchSpanProcessor` dell'SDK usa `futures_executor::block_on`: il client async reqwest panica senza reactor Tokio); test e2e con collector HTTP locale che riceve davvero lo span `mcp.tool`.
- **Test:** ibis-core 5 (sanitizer + `otel_active` spento), ibis-ai 8 (whitelist generate/embed, token negativi clampati, error kind only, span attivi disabilitati), ibis-server 10 telemetry + 9 tasks (stats cumulativi assign, non somma) + 3 panic guard; `ibis-logger` 17 con feature; e2e `stdio_framing_with_otel_enabled` (stdout strict + drain allo shutdown) e `otel_exporter_receives_tool_span` (collector 455 byte con `mcp.tool`/`get_working_state`); workspace **84 suite, 0 failure**.
- **Gate:** fmt ok; clippy workspace `-D warnings` ok; clippy `ibis-server`/`ibis-logger` `--features otel` ok; build default e feature ok; `cargo tree -p ibis-server` senza feature senza `opentelemetry`/`tonic`.
- **Deviazioni:** gating `otel_active` (necessario per mantenere byte-identico l'output `fmt` di default); `reqwest-blocking-client` invece di `reqwest-client` (vincolo del BatchSpanProcessor); `memory.consolidate` anche sul path tool via `execute_project`, non solo nel runner; `<unknown>` come valore bounded per nomi tool fuori registry (59° valore cardinalità); test attributi con Layer capture custom invece di `opentelemetry_sdk::testing` (evita metrics/logs nel build opt-in); e2e collector come prova reale dell'exporter.
- **Aperto:** step 6 (G19).

## Step 6 - G19 renderer e merge (2026-11-11)

- **Commit:** `2073f73` `feat(cli): deterministic memory projection renderer and merge (G19 ddofds)` + `e0c8c77` hardening (merge/project marker).
- **Modulo:** `crates/ibis-cli/src/memory_project.rs` (client-side, nessuna dipendenza da `ibis-search`): target `agents-md`/`claude-md`, parse del payload `memory_export` (`{"count","items"}`), filtro `rule`/`constraint`/`gotcha` + branch opzionale (righe vuote incluse, confronto case-insensitive Unicode), ordinamento kind → confidence desc → title lowercase → id, cap 20/kind e 60 totali, righe con titolo e contenuto vuoti scartate.
- **Rendering:** header fisso + intro, sezioni solo se non vuote, riga `- **title** — first sentence` con title cap 120 e one-line cap 200 + `…`, fallback titolo alla prima riga del content, sanitizzazione marker (`<!--`/`-->` neutralizzati), control/CR/LF/tab → spazio, whitespace collassato; EOL dal file (LF per un file nuovo); blocco vuoto esplicito.
- **Merge:** matrice completa (append con una sola riga vuota e newline finale, replace esatta della regione marker, rifiuto progetto diverso con messaggio esatto, marker sbilanciati, blocchi multipli, project marker fuori regione); idempotenza; byte fuori regione preservati (incluso file senza newline finale); nome progetto sanitizzato (niente marker injection).
- **Scrittura:** tmp `<file>.ibis-tmp` nella stessa directory + rename, copia permessi best-effort con warning, cleanup su errore, rifiuto directory.
- **Test:** 33 unit (`render_*`, `merge_*`, `write_atomic_*`, `parse_export`, `select_memories`), inclusi tie-break confidence/title/id, cap 120/200, prima frase, CRLF, marker injection, messaggi di rifiuto esatti.
- **Gate:** fmt ok; clippy workspace `-D warnings` ok; workspace **84 suite, 0 failure**.
- **Deviazioni:** `one line` = prima frase del content collassato (fallback all'intero content senza terminatori); `WriteOutcome.warnings` per il warning permessi (stampato dal comando in step 7); `render_block` non applica il filtro branch (il chiamante usa `select_memories`).
- **Aperto:** step 7 (comando `memory project`, seam `execute_cli`/`call_server_tool`, README).
