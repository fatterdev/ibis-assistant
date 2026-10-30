# Piano — ciclo di vita sidecar portabile (Windows / Linux / macOS)

> Stato: **implementato** (30/10/2026 — punti 1–4 + studio ottimali;
> reconciliation resta candidato Track D).
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
| Hard-kill tree alla morte del parent | Job Object `KILL_ON_JOB_CLOSE` (kernel) | PDEATHSIG: il kernel consegna SIGKILL al child alla morte del parent | **Nessuna primitiva kernel** — watchdog child-side (shim pipe-EOF, vedi studio §Meccanismi ottimali) |
| Graceful stop child | `CTRL_BREAK_EVENT` al gruppo (se console condivisa) → 2s → `TerminateProcess` | `SIGINT` → 2s → `SIGKILL` | `SIGINT` → 2s → `SIGKILL` |
| Ready wait | TCP dial 500ms→30s (portabile, esistente) | idem | idem |
| Fallback quando il self-attach/job non è disponibile | per-child `AssignProcessToJobObject` | n/a (PDEATHSIG è per-child di natura) | n/a |

## Limiti dichiarati

- **macOS / hard kill**: non esiste né job object né PDEATHSIG. Un `kill -9`
  di `ibis` (o un crash) può lasciare sidecar orfani; sulle vie graceful il
  `killpg` ripulisce. Mitigazione strutturale: **shim watchdog child-side**
  (pipe-EOF / kqueue NOTE_EXIT) + reconciliation all'avvio — studio completo in
  §Meccanismi ottimali; l'implementazione è pianificata nel punto 1 esteso.
- **Linux / PDEATHSIG race**: finestra standard tra `fork` e `prctl` (millisecondi):
  se il parent muore in quella finestra il child sopravvive. Accettato; nota in codice.
- **Windows / CTRL_BREAK**: consegnabile solo con console condivisa — da VS Code
  (child spawnato via pipe) degrada al kill-after-grace, già coperto dal job object.

## Studio — meccanismi ottimali per OS (30/10/2026)

Studio per portare la matrice da "best-effort" a **garanzie classificate**.
Fatti kernel verificati su man-pages 6.19 (`PR_SET_PDEATHSIG(2const)`), docs
Microsoft Job Objects, Apple kqueue(2). Il design è **a strati**: il layer 1
sola da solo su Windows/Linux; il layer 2 colma macOS; il layer 3 rende la
pulizia *eventualmente completa* su tutte le piattaforme.

### Layer 1 — ownership kernel al momento dello spawn

**Windows (già implementato, `39ec177`) — garanzia kernel hard.**
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: il kernel termina tutti i membri quando
l'ultimo handle del job si chiude; noi leakhiamo il handle di proposito, quindi
la terminazione di `ibis` per *qualsiasi* via chiude gli handle e innesca il
kill. Copre anche i **nipoti**: ogni processo figlio di un membro entra nel job
automaticamente (ereditarietà), l'unica via d'uscita è
`CREATE_BREAKAWAY_FROM_JOB` (nessun nostro sidecar lo usa). Caveat noti e
gestiti: nesting negato da un job padre (IDE/CI) → fallback per-child
`AssignProcessToJobObject`; da Windows 8+ il nesting multiplo è supportato.

**Linux — garanzia kernel hard con due caveat, entrambi chiudibili.**
`prctl(PR_SET_PDEATHSIG, SIGKILL)` via `pre_exec` (gira nel child dopo fork,
prima di `execve` — funziona con binari esterni tipo surreal/ollama, non
serve cooperare col child). Fatti verificati dalla man page:

1. Il segnale arriva alla morte del **thread** creatore, non del processo
   ("the parent in this case is considered to be the thread that created this
   process"). Vincolo implementativo: gli spawn sidecar devono avvenire su
   **thread long-lived** (worker del runtime tokio principale, che vive quanto
   il processo) — **mai dentro `spawn_blocking`**: il thread rilasciato alla
   fine del task consegnerebbe SIGKILL al sidecar con ibis ancora vivo.
2. Se il padre (thread + subreaper ancestrali) è **già morto** al momento del
   `prctl`, il segnale non viene mai consegnato → la race fork→prctl va chiusa
   con il check canonico `getppid()` dopo `prctl` (pattern richiesto e
   sufficiente; finestra residua: nessuna, il check è deterministico).
3. L'impostazione **sopravvive a `execve`** (surreal/ollama non sono
   setuid/setgid né con capabilities file; in quei casi verrebbe azzerata).

Limite strutturale PDEATHSIG: copre **solo il child diretto**. I processi
runner di `ollama serve` (subprocess) sopravviverebbero a un hard-kill del
parent: su Linux la copertura dei nipoti va al layer 2/3 (cgroup o shim) —
surreal non spawna figli, quindi per il DB il PDEATHSIG da solo basta.
Opzione enterprise per coprire l'albero intero kernel-side: cgroup v2 dedicato
con `cgroup.kill` (kernel ≥ 5.14) o `systemd-run --scope` — fuori scope per un
binario desktop, documentata come opzione server-grade.

**macOS — nessuna primitiva kernel (nemmeno in BSD: FreeBSD ha `procctl`,
macOS no).** Il layer 1 su macOS non esiste; la copertura si sposta al layer 2.

### Layer 2 — watchdog child-side (colma macOS, opzionale su Linux)

Pattern ottimo per OS senza primitiva kernel: **shim supervisor**. `ibis`
spawna uno shim (fork); lo shim a sua volta spawna il sidecar e fa watchdog:

- `ibis` crea una pipe e **tiene il write-end** aperto per tutta la vita del
  processo; lo shim eredita il read-end.
- Lo shim fa `setpgid` del sidecar (proprio gruppo) e blocca su `read()` della
  pipe. Morte di `ibis` per **qualsiasi** via (graceful, crash, SIGKILL,
  logout) → il kernel chiude il write-end → EOF → lo shim fa `killpg(SIGKILL)`
  sul gruppo del sidecar. Event-driven, zero polling, latenza sub-ms.
- Copre anche i **nipoti** (runner di ollama): stesso gruppo via `setpgid`.
- Residuo dichiarato: se lo *shim stesso* viene SIGKILLed nel medesimo istante
  del parent, il sidecar sopravvive → il layer 3 chiude il cerchio.

Varianti accettabili, in ordine di preferenza:

1. **Binario shim dedicato** (es. `ibis-sidecar-shim` accanto a `mcp-bridge`
   in `dist/`) — zero dipendenze shell, controllo completo del reaping (wait
   del sidecar, kill del gruppo, exit-code propagato).
2. **kqueue `EVFILT_PROC` `NOTE_EXIT`** sul pid del parent al posto della
   pipe — stesso processo watcher, event-driven, più codice (libc kqueue),
   utile solo se lo shim deve osservare più cose; la pipe resta più semplice
   e portabile anche a Linux.
3. **`/bin/sh -c` watchdog** — zero build extra ma fragile (quoting, processi
   shell extra, comportamento read(1) variabile). Da evitare.

### Layer 3 — reconciliation all'avvio (tutte le piattaforme)

All'avvio, `ibis` rileva e termina i sidecar stale legati allo *stesso data
dir / porta*: PID-file nel data dir + verifica liveness (`kill(pid, 0)` su
unix, handle/`OpenProcess` su Windows, con controllo di process start-time per
escludere PID-reuse). È già candidato Track D (punto 6 del piano) — questo
studio lo promuove a **componente strutturale**: è l'unico layer che copre gli
edge di tutti gli altri (shim SIGKILLed, cgroup assenti, macchine senza
kernel-kill) e trasforma "best-effort" in pulizia *eventualmente completa*.

### Sintesi garanzie

| Via d'uscita di ibis | Windows | Linux | macOS (con shim) |
|---|---|---|---|
| Graceful stop | CTRL_BREAK/kill + job | SIGINT→SIGKILL (killpg) + PDEATHSIG | SIGINT→SIGKILL (killpg) + shim inutile ma innocuo |
| Crash / SIGKILL di ibis | job (kernel) | PDEATHSIG (child diretto; nipoti via layer 3 o cgroup) | shim pipe-EOF → killpg (kernel chiude la pipe) |
| Shim/watchdog a sua volta ucciso | n/a (job è kernel-side) | PDEATHSIG resta kernel-side | n/a → layer 3 reconciliation |
| Nipoti (ollama runner) | job per ereditarietà | non coperti da PDEATHSIG → cgroup/shim/reconciliation | killpg dello shim li copre |

### Sketch per il punto 1 del piano (unix `configure_spawn`)

```rust
// catturato PRIMA dello spawn, nel parent:
let parent_pid = std::process::id() as libc::pid_t;

#[cfg(unix)]
unsafe fn pre_exec_child() {
    libc::setpgid(0, 0); // proprio gruppo: killpg copre nipoti nelle vie graceful
    #[cfg(target_os = "linux")]
    {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        // chiude la race fork→prctl: se il parent è già morto, esce subito
        if libc::getppid() != PARENT_PID { libc::_exit(1); }
    }
}
// cmd.before_exec / unsafe pre_exec(pre_exec_child) — pre_exec è la forma
// corretta (unsafe, async-signal-safe solo libc::_*; setpgid/prctl/getppid/_exit lo sono)
```

Vincoli implementativi che lo studio aggiunge al punto 1:
- spawn dei sidecar **solo** da thread long-lived (worker runtime principale),
  mai `spawn_blocking` (caveat thread di PDEATHSIG, sopra);
- `setpgid(0,0)` anche su Linux (non solo macOS) così `killpg` graceful copre
  i nipoti uniformemente;
- PDEATHSIG non fornisce liveness al parent: la graceful/kill path esistente
  resta necessaria (PDEATHSIG è solo la rete di sicurezza del parent-death).

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

- `pre_exec` + PDEATHSIG: richiede care (unsafe) ma pattern standard; la race
  fork→prctl è **chiudibile** con il check `getppid()` post-`prctl` (studio
  §Meccanismi ottimali) — più una race residua accettata. Caveat thread:
  spawn solo da thread long-lived (mai `spawn_blocking`).
- Il job object su Windows può fallire se il parent harness proibisce il nesting:
  già gestito (fallback per-child + warning).
- Estensione della surface `windows-sys` in ibis-db (Console) e ibis-core
  (JobObjects): dipendenze compile-time-only, già valide in CI.

## Cosa NON cambia

- Le configurazioni attach/esterno: nessuno spawn → nessuna ownership (principio
  già implementato nell'increment Windows, esteso identico agli altri OS).
- La porta/credenziali del DB e il layout data dir.

**Esito decisione owner**: approvato e implementato in giornata (30/10/2026)
con lo scope esteso dello studio — macOS con watchdog pipe-EOF, non solo
best-effort; reconciliation confermata come follow-up (Track D).

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

**Fatto (30/10/2026, macchina owner, Windows — piano implementato):**
- **Punti 1–4 implementati** (1 commit per punto, stile repo):
  - `696a6e4` — punto 1: `ibis-core::process_tree` API portabile —
    `configure_spawn` (Windows: job lazy + process group proprio; Linux:
    `setpgid` + PDEATHSIG con race-close `getppid()`; macOS: `setpgid`) +
    `graceful_stop` condivisa (SIGINT/CTRL_BREAK al gruppo → kill dopo grace)
    + **watchdog pipe-EOF macOS** (`ibis -sidecar-watchdog <pgid>`, hidden mode
    che short-circuita prima di ogni init) + e2e PDEATHSIG Linux
    (`tests/process_tree_portable.rs`, self-respawn) + round-trip unix di
    `graceful_stop`. Cross-check su target `x86_64-unknown-linux-gnu` e
    `aarch64-apple-darwin` (quì ha colto `pipe2` assente su Apple → pipe +
    `FD_CLOEXEC`).
  - `031914b` — punto 2: `ProcessManager` su `configure_spawn`/`graceful_stop`;
    `windows-sys` e `libc` escono da ibis-db (FFI solo in ibis-core).
  - `3a45ece` — punto 3: `OllamaProcess` su tokio Child + `graceful_stop`
    (su unix SIGINT+grace invece del kill secco; guard fuori dall'await per
    future Send).
  - Docs: header `process/mod.rs` e `process_tree.rs` aggiornati; sezione
    **"Sidecar lifecycle"** nei docs di prodotto (`docs/runtime_modes.md`) +
    nota nel README (albero crate); questo piano aggiornato.
- **Studio meccanismi ottimali**: sezione §Studio sopra (layered design,
  fatti kernel verificati su man-pages 6.19, shim/watchdog macOS,
  reconciliation promossa a layer strutturale, vincolo thread long-lived).
- **Verifica Windows locale** (nuova dopo l'implementazione):
  ibis-core 40+1/41, ibis-db 20/20 lib + 15 + 7/7 `live_surreal`
  (incluso `embedded_process_lifecycle` con la `graceful_stop` nuova),
  ibis-ai 126+3/129; clippy workspace `-D warnings` verde; e2e orfani
  ri-eseguito con il codice nuovo → **PASS** (surreal + ollama spawnati,
  `taskkill /F` di ibis, zero orfani).
- **Op macchina (mattina)**: binario PATH → 0.2.0 (`cargo install`),
  `dist\ibis.exe` ricostruito → 0.2.0 (il precedente era corrotto, 1 byte,
  oggi 15:39 — se il placeholder torna, indagare chi lo scrive).

**Sbloccato:** il gate R3.3 della checklist (espansione tree) riparte da
questo stato: binario PATH e dist entrambi 0.2.0 post-job-object, API
portabile committata, e2e verde.

**Rimanente / decisioni owner:**
- Linux CI: il test PDEATHSIG (`process_tree_portable`) gira alla prossima
  esecuzione della matrice ubuntu (non eseguibile su macchina Windows).
- macOS: nessun runner CI — verifica manuale owner (watchdog armato + graceful)
  quando serve; codice cross-checkato per compilazione su apple target.
- Punto 6 (reconciliation) resta candidato Track D — promosso a layer
  strutturale dallo studio, non ancora implementato.
- **Percorsi macchina rilevanti:** engine locale `C:\devbin\paoai-strix-engine`
  (modelli in `models\`, servizi nssm `StrixServer` :8080, `StrixEmbed` :8081,
  `StrixUITars` :8082 per i test UI); runner UI test `scripts/ui-test/`.