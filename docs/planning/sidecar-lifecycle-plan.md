# Piano — ciclo di vita sidecar portabile (Windows / Linux / macOS)

> Stato: **proposto, in attesa di approvazione owner** (30/10/2026).
> Primo increment Windows già committato: `39ec177`
> (`fix(runtime): kill sidecar tree on exit (windows job object, lazy)` — ibis-core
> `process_tree` lazy + ProcessManager/runner wiring + CTRL_BREAK graceful + test).
> Nota: il commit include anche un reformat cosmectico rustfmt in
> `ibis-core/src/workspace.rs` (13 righe, test formattati).

## Obiettivo

1. **Ownership esplicita**: ibis possiede solo i processi che avvia lui. Le
   configurazioni che usano servizi esterni già attivi (attach Streamable HTTP,
   SurrealDB/Ollama indipendenti) non creano job/gruppi e non toccano quei processi.
2. **Terminazione dell'albero alla morte di ibis**, per qualunque via di uscita
   (graceful, crash, hard kill) — con le garanzie che ogni OS può dare.
3. **Shutdown grace del DB** per l'integrità (flush del journal surrealkv) su
   tutti i sistemi operativi.
4. API unica in `ibis-core::process_tree`, riusata da `ibis-db` e `ibis-ai`
   (oggi la logica di stop è duplicata).

## Matrice per OS

| Meccanismo | Windows ✅ fatto | Linux | macOS |
|---|---|---|---|
| Spawn config | `CREATE_NEW_PROCESS_GROUP` + ereditarietà job (lazy ensure) | `pre_exec`: `prctl(PR_SET_PDEATHSIG, SIGKILL)` (+ opz. `setpgid(0,0)`) | `pre_exec`: `setpgid(0,0)` (proprio gruppo) |
| Hard-kill tree alla morte del parent | Job Object `KILL_ON_JOB_CLOSE` (kernel) | PDEATHSIG: il kernel consegna SIGKILL al child alla morte del parent | **Nessun equivalente kernel** — best-effort (vedi limiti) |
| Graceful stop child | `CTRL_BREAK_EVENT` al gruppo (se console condivisa) → 2s → `TerminateProcess` | `SIGINT` → 2s → `SIGKILL` | `SIGINT` → 2s → `SIGKILL` |
| Ready wait | TCP dial 500ms→30s (portabile, esistente) | idem | idem |
| Fallback quando il self-attach/job non è disponibile | per-child `AssignProcessToJobObject` | n/a (PDEATHSIG è per-child di natura) | n/a |

## Limiti dichiarati

- **macOS / hard kill**: non esiste né job object né PDEATHSIG. Un `kill -9`
  di `ibis` (o un crash) può lasciare sidecar orfani; sulle vie graceful il
  `killpg` ripulisce. Mitigazione strutturale rimandata al follow-up (reconciliation).
- **Linux / PDEATHSIG race**: finestra standard tra `fork` e `prctl` (millisecondi):
  se il parent muore in quella finestra il child sopravvive. Accettato; nota in codice.
- **Windows / CTRL_BREAK**: consegnabile solo con console condivisa — da VS Code
  (child spawnato via pipe) degrada al kill-after-grace, già coperto dal job object.

## Cambi proposti (1 commit per punto, stile repo)

1. **`ibis-core::process_tree` API portabile**
   - `configure_spawn(cmd)`: Windows→`ensure_children_job()` (già); Linux→`pre_exec`
     PDEATHSIG (+`setpgid`); macOS→`pre_exec` `setpgid(0,0)`.
   - `graceful_stop(child, grace)` async: SIGINT→SIGKILL (unix) / CTRL_BREAK→kill
     (windows) — estratta qui per eliminare la duplicazione db/ai.
2. **`ibis-db::ProcessManager`**: usa `configure_spawn` + `graceful_stop`
   (il CTRL_BREAK Windows resta; su unix SIGINT→SIGKILL).
3. **`ibis-ai::OllamaProcess`**: idem; su unix `stop()` diventa SIGINT+grace
   invece del kill secco attuale.
4. **Docs**: header `process/mod.rs` aggiornati; limite macOS documentato in
   `process_tree.rs` e README runtime.
5. **Test/verifica**:
   - Windows (locale + CI): e2e spawn `ibis -mode stdio` con data dir temporanea →
     `taskkill /F` → nessun residuo `surreal.exe`/`ollama.exe`.
   - Linux (CI ubuntu): test e2e PDEATHSIG (child spawnato muore alla morte del parent).
   - `live_surreal` (round-trip start/stop) resta verde su entrambe le matrici CI.
   - macOS: nessun runner CI oggi — verifiche manuali owner se/punti servono (nota).
6. **Follow-up (fuori scope, candidato Track D)**: *post-crash reconciliation* —
   all'avvio, rilevare e terminare sidecar stale legati allo stesso data dir/porta
   (utile specie su macOS e per dati condivisi tra plugin child).

## Rischi

- `pre_exec` + PDEATHSIG: richiede care (unsafe) ma pattern standard; il rischio
  race è documentato e accettato.
- Il job object su Windows può fallire se il parent harness proibisce il nesting:
  già gestito (fallback per-child + warning).
- Estensione della surface `windows-sys` in ibis-db (Console) e ibis-core
  (JobObjects): dipendenze compile-time-only, già valide in CI.

## Cosa NON cambia

- Le configurazioni attach/esterno: nessuno spawn → nessuna ownership (principio
  già implementato nell'increment Windows, esteso identico agli altri OS).
- La porta/credenziali del DB e il layout data dir.

**Decisione richiesta dall'owner**: approvare il piano (o ridurne lo scope —
es. macOS best-effort ora, reconciliation dopo) prima di procedere con i punti 1–4.

## Stato di esecuzione (aggiornato 30/10/2026)

**Fatto e committato:**
- `39ec177` — increment Windows: `ibis-core::process_tree` (job object lazy
  KILL_ON_JOB_CLOSE, `ensure_children_job()` chiamato *prima* dello spawn nei
  soli siti che avviano child; fallback per-child; `children_job_active()`),
  wiring in `ibis-db::ProcessManager` + `ibis-ai::OllamaProcess`,
  CTRL_BREAK graceful (spawn in process group proprio), probe e2e
  (`examples/job_probe.rs`: child verificato morto dopo hard exit del parent).
  Test: ibis-core 40/40, ibis-db lib 20/20. FFI windows-sys (la FFI manoscritta
  falliva con error 87 — usare windows-sys, non riportarla).
- `86565de` — questo piano.

**Operazioni pendenti (macchina owner, Windows):**
1. `cargo install --path crates/ibis-server --bin ibis --force` — il binario su
   PATH `C:\devbin\cargo\bin\ibis.exe` è ancora **stale 0.1.0** (pre-fix
   BatchManager: log loop `Expected string, got record` in
   `%LOCALAPPDATA%\ibis\plugin\plugin.log`).
2. `cargo xtask dist` — ricostruire `dist\ibis.exe` (quello attuale è delle
   11:47, pre-job-object) e verificare `ibis --version` → `0.2.0`.
3. e2e orfani con il binario nuovo: spawn `ibis -mode stdio` (IBIS_DATA_DIR
   temporanea) → kill → nessun residuo `surreal.exe`/`ollama.exe`.
4. Poi riprendere il gate: R3.3 della checklist (espansione tree) era bloccata
   proprio dal binario stale.

**Decisioni owner pendenti:** punti 1–4 del piano (approvazione), scope macOS
best-effort, follow-up reconciliation (candidato Track D).

**Percorsi macchina rilevanti:** engine locale `C:\devbin\paoai-strix-engine`
(modelli in `models\`, servizi nssm `StrixServer` :8080, `StrixEmbed` :8081,
`StrixUITars` :8082 per i test UI); runner UI test `scripts/ui-test/`.