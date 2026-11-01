# Brainstorm — Memory view VSIX + Team server

> Bozza per la sessione di brainstorming con l'owner (convenzione superpowers:
> questo documento raccoglie domande aperte, opzioni e trade-off; le decisioni
> aprono la spec in `docs/superpowers/specs/<data>-memory-view-team-server-design.md`
> e la nuova fase in [PLAN.md](PLAN.md)).
>
> Provenienza: round di debug "blocchi e lentezze" chiuso in S1 (01/11/2026,
> client stdio persistente in ide-host + fix pipeline embed), gate R3/R4 in
> corso (Parte B), e i temi rimasti fermi da F0: l'evoluzione della memory
> tree dopo il primo uso reale e la modalità **team server** del runtime.
>
> Ambito confermato: **entrambi** i temi. Track D (Microsoft Scope) resta
> differita. Sequencing previsto: decisioni qui → spec → fase in PLAN.md →
> runtime `0.3.0` / VSIX `1.1.0`.

## 0. Stato di partenza (cosa è già deciso/fatto)

| Deciso | Dove |
|---|---|
| Client stdio persistente per le interazioni VSIX (1 child per backend identity, handshake 1×, idle 120s, retry su child morto) | `ide-host/src/index.js` (`StdioSession`), commit `7f266fd` |
| Browse kind senza embedding (`memory_list`: kind/status/visibility/limit/actor) | `crates/ibis-server/src/tools_memory.rs`, `vscode-extension/memory-tree.js` |
| Embed pipeline locale: keep_alive 30m, cache query, FIFO limiter | `crates/ibis-ai/src/ollama.rs` |
| Sidecar lifecycle portabile (job object / PDEATHSIG / watchdog macOS) + **reconciliation Layer 3 studiata ma non implementata** | [sidecar-lifecycle-plan.md](sidecar-lifecycle-plan.md) §Studio |
| Memoria tipizzata: kind/status/visibility (team default)/owner/supersession/provenance | [superpowers/specs/2026-10-09-memory-absorption-design.md](../superpowers/specs/2026-10-09-memory-absorption-design.md) |
| Claim store git (`.ibis/claims/`) come SoT durevole, `db_only` default su team server | [superpowers/specs/2026-10-24-git-claim-store-addendum.md](../superpowers/specs/2026-10-24-git-claim-store-addendum.md) |
| Risorse MCP live (`resources/subscribe` + `resources/updated` su mutazioni memoria) | F0.4, `smoke-mcp.mjs` |
| Identity: `X-Ibis-Actor` (HTTP) / arg `actor` (stdio) — hint, non credenziale | [runtime_modes.md](../runtime_modes.md), [memory_layer.md](../memory_layer.md) |
| MCP App "Memory Curator" con degradazione testuale garantita | F0.7 |

Vincoli non negoziabili (da DNA guardrails): SurrealDB resta SoT; nessuna ACL
row-level (`visibility` è sharing-first, `owner` è attribuzione); nessuna
multi-tenant SaaS; la proiezione `.ibis/memory` è output rigenerabile, non sync.

---

## 1. Ambito A — Memory view (VSIX)

### A1. Filtri status/visibility nell'albero

`memory_list` supporta già `status` e `visibility`, ma la tree li chiama
senza filtri: i kind node mischiano `current`, `superseded`, `archived`,
`stale` (la descrizione mostra lo status quando ≠ current — scelto per il
browse "tutto visibile").

**Opzioni:**
- **(a)** Filtro globale di view (`ibis.memory.statusFilter`: `all` (default) |
  `current`), badge nello title bar della view. Semplice, uno stato solo.
- **(b)** Per-kind: ogni kind node espone un menu per filtrare. Più flessibile,
  UI più pesante.
- **(c)** Due viste affiancate (Explorer + "Archive"). Costosa, dubbia utilità.

**Raccomandazione bozza:** (a) con default `all` e switch rapido — il browse
serve anche al curation ("cosa c'è da supersediare?"), nascondere i
non-current di default farebbe perdere quello use case.

### A2. Grouping: workstream / tag / repo

Il modello ha `workstream` + edge `part_of_workstream`, ma `memory_list` non
espone filtri per esso (serve estendere la query con un join/edge filter) e
la tree è piatta per kind.

**Opzioni:**
- **(a)** Nesting a due livelli: kind → workstream/tag → entry. Richiede:
  estensione `memory_list` (arg `workstream`/`tag` + un `memory_workstreams`
  list) + UI. 
- **(b)** Vista "Workstreams" come secondo tree provider nella stessa view
  (container con 2 viste). Separazione netta browse-per-kind vs browse-per-topic.
- **(c)** Niente grouping per ora; i tag restano nella descrizione/dettaglio.

**Raccomandazione bozza:** (b) come obiettivo dell'iterazione, (c) come
fallback se il costo dell'estensione server slitta; (a) evitato (gerarchia
doppia difficile da scorrere).

### A3. Azioni di curation inline

Oggi la tree è read-only (dettaglio in editor virtuale). Il curator App (F0.7)
copre la curation ricca ma è experimental in VS Code.

**Opzioni:**
- **(a)** Context menu sulle entry: `Supersede…`, `Archive`, `Update content…`,
  `Share (personal↔team)` — tutti mutanti con conferma esplicita (coerente
  con la policy annotations: `readOnlyHint: false`).
- **(b)** Solo `Share` + `Archive` (le due operazioni a basso rischio),
  supersede/update restano al curator App / agenti.
- **(c)** Nulla di nuovo: la curation passa per `memory_consolidate`.

**Raccomandazione bozza:** (b) nell'iterazione 1.1.0 — l'owner curation dal
tree senza aprire chat; supersede richiede contenuto nuovo → quick pick +
input box è fragile, meglio lasciarlo al curator/app o a un agente.

### A4. Live refresh via `resources/updated`

F0.4 espone già subscription + notifiche, ma il tree refresha solo manuale.
Con il child persistente (S1.1) la connessione vive abbastanza da rendere la
subscription utile.

**Opzioni:**
- **(a)** Il tree si iscrive a `ibis://memory/**` e refresha il kind node
  toccato (coalesce con debounce ~500ms).
- **(b)** Refresh on focus della view (evento VS Code) — più economico, quasi
  equivalente in pratica.

**Raccomandazione bozza:** (b) subito, (a) solo se il feedback "aggiornamento
live" emerge dall'uso reale. La subscription multiplexata su sessione
persistente è nuovi protocollo in ide-host → costo non banale.

### A5. Topologia backend VSIX: due child coesistenti?

Stato attuale: il **provider MCP** registrato dalla VSIX spawn `ibis` per la
chat (posseduto da VS Code), e il **client persistente** dei comandi spawn il
suo child. Due child, stessa data dir: il secondo si attacca al SurrealDB
sidecar del primo (start_embedded fallisce su porta occupata → connect).
Funziona, ma sono due ingest lane e due batch manager potenzialmente attivi.

**Opzioni:**
- **(a)** Status quo documentato (due child, attacco al sidecar condiviso).
- **(b)** I comandi riutilizzano il backend HTTP quando `ibis.runtime.mode=attach`
  è attivo (già così) e in spawn mode… non c'è via pubblica per parlare col
  server del provider → restano due child. Documentare il limite.
- **(c)** La VSIX rinuncia al provider interno in favore di un daemon locale
  sempre-on (spawn `ibis run` attach): un solo runtime, ma cambia il modello
  di installazione/upgrade (lifecycle del daemon fuori dal VS Code).

**Raccomandazione bozza:** (a) per la 1.1.0, con (c) come candidata naturale
del tema team server (sotto) — se il team server introduce il daemon
personale, la VSIX lo adotta come backend unico e i due child collassano in
uno. Da decidere INSIEME a T1.

### A6. Search box: memory_search resta semantico

Con il limiter FIFO la search singola è stabile; il costo è un embedding per
query (cache copre i repeat). Nessuna azione prevista; unico open: mostrare
lo status "embedding…" durante la fetch (attesa percepita).

---

## 2. Ambito B — Team server

### T1. Deployment target del "team server"

La tabella deployment in `docs/memory_layer.md` ha già `personal` (daemon
locale condivisibile) e `server` (host dedicato). Non esiste però nulla che
guidi l'installazione del secondo.

**Opzioni:**
- **(a)** Documentazione runbook (binding LAN, systemd/NSSM/Task Scheduler,
  data dir dedicata, backup del data dir).
- **(b)** `ibis serve --team` subcommand: binding configurabile, token opzionale,
  proiezione `.ibis/memory` abilitata di default, log strutturati.
- **(c)** docker-compose ufficiale (immagine runtime + SurrealDB server).

**Raccomandazione bozza:** (b) come obiettivo con (a) come primo step del
runbook; (c) rimandato finché non c'è domanda reale (il runtime è già
self-contained).

### T2. Identity su HTTP remoto

`X-Ibis-Actor` è un hint (trust = "raggiungi l'istanza"). Su un team server
LAN il modello regge (stesso boundary di git credentials), ma manca la
pratica: chi imposta l'actor? 

**Opzioni:**
- **(a)** Wizard/Settings VSIX: `ibis.identity.actor` → header injectato dai
  comandi + documentato per i client CLI.
- **(b)** Solo env var `IBIS_ACTOR` documentata (zero UI).
- **(c)** Auth vera (token per utente) — viola i guardrails (nessun ACL), fuori
  scope.

**Raccomandazione bozza:** (a) con fallback (b): l'actor viene iniettato in
`buildSpawnEnv`/attach URL query? No — header solo per HTTP: la VSIX in attach
mode passa l'header `X-Ibis-Actor` in `callMcpToolHttp` (estensione
`postJsonRpc`). Personal reads restano "trusted actor" come da spec.

### T3. Conflitti e consolidamento multi-client

Due client attaccati allo stesso server: `memory_consolidate` gira dove? Se
ogni client lancia il loop, doppi supersede.

**Opzioni:**
- **(a)** Consolidamento solo su richiesta esplicita (`ibis memory consolidate`
  manuale, cron dell'automazione plugin su UNA macchina).
- **(b)** Lease nel DB (row `consolidation_lease` con TTL) — solo il lease
  holder esegue. Robusto ma nuovo stato.
- **(c)** Il daemon `ibis run` esegue il loop di default, i client mai.

**Raccomandazione bozza:** (a) per la 1.1.0 (stato attuale: nessun loop
automatico client-side), (c) quando T1(b) esiste — il daemon diventa il posto
naturale; (b) se emergeranno più daemon sullo stesso DB.

### T4. Reconciliation Layer 3 (sidecar stale)

Lo studio sidecar l'ha promossa a layer strutturale: PID-file nel data dir +
liveness + start-time check, per coprire shim uccisi/macchine senza kernel
kill. Con il team server (processo long-lived) il rischio orfano cresce.

**Opzioni:**
- **(a)** Implementarla ora (punto 6 del piano sidecar, già studiato).
- **(b)** Differire al momento del team server deploy effettivo.

**Raccomandazione bozza:** (a) nella stessa fase — è la precondizione per
fidarsi del daemon (T1) e chiude l'ultimo layer del piano sidecar.

### T5. Superficie di condivisione: git claims + proiezione

Il claim store git rende le memorie durevoli PR-reviewable (`.ibis/claims/`),
la proiezione `.ibis/memory` è committabile per rule/decision/fix/constraint/
design/gotcha. Su team server il default `db_only` evita file orfani su
clone non-owned.

**Domanda aperta:** il wizard/Settings deve esporre il knob `personal-claims`
(`stub` vs `db_only`) già specificato (4c del claim-store addendum) o resta
env-only? **Raccomandazione bozza:** Settings knob in 1.1.0 insieme a T2
(sono lo stesso pannello "Team").

### T6. Binding di rete e degradation

Il daemon HTTP oggi è loopback. Per team server serve binding LAN esplicito
(`--bind 0.0.0.0` o setting) + consapevolezza che non c'è auth (trust =
rete). Documentare nel runbook T1(a); MCP registry `server.json` resta
loopback per il caso personale.

---

## 3. Cross-cutting

- **Docs (F3)**: la fase docs/posizionamento (PLAN §8) deve assorbire le
  decisioni di questo brainstorm (matrice integrazioni + "Ibis vs Copilot
  Memory" citano il team server). Sequencing: brainstorm → spec → F3.
- **D-CLAIMS (Track D, differita)**: il knob personal-claims e il runbook team
  server sono input naturali per il benchmark quando si riprenderà.
- **Versioning**: le decisioni qui alimentano la fase successiva nel PLAN
  (runtime `0.3.0` con T-Work; VSIX `1.1.0` con A-Work). R3/R4 publish della
  1.0.0 resta sganciato e non aspetta questo brainstorm.

## 4. Domande per la sessione owner

1. A1 filtro status: default `all` o `current`?
2. A2 grouping: seconda vista workstream (b) o si slitta (c)?
3. A3 curation inline: solo Share/Archive (b)?
4. A5: accettare i due child in spawn mode per la 1.1.0?
5. T1: runbook docs (a) prima, `ibis serve --team` (b) nell'iterazione?
6. T2: actor via Settings + header HTTP (a)?
7. T4: reconciliation implementata nella stessa fase del team server?
8. Nome della fase nel PLAN: **F4 — Team server & memory view** (o Track E)?

## 5. Prossimi passi (dopo le decisioni)

1. Spec: `docs/superpowers/specs/2026-11-01-memory-view-team-server-design.md`
   (una spec unica con due milestone A/B, o due spec se gli owner preferiscono
   separare i flussi di lavoro).
2. Nuova fase in [PLAN.md](PLAN.md) con sub-fasi commit-style e gate review
   (proposto: **R7** su team server con 2+ client reali attaccati).
3. Sequencing: `[S3 publish 1.0.0] → F4 A (view) ∥ F4 B (team server) → F3 docs`.

## 6. Decisioni owner (01/11/2026)

Sessione decisionale eseguita sulle 8 domande §4. Dove la decisione diverge
dalla raccomandazione bozza è indicato.

| # | Domanda | Decisione | vs bozza |
|---|---|---|---|
| A1 | Default filtro status | **`current`** — l'albero mostra solo memorie correnti; switch `all` per il curation browse | diverge (bozza: `all`) |
| A2 | Grouping | **Seconda vista "Workstreams"** (2 viste nel container) — richiede estensione server (`memory_list` con filtro workstream + list dei workstream) | conforme |
| A3 | Curation inline | **Menu completo**: Share, Archive, **Supersede…, Update content…** dal tree (conferma esplicita; update solo su memorie proprie — la governance rifiuta update di team altrui e indirizza al supersede) | diverge (bozza: Share/Archive) |
| A4 | Live refresh | non chiesto: **refresh on focus** (opzione (b) bozza); subscription `resources/updated` solo se l'uso reale la chiede | — |
| A5 | Topologia backend | **Daemon sempre-on**: la VSIX rinuncia al provider spawn e attacca un daemon `ibis run` locale — un solo runtime; lifecycle del daemon fuori da VS Code (start/health integrate, senza ownership) | diverge (bozza: status quo) |
| T1 | Deployment team server | **Runbook + `ibis serve --team` + docker-compose ufficiale** (tutte e tre: docs (a), subcommand (b), compose (c)) | diverge (bozza: a+b) |
| T2 | Identity actor | **Settings `ibis.identity.actor` + header `X-Ibis-Actor`** su attach HTTP, fallback env `IBIS_ACTOR`; niente auth/token (guardrails) | conforme |
| T3 | Consolidamento multi-client | non chiesto: **il daemon esegue il loop di default, i client mai** (coerente con A5) | — |
| T4 | Reconciliation Layer 3 | **Implementata nella fase F4** (precondizione del daemon/serve) | conforme |
| T5 | Knob personal-claims | non chiesto: Settings knob insieme a T2 (pannello "Team") | — |
| — | Nome fase | **F4** (dopo F0–F3; Track D resta il benchmark Scope) | conforme |

Effetti di A5 (la decisione più strutturale): la VSIX passa a backend
attach-only verso il daemon personale; il provider MCP contribuito diventa una
definition Streamable HTTP; i due child collassano in **un solo runtime**
(una ingest lane, un batch manager, un consolidatore). La VSIX integra
start/health del daemon (comando "Start Daemon" + probe esistente
`runtime.autoAttach`) **senza** possederne il lifecycle: il daemon sopravvive
a VS Code e si spegne da sé / da gestione owner.