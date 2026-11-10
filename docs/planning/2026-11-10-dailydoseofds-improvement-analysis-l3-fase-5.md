# Analisi funzionale L3 — fase 5 (E16, F17, G19, G20)

**Data:** 2026-11-10
**Predecessori:**
1. [2026-11-01-dailydoseofds-improvement-analysis.md](./2026-11-01-dailydoseofds-improvement-analysis.md) — interventi E16, F17, G19, G20
2. [2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md](./2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md) — §1.5, §2.4, §2.7, §3.5
3. [2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md) — formato e stile delle fasi 0–3
4. [2026-11-09-dailydoseofds-improvement-analysis-l3-fasi-4-5.md](./2026-11-09-dailydoseofds-improvement-analysis-l3-fasi-4-5.md) — fasi 4–5 (C8, E14, E15), il cui stato è il punto di partenza
5. [2026-11-02-master-work-queue.md](./2026-11-02-master-work-queue.md) — WQ-5 (stato)

**Stato:** fasi 0–4 complete (ultimo commit `9c7de2e`). Questo documento specifica la fase 5 limitatamente a E16, F17, G19 e G20. B5 non è più un intervento autonomo (assorbito da DiskANN in WQ-7, come già registrato nella coda).

**Metodo:** ogni affermazione sul flusso attuale è ancorata a codice letto in prima persona al commit `9c7de2e`. I numeri di riga sono verificati a quel commit; il codice resta la fonte di verità. Formato per intervento: **Aggancio → Flusso attuale → Trasformazione (numerata) → Casi limite → Test → Accettazione**.

**Struttura e ordine (per dipendenza, non per numero):**

- **Parte I — E16** (prima): il task store durevole è l'involucro che rende le passate lunghe esplicite, esclusive, limitate e riprendibili. C8 oggi è sincrono e bounded (≤ 10 sintesi/pass): finché la passata resta corta il cap basta, ma è il task store che le permette di crescere (passate lunghe, resume) senza perdere il controllo dei costi. F17 poi lo strumenta.
- **Parte II — F17** (seconda): osserva le stesse passate e le stesse tool call; consuma gli attributi del task (`ibis.task.id`, `ibis.task.recoveries`) senza dipendere da E16 per compilare.
- **Parte III — G19** (terza): indipendente dalle prime due; è la prima superficie utente della memoria fuori dall'IDE.
- **Parte IV — G20** (ultima): consolida il wiring tra host; nessuna dipendenza, rischio più basso, beneficio più indiretto.

---

## 0. Gate comuni (validi per tutte e quattro le parti)

### 0.1 Le quattro porte obbligatorie

| Gate | Contratto | Come lo rispettano E16/F17/G19/G20 |
|---|---|---|
| **Nessun nuovo tool MCP (58)** | `crates/ibis-server/tests/tool_contract_test.rs:13-14` (`required: [&str; 58]`), `:83`; fixture `tests/fixtures/discovery_tools.json` rigenerata solo per cambi deliberati (`discovery_snapshot_test.rs:9-13`). | E16 usa una **risorsa** (`ibis://tasks/{id}`), non un tool, e cambia solo il testo di ritorno di `memory_consolidate`. F17 è config + span. G19 è un comando CLI composto da `memory_export` (tool esistente) + rendering client-side. G20 è config + risoluzione server-side. Nessuno aggiunge una voce a `TOOL_SPECS`. |
| **A1 invariato** | `ibis eval retrieval` su `evaluations/retrieval/dataset.yaml` (21 case): baseline offline MRR 0.952 / r@5 0.976 / r@15 1.0; nessuna modifica a ranking, fusione, espansione o rerank. | E16 non tocca il read path. F17 non cambia una riga di retrieval. G19 legge `memory_export` (WHERE già esistente) e non entra in `ask_project`. G20 non nasconde tool di default (profilo default = comportamento attuale). |
| **Live test su SurrealDB 4.0.0** | Job `live-tests` di `.github/workflows/ci.yml:72-113` con `surrealdb/surrealdb:v4.0.0`; ogni suite live è gated da `IBIS_SPIKE_SURREAL_URL`. | E16 è l'unica parte che aggiunge DDL e protocollo DB: la sua suite live è obbligatoria e copre UNIQUE con `NONE`, reclaim del lease, fencing, prune. F17/G19/G20 non aggiungono query nuove (G19 usa `memory_export`); il loro gate è la suite live esistente verde. |
| **Eval observations** | `ibis eval observations` su `evaluations/observations/dataset.yaml` (8 case, o01–o08), step CI in `retrieval-eval` (`.github/workflows/ci.yml:63`). | E16 non cambia membership né hash: il checkpoint è un involucro, il reconcile è lo stesso codice. Il test di ripresa verifica che un secondo tentativo non spenda una seconda sintesi. F17/G19/G20 non toccano C8. |

### 0.2 Invarianti trasversali

- **Nessuna migrazione**: greenfield 4.0; ogni DDL nuovo è `IF NOT EXISTS` e idempotente al boot (`schema.rs:1-44`). Le tabelle riservate dichiarano i propri campi quando la lane arriva (precedente: `workstream` in F4-A3, `schema.rs:352-358`).
- **Nessun contenuto nei log/telemetria**: i nuovi attributi sono nomi, id, conteggi e stato. Mai titoli, contenuti di memoria, prompt, path assoluti di file utente.
- **Fail-open sulle superfici di servizio**: task store e telemetria non possono far fallire una tool call o una passata. Un errore di bookkeeping è un `warn`, non un `Err`.
- **`cargo fmt`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` verdi** a ogni step.

---

# Parte I — E16: task store durevole per le passate lunghe

---

## 0. Ancoraggi verificati

| Ancoraggio | Fatto verificato |
|---|---|
| `crates/ibis-server/src/tools_memory.rs:949-994` | `memory_consolidate` MCP: spawn **detached** (`tokio::spawn` a `:962`), risponde subito `"Consolidation started for up to N candidate observations."` (`:991-993`). Nessun id, nessuno stato, nessuna guardia contro due chiamate concorrenti. |
| `crates/ibis-server/src/consolidation.rs:78-122` | Pass daemon: `pending_observation_projects` → `consolidate_loop(project, 20)` in sequenza; nessun lease, nessuna scoperta di task interrotti; `maybe_project_memory` solo a passata riuscita (`:118`). |
| `crates/ibis-optimization/src/memory_consolidate.rs:160-315` | `consolidate_loop`: drena oldest-first, un'osservazione per volta, `max_items` default 20; a fine loop chiama `observations_pass` (`:300-313`) se `ai.observations.enabled`. |
| `crates/ibis-optimization/src/memory_consolidate.rs:375-435` | `select_observations`: `ORDER BY created_at ASC LIMIT {max_items}` — nessun cursore, nessuna paginazione stabile. |
| `crates/ibis-optimization/src/memory_consolidate.rs:668-740` | `persist_consolidation`: `add_memory` (transazione propria, `:712-715`) e **poi** una seconda transazione che archivia l'osservazione sorgente (`:726-738`). La finestra fra le due non è atomica. |
| `crates/ibis-optimization/src/observations.rs:396-461` | `observations_pass`: reconcile `Full` per scope, poi backlog di sintesi; cap `max_llm_summaries_per_pass` (`:439`); contatori in `ObservationPassReport` (`:101-110`). |
| `crates/ibis-optimization/src/observations.rs:59` | `SYNTHESIS_BACKLOG_LIMIT = 100` (bounded per scope). |
| `crates/ibis-optimization/src/memory_consolidate.rs:341-373` | `pending_observation_projects`: due rami (raw + derived), dedup in Rust. |
| `crates/ibis-ai/src/batch_manager.rs:26-28` | Precedente di durabilità per riga: `BATCH_SIZE = 100`, `POLL_INTERVAL = 10s`. |
| `crates/ibis-ai/src/batch_manager.rs:328-370` | `start`/`run_loop`: ticker con `MissedTickBehavior::Skip`, shutdown via `watch`, un solo sweep in volo. |
| `crates/ibis-ai/src/batch_manager.rs:71-79` | `CommitTxn`: stato terminale per riga (`completed`/`failed` + `batch_error`) — il pattern "stato sul dato", non un task store. |
| `crates/ibis-db/src/schema.rs:116` | `DEFINE TYPE batch_status AS 'pending' \| 'completed' \| 'failed'`. |
| `crates/ibis-db/src/schema.rs:203-204`, `:451` | Tabella `batch_job` con il solo `status` e indice `job_status`: placeholder senza writer. |
| `crates/ibis-db/src/schema.rs:211-213` | Precedente `UNIQUE` + `NONE`: `log_file.path` è opzionale perché "UNIQUE ignores NONE". |
| `crates/ibis-db/src/schema.rs:326-342` | Tabelle cache (`embedding_cache`, `llm_response_cache`) con TTL/prune: precedente di retention. |
| `crates/ibis-server/src/state.rs:74-107` | `AppState`: `db`, `search`, `ai`, `resource_subs`, `tool_discovery` — il posto dove vive l'handle del task runner. |
| `crates/ibis-server/src/state.rs:288-307` | `maybe_project_memory`: proiezione solo se `memory.projection_enabled` e root risolta. |
| `crates/ibis-server/src/main.rs:681-708` | Background workers + sequenza di shutdown (token → stop AI → batch manager → ingestion → DB). |
| `crates/ibis-server/src/resources.rs:50-120` | URI `ibis://memory\|rules\|ticket` + `parse_uri`; `:125-155` template table; `:163-268` `read_parsed`. |
| `crates/ibis-server/src/resources.rs:269-395` | Registry sottoscrizioni + `fire(uri)`: notifica `resources/updated` già disponibile. |
| `crates/ibis-server/src/registry.rs:366-373` | Spec `memory_consolidate`: `project_name`, `max_items`. |
| `crates/ibis-server/src/registry.rs:734-1035` | `TOOL_ANNOTATIONS`: tabella statica con `read_only` per ogni tool (riusata da F17). |
| `crates/ibis-server/src/tools.rs:114-131` | `dispatch`: `actor_context`, log `MCP Tool Call: {name}`, poi match sul nome. |
| `crates/ibis-server/src/mcp_server.rs:216-260` | `call_tool`: `tool_visible` gate, `catch_tool_panic`, durata misurata (`t0.elapsed()`), log `MCP Tool Done`. |
| `crates/ibis-server/src/mcp_server.rs:181-201` | `list_tools`: filtro `ticket_*` via `ticketing_configured`. |
| `crates/ibis-core/src/config.rs:237-260` | `ConsolidationConfig` (`enabled`, `interval_secs`, `dup_similarity_min`). |
| `crates/ibis-core/src/config.rs:523-612` | `Config` root: punto in cui aggiungere il blocco `tasks`. |
| `crates/ibis-core/src/sidecar_ledger.rs` | Precedente di identità di processo (pid + start-time) per distinguere istanze vive. |
| `crates/ibis-server/tests/tool_contract_test.rs:13-14` | 58 tool. |

---

## 1. Sintesi delle decisioni (le 8 domande chiuse)

| # | Domanda | Decisione |
|---|---|---|
| 1 | Tesi | La ripresa **esiste già implicitamente** (il backlog è il cursore: un'osservazione processata esce da `select_observations`). E16 non inventa la ripresa: la rende **esplicita, esclusiva, limitata e osservabile**. Un task è l'involucro durevole di una passata. |
| 2 | Tabella | Nuova tabella `task` (`SCHEMAFULL`), non `batch_job`: `batch_job` resta il placeholder delle lane di ingestion batch (ha solo `status` + indice `job_status`). Un task ha lifecycle, lease, checkpoint e params: è un concetto diverso. Costo: 1 tabella in un greenfield 4.0, zero migrazioni. |
| 3 | Identità | `task:⟨<32 hex>⟩` da UUID v4 (un task è un evento, non un singleton). L'unicità dell'attivo è garantita da `active_key` (`"{kind}:{project}"`, `NONE` nei terminali) con indice **UNIQUE**: precedente provato `log_file.path` (`schema.rs:211-213`). |
| 4 | Lease | Claim atomico con `UPDATE … WHERE status='queued' OR (status='running' AND lease_expires_at < time::now())`, `lease_epoch` incrementale e **fencing** su heartbeat/finish. Heartbeat in un task tokio indipendente (ogni 10s, lease 30s), non nel loop della passata: una chiamata LLM da 60s non fa scadere il lease. |
| 5 | Ripresa | La passata riparte dal backlog (verità di store). Il checkpoint (`last_item_id`, `items_done`, `summaries_used`, `scopes_done`) è **osservabilità + ledger di budget**, non correttezza. `max_recoveries` (default 3) limita i giri su un progetto avvelenato. |
| 6 | Budget | `summaries_used` persistito **a fine scope** (≤ 8 UPDATE/passata): un crash a metà passata non azzera il cap di 10 sintesi; la perdita massima è un scope. Il cap resta per task, non per tentativo. |
| 7 | Superficie | Nessun tool nuovo: `memory_consolidate` guadagna solo il task id nel testo di ritorno; lettura via risorsa `ibis://tasks/{id}` e `ibis://tasks` (attivi), con `notifications/resources/updated` sui cambi di stato. Log strutturati `task_*`. |
| 8 | Approvazioni con expiry | **Fuori dalla v1**, con motivazione esplicita (§4.8): nessun client negozia oggi un consenso umano, e una coda di approvazioni senza approvatore o blocca le mutazioni per sempre o si auto-approva. Il task store è il prerequisito (la richiesta di approvazione è un task con expiry), non il consumatore. |

---

## 2. Aggancio

- **Tool**: `memory_consolidate` (`tools_memory.rs:949-994`), spec `registry.rs:366-373`.
- **Loop daemon**: `consolidation::run_pass` (`consolidation.rs:78-122`), avviato da `main.rs:690`.
- **Passata**: `Optimizer::consolidate_loop` (`memory_consolidate.rs:160-315`) e `observations_pass` (`observations.rs:396-461`).
- **Stato durevole esistente**: `batch_status` per riga (`schema.rs:116`, `batch_manager.rs:71-79`), backlog osservazioni (`memory_consolidate.rs:341-373`), offset tailer (`log_file.offset`), ledger sidecar (`ibis-core::sidecar_ledger`).
- **Superfici**: `AppState` (`state.rs:74-107`), risorse (`resources.rs`), notifiche (`resources.rs:344-395`), shutdown (`main.rs:700-726`).
- **Config**: `Config` root (`config.rs:523-612`), accanto a `search`/`memory`.

## 3. Flusso attuale

1. `memory_consolidate` valida `project_name`/`max_items`, fa `tokio::spawn` e risponde **prima** che la passata sia iniziata (`tools_memory.rs:960-993`). Se il processo muore, la chiamata è già stata confermata ma nessuno sa che la passata non è finita.
2. Il loop daemon scopre i progetti con backlog e chiama `consolidate_loop` in sequenza (`consolidation.rs:98-121`). Non esiste alcuna nozione di "passata in corso": due processi sullo stesso store (due daemon, o daemon + tool) possono drenare lo stesso progetto in parallelo. Il residuo è già registrato in F4-B3 ("lease in-process candidato follow-up").
3. `consolidate_loop` seleziona fino a 20 osservazioni con `ORDER BY created_at ASC` (`memory_consolidate.rs:400-402`), le processa una a una, e per ciascuna: duplicate lookup (C9) → distill (LLM) → `add_memory` → seconda transazione che archivia la sorgente (`:668-740`).
4. La ripresa è implicita: un'osservazione archiviata non è più `status='current'`, quindi non rientra nel backlog. Ma la finestra fra `add_memory` e l'archiviazione (`:712` vs `:726`) non è atomica: un crash lì dentro rigioca l'item al tentativo successivo.
5. `observations_pass` riconcilia `Full` per scope e sintetizza il backlog derivato, con cap 10 per passata (`observations.rs:439`). Il cap è **per invocazione**: un crash-loop che riavvia la passata può spendere 10 sintesi a ogni giro.
6. Non esiste alcuna superficie che risponda a "che cosa sta girando, da quanto, quante volte è fallito, a che punto è". Solo log.
7. `AppState` non ha un handle di task: chi vuole una passata durevole non ha dove metterla.

**Gap funzionale:** la passata è già idempotente per item (grazie allo stato del backlog), ma non è **esclusiva** (nessun lease), non è **osservabile** (nessun record), non è **limitata nel tempo** (nessun `max_recoveries`) e il suo budget non sopravvive a un crash. Sono quattro proprietà distinte, e il task store le copre tutte con una riga per passata.

## 4. Trasformazione

### 4.1 Modello e schema

Nuova tabella `task`, dichiarata accanto alle altre tabelle nodo (`schema.rs:203`), con il tipo di dominio per lo stato (regola `DEFINE TYPE`, `schema.rs:11-13`):

```sql
DEFINE TYPE IF NOT EXISTS task_status AS 'queued' | 'running' | 'completed' | 'failed';

DEFINE TABLE IF NOT EXISTS task SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS kind ON TABLE task TYPE string;
DEFINE FIELD IF NOT EXISTS status ON TABLE task TYPE task_status DEFAULT 'queued';
DEFINE FIELD IF NOT EXISTS project ON TABLE task TYPE option<string>;
DEFINE FIELD IF NOT EXISTS params ON TABLE task TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS checkpoint ON TABLE task TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS active_key ON TABLE task TYPE option<string>;
DEFINE FIELD IF NOT EXISTS lease_owner ON TABLE task TYPE option<string>;
DEFINE FIELD IF NOT EXISTS lease_epoch ON TABLE task TYPE int DEFAULT 0;
DEFINE FIELD IF NOT EXISTS lease_expires_at ON TABLE task TYPE option<datetime>;
DEFINE FIELD IF NOT EXISTS claims ON TABLE task TYPE int DEFAULT 0;
DEFINE FIELD IF NOT EXISTS recoveries ON TABLE task TYPE int DEFAULT 0;
DEFINE FIELD IF NOT EXISTS error ON TABLE task TYPE option<string>;
DEFINE FIELD IF NOT EXISTS requested_by ON TABLE task TYPE option<string>;
DEFINE FIELD IF NOT EXISTS created_at ON TABLE task TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS updated_at ON TABLE task TYPE datetime DEFAULT time::now();
DEFINE FIELD IF NOT EXISTS claimed_at ON TABLE task TYPE option<datetime>;
DEFINE FIELD IF NOT EXISTS finished_at ON TABLE task TYPE option<datetime>;

DEFINE INDEX IF NOT EXISTS task_active_key ON TABLE task COLUMNS active_key UNIQUE;
DEFINE INDEX IF NOT EXISTS task_status_kind ON TABLE task COLUMNS status, kind;
DEFINE INDEX IF NOT EXISTS task_project ON TABLE task COLUMNS project, kind;
DEFINE INDEX IF NOT EXISTS task_finished_at ON TABLE task COLUMNS finished_at;
```

Perché questi campi e non altri:

| Campo | Ruolo | Perché non altrove |
|---|---|---|
| `kind` | `memory_consolidate` in v1; whitelist Rust (string, non union: aggiungere un kind non deve richiedere un `REMOVE`/`DEFINE` di tipo). | Un tipo a un solo valore sarebbe un falso dominio. |
| `active_key` | `"{kind}:{project_lower}"` mentre queued/running, `NONE` nei terminali. | È la guardia di unicità **cross-process**, più forte del lease in-process ipotizzato in F4-B3. |
| `params` | Snapshot dei parametri della passata (`max_items`, `max_llm_summaries`, `mode`). | La ripresa deve usare i parametri con cui il task è nato, non la config viva. |
| `checkpoint` | `last_item_id`, `items_done`, `items_total`, `summaries_used`, `scopes_done`, `scopes_total`. | Osservabilità e ledger di budget (§4.3); non è la verità di ripresa. |
| `lease_owner` + `lease_epoch` | Identità dell'istanza che tiene il lease + fencing token. | `lease_owner` da solo non basta: dopo un reclaim il vecchio owner deve fallire l'heartbeat. |
| `claims` / `recoveries` | Claim totali / reclaim dopo lease scaduto. | Un riavvio pulito non deve consumare i tentativi: `recoveries` conta solo i crash, `max_recoveries` si applica a quello. |
| `error` | Ultimo errore, troncato a 2k. | Osservabilità; non è un log (il log resta `tracing`). |

**Perché non `batch_job`:** la tabella esiste con il solo `status` e un indice `job_status` (`schema.rs:203-204`, `:451`), ma è il posto giusto per un job di ingestion batch (righe da processare in blocco, stato per riga). Un task ha lease, epoch, checkpoint e params: sovraccaricarla renderebbe illeggibile sia il job sia il task. La convenzione "tabelle senza writer dichiarano i campi quando la lane arriva" (`schema.rs:34-36`, precedente `workstream` in `:352-358`) vale per entrambe: `batch_job` resta libera per la sua lane.

### 4.2 Protocollo claim / lease / heartbeat (fencing)

Tutte le mutazioni sono statement singoli (atomici) con valori bindati; `$now` non serve (si usa `time::now()`).

**Create** (dal tool o dal loop, prima di qualunque lavoro):

```sql
CREATE task:⟨<uuid32⟩⟩ SET
  kind = 'memory_consolidate',
  status = 'queued',
  project = $project,
  params = { max_items: $max_items, max_llm_summaries: $max_summaries, mode: $mode },
  active_key = $active_key,
  requested_by = $actor;
```

Se l'`active_key` esiste già, il `CREATE` viola l'indice UNIQUE: il chiamante legge `SELECT id FROM task WHERE active_key = $active_key` e restituisce l'id esistente (nessuno spawn, nessuna passata). Il runner fa un `SELECT` rapido **prima** del `CREATE` (fast path) e, se il `CREATE` fallisce per UNIQUE, rilegge l'attivo: la corsa è risolta dal vincolo, non dal pre-check. `requested_by` è l'attore della tool call o `ibis.consolidation_loop` per il pass daemon.

**Claim** (una sola statement; il vincitore è chi riceve righe):

```sql
UPDATE task:⟨<key⟩⟩ SET
  status = 'running',
  lease_owner = $owner,
  lease_epoch = lease_epoch + 1,
  lease_expires_at = time::now() + {lease_secs}s,
  claims = claims + 1,
  claimed_at = time::now(),
  updated_at = time::now()
WHERE status = 'queued'
   OR (status = 'running' AND lease_expires_at < time::now())
RETURN AFTER;
```

`{lease_secs}` è un intero clampato in config (non contenuto utente) interpolato come duration literal — stesso precedente di `NOISE_RETENTION` (`memory_consolidate.rs:757`). Zero righe → il task non è reclamabile (già running con lease vivo, o terminale): il chiamante logga `task_claim_skipped` e prosegue.

**Heartbeat** (task tokio dedicato, ogni `heartbeat_secs`):

```sql
UPDATE task:⟨<key⟩⟩ SET
  lease_expires_at = time::now() + {lease_secs}s,
  updated_at = time::now()
WHERE lease_owner = $owner
  AND lease_epoch = $epoch
  AND status = 'running'
RETURN AFTER;
```

Zero righe → lease perso: si alza un `AtomicBool` (`lease_lost`) e la passata si ferma al confine dell'item successivo. Il fencing è esattamente `lease_epoch`: dopo un reclaim l'epoch è cambiato e il vecchio owner non può più scrivere né fare heartbeat.

**Checkpoint** (dopo ogni item e a fine scope, con lo stesso fencing):

```sql
UPDATE task:⟨<key⟩⟩ SET
  checkpoint = {
    last_item_id: $last_item_id,
    items_done: $items_done,
    items_total: $items_total,
    summaries_used: $summaries_used,
    scopes_done: $scopes_done,
    scopes_total: $scopes_total
  },
  updated_at = time::now()
WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running';
```

**Finish** (una sola statement, libera lo slot):

```sql
UPDATE task:⟨<key⟩⟩ SET
  status = $terminal,              -- 'completed' | 'failed'
  finished_at = time::now(),
  updated_at = time::now(),
  active_key = NONE,
  error = $error,
  checkpoint = $checkpoint
WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running';
```

**Riposo pulito** (shutdown del daemon o `ibis stop`): il runner, al confine dell'item, scrive:

```sql
UPDATE task:⟨<key⟩⟩ SET
  status = 'queued',
  lease_owner = NONE,
  lease_expires_at = NONE,
  updated_at = time::now()
WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running';
```

`active_key` resta valorizzata (il task è ancora attivo): nessuna scrittura, nessuna riapertura dello slot UNIQUE.

Il task resta `queued` e viene ripreso al boot successivo **senza** consumare una recovery. È il comportamento che rende `ibis stop` veloce anche con una passata lunga in corso.

**Recovery al boot** (prima del primo tick del loop):

```sql
-- 1) lease scaduto, tentativi residui: torna in coda
UPDATE task SET status = 'queued', lease_owner = NONE, lease_expires_at = NONE,
                recoveries = recoveries + 1, updated_at = time::now()
WHERE status = 'running' AND lease_expires_at < time::now() AND recoveries < $max_recoveries;

-- 2) lease scaduto, tentativi esauriti: fallisce e libera lo slot
UPDATE task SET status = 'failed', error = 'lease_expired', finished_at = time::now(),
                active_key = NONE, recoveries = recoveries + 1, updated_at = time::now()
WHERE status = 'running' AND lease_expires_at < time::now() AND recoveries >= $max_recoveries;
```

Poi il loop esegue prima i `queued` (con i loro params) e solo dopo la discovery di nuovi progetti. Un task `queued` creato da un tool e mai partito (crash immediato) viene ripreso qui.

**Perché il lease è un task tokio separato:** una chiamata LLM locale può durare più del lease (timeout provider 120s di default). Se l'heartbeat vivesse nel loop della passata, il lease scadrebbe a metà chiamata e un secondo processo la duplicherebbe. Con un heartbeat indipendente il lease resta vivo per tutta la durata della chiamata; il loop controlla `lease_lost` solo ai confini degli item.

### 4.3 Checkpoint, ripresa e budget

**Verità di ripresa = backlog.** `select_observations` è già la query che definisce il lavoro (`memory_consolidate.rs:400-402`): un'osservazione archiviata non rientra. Lo stesso vale per `observations_pass`: una riga `generated_by=llm` con embedding non è più nel backlog (`observations.rs:481-491`). Il checkpoint **non sostituisce** questa verità, la rende leggibile:

- `items_total` è noto dopo `select_observations` (o dopo il reconcile, per gli scope) e viene scritto subito;
- `items_done` e `last_item_id` avanzano dopo ogni persist riuscito;
- `summaries_used` avanza **a fine scope** (una UPDATE per scope, ≤ 8 per passata): il cap residuo è `max(0, cap − used)`, passato a `observations_pass` come `remaining_summaries`. Un crash a metà scope può perdere al più le sintesi di quello scope (≤ 10), non l'intero cap.

**Interfaccia**: `consolidate_loop` guadagna un osservatore opzionale (seam, come `ChunkStore` in `ibis-ai`):

```rust
pub trait PassCheckpoint: Send + Sync {
    /// false = lease perso o shutdown: la passata si ferma al confine dell'item.
    fn should_continue(&self) -> bool;
    /// dopo il persist riuscito di un item (`ok` | `noise` | `error`).
    fn item_done(&self, item_id: &str, outcome: &str, items_total: usize);
    /// dopo reconcile + sintesi di uno scope.
    fn scope_done(&self, report: &ObservationPassReport);
    /// sintesi residue per il task (`None` = usa il cap della config).
    fn remaining_summaries(&self) -> Option<usize>;
}
```

Firma: `consolidate_loop(&self, project_name, max_items, checkpoint: Option<&dyn PassCheckpoint>)`. I test esistenti passano `None` (comportamento identico). `memory_consolidate` (wrapper) propaga `None`. Lo stesso `checkpoint` scende a `observations_pass` (nuova firma `observations_pass(project_name, checkpoint)`), che usa `remaining_summaries()` per il cap residuo e chiama `scope_done()` dopo ogni scope.

**Finestra non atomica nota.** `persist_consolidation` fa `add_memory` e poi archivia la sorgente in due transazioni (`memory_consolidate.rs:712-738`). Un crash fra le due rigioca l'item: C9 lo riconosce come merge (titolo normalizzato identico) e crea una nuova riga che supersede la precedente, invece di duplicare il `current`. Il danno è una catena di supersessioni, **limitata da `max_recoveries`**. Chiudere la finestra richiede di spostare l'archiviazione nella transazione di `add_memory`: è un follow-up dichiarato (§Fuori scope), non un requisito di E16.

### 4.4 Ciclo di vita

```text
            create                 claim                    finish
  (nessuno) ──────► queued ──────────────► running ──────────────► completed
                      ▲                      │  │
                      │  recovery (crash)    │  │ shutdown pulito
                      └──────────────────────┘  └──────────────► queued
                                             │
                                             └── recoveries >= max ──► failed
```

- Terminali (`completed`, `failed`) immutabili: nessuna transizione in uscita, `active_key = NONE`.
- `completed` copre anche le passate con item falliti: gli errori item-level restano nel log e in `checkpoint.items_done < items_total`; `failed` è riservato agli errori di passata (DB/AI assente, discovery/reconcile fallito) e al superamento di `max_recoveries`, dove non ha senso riprovare gli stessi item subito.
- `queued` è l'unico stato reclamabile senza lease scaduto.
- `running` è reclamabile solo con `lease_expires_at < time::now()`.
- `failed` è terminale: un nuovo tentativo è un **nuovo** task (nuovo id), così la storia resta leggibile.

### 4.5 Superfici: tool, risorsa, log (niente nuovo tool)

**Tool `memory_consolidate`** — stessa spec (`project_name`, `max_items`), nessun parametro nuovo. Cambia solo il testo di ritorno:

- task creato: `"Consolidation started for up to N candidate observations (task:⟨…⟩)."`
- task `queued` da un riavvio precedente: il chiamante lo **claima e lo riprende** (stessi params), risposta `"Resumed consolidation for <project> (task:⟨…⟩, attempt 2)."`
- task `running` con lease vivo: nessun claim, risposta `"A consolidation pass is already running for <project> (task:⟨…⟩, status running, attempt 2)."`
- DB assente: errore actionable (`"database not connected"`) **prima** dello spawn, invece di un successo con l'errore solo nei log. È un miglioramento di correttezza, non un cambio di contratto.
- La descrizione del tool guadagna una frase sulla lease: questo è l'unico cambio che richiede la rigenerazione della fixture discovery (D15 lockstep, `discovery_snapshot_test.rs:9-13`).

**Risorsa `ibis://tasks/{id}`** (lettura di un task) e **`ibis://tasks`** (attivi, `status IN ['queued','running']`, ordinati per `created_at`), MIME `application/json`. Sono risorse, non tool: il contratto resta 58. `parse_uri` guadagna il ramo `tasks` (`resources.rs:86-120`), `resource_templates()` la riga nuova (`:125-155`), `read_parsed` il decode (`:163-268`). Il test `templates_table_has_the_three_f04_rows` diventa quattro template (i tre F0.4 + `ibis://tasks/{id}`): aggiornamento deliberato e documentato.

**Notifiche**: `AppState.resource_subs.fire("ibis://tasks")` sui cambi di stato (create, claim, finish), **non** a ogni checkpoint (troppo rumore). `ibis://tasks/{id}` viene fireato solo se un client vi è sottoscritto (il registry è già per-URI).

**Log** (strutturati, cercabili): `task_created`, `task_claimed`, `task_claim_skipped`, `task_heartbeat_lost`, `task_checkpoint`, `task_completed`, `task_failed`, `task_requeued`, `task_recovered`, `task_pruned`.

### 4.6 Fail-open e degradazione

- Se il task store non è disponibile (DB assente), il tool risponde con errore onesto **prima** di spawnare: senza DB non c'è passata, quindi non c'è nulla da salvare. Questo non è fail-open, è coerenza.
- Se il task store fallisce **dopo** l'avvio (es. schema mancante su un DB pre-E16, errore transitorio), il runner logga `warn` e prosegue **senza lease**: la passata gira come oggi. Il bookkeeping non deve mai bloccare la manutenzione della memoria.
- Se l'`active_key` UNIQUE non si comportasse come `log_file.path` su 4.0 (due `NONE` che collidono), il piano B è un sentinel terminale unico (`"done:{id}"`) al posto di `NONE`. Il probe live (§Test) decide: la specifica non presume il comportamento dell'indice.
- `memory_consolidate` chiamato da un client stdio/plugin: il task vive nel DB di quel runtime (plugin isolato compreso). Nessuna condivisione fra runtime isolati.

### 4.7 Configurazione e retention

Nuovo blocco root in `Config` (`config.rs:523-612`), accanto a `search`/`memory`:

```json
"tasks": {
  "lease_secs": 30,
  "heartbeat_secs": 10,
  "max_recoveries": 3,
  "retention_days": 30
}
```

| Chiave | Default | Clamp | Semantica |
|---|---|---|---|
| `lease_secs` | 30 | `[10, 600]` | Durata del lease; un processo morto è reclamabile dopo. |
| `heartbeat_secs` | 10 | `[1, lease_secs/2]` | Cadenza dell'heartbeat; il clamp garantisce almeno due battiti per lease. |
| `max_recoveries` | 3 | `[1, 10]` | Reclaim dopo crash prima di dichiarare `failed`. |
| `retention_days` | 30 | `[1, 365]` | Prune dei terminali al boot. |

Nessun `enabled`: la durabilità non è un feature toggle. Le uniche righe scritte sono quelle delle passate reali (tool o loop opt-in), e il fail-open di §4.6 copre l'eventuale costo.

**Prune** al boot, dopo la recovery:

```sql
DELETE task WHERE status IN ['completed', 'failed']
  AND finished_at < time::now() - {retention_days}d
RETURN BEFORE;
```

`RETURN BEFORE` e non `RETURN AFTER`: su 4.0 `DELETE … RETURN AFTER` rende `[[NONE]]` (finding live B6, WQ-5). Il conteggio loggato è la lunghezza della lista restituita.

### 4.8 Approvazioni con expiry: perché fuori dalla v1

L1 include "approval con expiry per le mutazioni in server mode". Non entra nella v1 per tre ragioni verificate:

1. **Nessun approvatore**: il daemon è per definizione non presidiato (`runtime_modes.md`); una coda di approvazioni senza UI di consenso o blocca le mutazioni per sempre o scade e le rifiuta — entrambe peggiori del comportamento attuale, che è "chi raggiunge la porta scrive" (trust boundary di rete, `team_server.md`).
2. **Nessuna capability negoziata**: l'approvazione umana richiederebbe `elicitation` o un'app; oggi nessun client del prodotto la negozia in modo affidabile. Il task store è il prerequisito giusto (una richiesta di approvazione è un task con `expires_at` e `requested_by`), non il consumatore.
3. **Semantica non decisa**: quali mutazioni? `memory_add` di un agente è il caso d'uso primario del prodotto; approvarla una per una annullerebbe il valore. La decisione (allowlist per tool/attore/visibilità) è di prodotto, non di specifica tecnica.

Il campo `requested_by` e lo stato `queued` sono già la base per aggiungerla senza cambiare schema (basterà un `kind='approval'` e un `expires_at`), quando esisterà un client che la consuma.

---

## 5. Casi limite

| Caso | Comportamento |
|---|---|
| Due `memory_consolidate` sullo stesso progetto | Il secondo `CREATE` viola `active_key` UNIQUE; risponde con l'id del task attivo, nessuno spawn. |
| Due daemon sullo stesso store | Il claim è atomico: uno solo ottiene `RETURN AFTER` non vuoto; l'altro logga `task_claim_skipped`. |
| Crash a metà item | La recovery riporta il task `queued` (o `failed` a `max_recoveries`); l'item non archiviato rientra nel backlog. |
| Crash fra `add_memory` e l'archiviazione | L'item viene riprocessato: C9 riconosce il merge per titolo; la catena di supersessioni è limitata da `max_recoveries`. |
| `ibis stop` durante una passata | Il runner requeuea al confine dell'item; il task riprende al boot senza consumare una recovery. |
| Lease scaduto ma processo ancora vivo (GC pause, macchina sospesa) | Il vecchio owner fallisce l'heartbeat (epoch cambiato), si ferma al confine e non scrive più. Il nuovo owner ha già il lease. |
| DB degradato dopo l'avvio | Warn e passata senza lease (fail-open, §4.6). |
| Schema `task` assente (DB creato prima di E16) | Il DDL `IF NOT EXISTS` gira a ogni boot: la tabella nasce al primo avvio post-E16. Nessuna migrazione. |
| `active_key` UNIQUE e `NONE` non si comportano come atteso | Probe live; piano B con sentinel `done:{id}`. |
| Progetto rinominato con un task attivo | Il task resta legato al nome vecchio; la discovery nuova crea un task per il nome nuovo. Il vecchio scade per lease o completa a vuoto. |
| `max_items` diverso durante un task attivo | Il valore del chiamante è ignorato; vale lo snapshot in `params`. |
| Passata con `ai.observations.enabled=false` | La fase C8 no-op; il task completa con `summaries_used=0`. |
| Task terminale letto dalla risorsa | 200 con il record; id inesistente → errore di risorsa non trovata (stesso pattern `read_parsed`). |
| Retention a 0 o negativa | Clamp a 1 giorno + warn. |
| Clock del DB vs clock del processo | Il lease usa solo `time::now()` del DB (claim, heartbeat, recovery): nessun confronto fra clock locali. |

---

## 6. Test

**Unit — modello puro (`ibis-optimization/src/tasks.rs`)**

| Test | Asserzione |
|---|---|
| `active_key_is_normalized` | `("memory_consolidate", " App ")` → `"memory_consolidate:app"`; vuoto/whitespace rifiutato. |
| `task_id_is_32_hex` | `task:⟨<32 hex⟩⟩`, nessun carattere non sicuro (`is_safe_record_id`). |
| `claim_sql_shape` | Contiene `status = 'queued'`, il ramo `lease_expires_at < time::now()`, `lease_epoch = lease_epoch + 1`, `RETURN AFTER`. |
| `heartbeat_sql_shape` | Fencing su `lease_owner` + `lease_epoch` + `status='running'`. |
| `finish_sql_clears_active_key` | `active_key = NONE`, `finished_at`, `error`. |
| `recovery_matrix` | `(running, scaduto, recoveries<max)` → requeue; `(running, scaduto, ≥max)` → failed; `(running, vivo)` → skip; `(queued)` → skip. |
| `terminal_states_are_immutable` | Nessuna transizione da `completed`/`failed`. |
| `remaining_summaries_saturates` | `cap - used`, mai negativo. |
| `checkpoint_defaults` | Oggetto completo anche quando i campi non sono ancora noti. |
| `clamp_matrix` | `lease_secs`, `heartbeat_secs`, `max_recoveries`, `retention_days` ai bordi. |

**Unit — runner con fake store (`ibis-server/src/tasks.rs`)**

| Test | Asserzione |
|---|---|
| `tool_creates_and_claims_one_task` | 1 create + 1 claim; il testo di ritorno contiene l'id. |
| `second_call_returns_active_task_without_spawning` | Nessun secondo claim; messaggio "already running". |
| `pass_completion_marks_completed_and_refreshes_projection` | Finish `completed` + `maybe_project_memory` una volta. |
| `pass_failure_marks_failed_with_bounded_error` | `failed`, `error` ≤ 2k, nessun panico. |
| `lease_lost_stops_between_items` | `should_continue()` false dopo la perdita; gli item successivi non partono. |
| `shutdown_requeues_without_counting_a_recovery` | `status=queued`, `recoveries` invariato. |
| `store_error_is_fail_open` | Store in errore → passata eseguita, warn una volta. |
| `checkpoint_persists_per_item_and_scope` | Conteggio UPDATE atteso (≤ 20 item + ≤ 8 scope). |
| `remaining_summaries_passed_to_observations` | Secondo tentativo con `used=10` → 0 sintesi. |

**Live (SurrealDB 4.0.0)**

| Test | Asserzione |
|---|---|
| `unique_active_key_blocks_second_active_task` | Secondo create → errore UNIQUE; due terminali con `NONE` **coesistono** (probe del comportamento dell'indice). |
| `expired_lease_is_reclaimed_by_another_owner` | Owner B claima dopo la scadenza; `lease_epoch` +1; heartbeat di A → 0 righe. |
| `checkpoint_survives_reclaim` | Il checkpoint scritto da A è letto da B. |
| `recovery_requeues_then_fails_after_max_recoveries` | Sequenza di 3 reclaim → `failed`, `active_key=NONE`. |
| `shutdown_requeue_resumes_on_boot` | `queued` ripreso senza nuova recovery. |
| `prune_terminal_tasks_by_retention` | `RETURN BEFORE` restituisce le righe eliminate; i terminali recenti restano. |
| `task_resources_read_rows` | `ibis://tasks` e `ibis://tasks/{id}` restituiscono JSON coerente. |
| `resource_updated_fires_on_state_change` | Sottoscrizione a `ibis://tasks` → 1 notifica su create/claim/finish, 0 sui checkpoint. |

**Eval**

| Gate | Asserzione |
|---|---|
| `observations_eval_unchanged` | `ibis eval observations`: 8/8, hash invariati. |
| `retrieval_eval_no_regression` | 21/21, MRR ≥ 0.942 / r@5 ≥ 0.966 (baseline − 0.01). |
| `resume_spends_no_second_summary` | Fake AI con contatore: crash simulato dopo lo scope 1 → ripresa → 0 chiamate sul già sintetizzato. |

**Contract**

| Test | Asserzione |
|---|---|
| `tool_contract_test` | 58 tool; nessun nome nuovo. |
| `discovery_snapshot_test` | Fixture rigenerata **solo** per la descrizione di `memory_consolidate`. |
| `resources_protocol_test` | Template `ibis://tasks/{id}` presente; il test "three F0.4 rows" aggiornato a quattro template. |

---

## 7. Accettazione

1. **Esclusività**: due chiamate concorrenti (stesso processo o processi diversi) sullo stesso progetto → una sola passata; la seconda risponde con l'id del task attivo.
2. **Ripresa**: kill del processo a metà passata → al riavvio il task riparte, gli item già persistiti non si duplicano (`current` invariato) e il task raggiunge uno stato terminale.
3. **Fencing**: un owner che ha perso il lease non scrive più nulla (heartbeat, checkpoint e finish a 0 righe); nessuna scrittura di un owner stantio.
4. **Budget**: il totale delle sintesi di un task è ≤ cap (+ al più uno scope perso per crash), anche attraverso più tentativi.
5. **Shutdown pulito**: `ibis stop` con una passata in corso requeuea il task e non consuma una recovery.
6. **Osservabilità**: `ibis://tasks/{id}` restituisce stato, checkpoint, claim/recovery, errore; i log hanno i dieci eventi `task_*`.
7. **Contratto MCP**: 58 tool, contract test verde, fixture discovery rigenerata solo per la descrizione.
8. **Fail-open**: un errore del task store non impedisce la passata (warn).
9. **Nessuna regressione**: A1 e observations invariati; live 4.0.0 verde.
10. **Documentazione**: `docs/ai_and_settings.md` riceve `tasks.*`; la coda WQ-5 linka questo documento.

---

# Parte II — F17: OpenTelemetry opt-in

---

## 0. Ancoraggi verificati

| Ancoraggio | Fatto verificato |
|---|---|
| `crates/ibis-logger/src/lib.rs:122-143` | `init(log_file, level, file_only)` costruisce un `tracing_subscriber::fmt` con `Sink` custom, `with_ansi(false)`, `with_target(false)`, `set_global_default` first-call-wins. |
| `crates/ibis-logger/src/lib.rs:100-120` | Selezione sink: `Discard` / `Stderr` / `File` / `Multi`; stdout mai toccato. |
| `crates/ibis-logger/Cargo.toml` | Solo `tracing` + `tracing-subscriber`: nessuna dipendenza OTel. |
| `crates/ibis-server/src/main.rs:531-539` | `stdio_quiet = mode == "stdio" \|\| is_plugin()`; logger inizializzato prima di ogni altra cosa. |
| `crates/ibis-server/src/tools.rs:114-131` | `dispatch`: `actor_context`, log `MCP Tool Call: {name}`. |
| `crates/ibis-server/src/mcp_server.rs:216-260` | `call_tool`: durata misurata (`t0.elapsed()`), esito `ok`/`is_error`/`Err`, log `MCP Tool Done`. |
| `crates/ibis-server/src/registry.rs:716-1035` | `TOOL_ANNOTATIONS`: `name`, `title`, `read_only` — la fonte per l'attributo `mcp.tool.read_only`. |
| `crates/ibis-ai/src/client.rs:33-66` | Choke point unico di embedding e generazione: `Client::embed_text`, `Client::batch_embed_text`, `Client::generate_content`. |
| `crates/ibis-ai/src/types.rs:94-125` | `GenerationConfig { temperature, max_output_tokens, route_hint, response_cache }`. |
| `crates/ibis-ai/src/types.rs:127+` | `UsageMetadata` (prompt/candidates/cached tokens). |
| `crates/ibis-ai/src/cost.rs:19-66` | `CostTracker`: il costo è registrato per call-site (`system:*`) su DB; non è un attributo del provider. |
| `crates/ibis-optimization/src/observations.rs:101-110` | `ObservationPassReport` (scopes, summarized, fallbacks, embedding_retries, deferred, failed) — gli attributi del pass. |
| `crates/ibis-server/src/consolidation.rs:108-121` | Il loop daemon logga progetto e esito: punto in cui nasce lo span `memory.consolidate`. |
| `crates/ibis-core/src/config.rs:523-612` | `Config` root: punto del blocco `telemetry`. |
| `.github/workflows/ci.yml:18-30` | Job `test`: fmt, clippy, `cargo test --workspace`. |
| `.github/workflows/ci.yml:72-113` | Job `live-tests` con SurrealDB 4.0.0. |

---

## 1. Sintesi delle decisioni (le 7 domande chiuse)

| # | Domanda | Decisione |
|---|---|---|
| 1 | Compile-time o runtime | **Entrambi**: feature Cargo `otel` (default off: il build standard non linka lo stack OTel) + `telemetry.otlp.enabled` (default false). Feature spenta + config accesa → warn esplicito, non silenzio. |
| 2 | Protocollo | `http/protobuf` (default, porta 4318, niente stack tonic) e `grpc` (opt-in). Endpoint base; il path `/v1/traces` lo aggiunge l'exporter. |
| 3 | Cosa si strumenta | Tre span con nomi stabili: `mcp.tool` (in `call_tool`), `ai.generate` e `ai.embed` (in `Client`), `memory.consolidate` (nel loop daemon/tool). Nessuno span per query DB o per item: cardinalità e rumore. |
| 4 | Attributi | Whitelist esplicita, costruita da funzioni pure: nomi/id/conteggi/stato. **Mai** contenuti, prompt, titoli, path assoluti, messaggi d'errore del provider. |
| 5 | Costo | Token (prompt/completion/cached) e `route_hint` sì; USD no: il pricing per provider non è nel layer (il `CostTracker` su DB resta la fonte di billing). |
| 6 | Shutdown | Un `TelemetryGuard` interno a `ibis-logger` + `shutdown_telemetry(timeout)` chiamato nella sequenza di `main.rs:700-726`; flush bounded 5s, un collector irraggiungibile non blocca l'uscita. |
| 7 | Metriche/log OTel | Fuori v1: solo span. Esportare anche i log duplicherebbe il sink `fmt`; le metriche si derivano dagli span. |

---

## 2. Aggancio

- **Logger**: `ibis_logger::init` (`lib.rs:122-143`) e il sink custom (`:30-120`).
- **Tool call**: `mcp_server::call_tool` (`:216-260`) — durata e esito già calcolati.
- **Annotazioni**: `TOOL_ANNOTATIONS` (`registry.rs:734`), `annotation_of` (`:1038-1040`).
- **AI**: `Client::generate_content` / `embed_text` (`client.rs:33-66`).
- **Passate**: `consolidation::run_pass` (`:78-122`), `consolidate_loop` (`memory_consolidate.rs:160-315`), `observations_pass` (`observations.rs:396-461`).
- **Config**: `Config` root (`config.rs:523-612`).
- **Shutdown**: `main.rs:700-726`.

## 3. Flusso attuale

1. L'unico subscriber è `fmt` su stderr/file; `stdio_quiet` garantisce che stdout resti pulito per il framing MCP (`lib.rs:100-120`, `main.rs:531-539`).
2. Ogni tool call produce due righe di log (`MCP Tool Call: {name}`, `MCP Tool Done: {name} ok in {duration}`) ma **nessuno span**: non esiste un identificatore di operazione che leghi la tool call alle chiamate AI che ha generato.
3. `Client::generate_content` non logga nulla; l'unico segnale di consumo è il `CostTracker` su DB, per call-site, senza correlazione con la richiesta.
4. Le passate loggano inizio/fine per progetto, ma non hanno un envelope: non si può rispondere a "quanto è costata la passata X" senza aggregare righe di log per timestamp.
5. Non esiste alcun export OTLP, nessuna configurazione di collector, nessun sampling.
6. Il logger non ha un handle di shutdown: `init` restituisce `()` e non c'è modo di flushare un exporter.

**Gap funzionale:** manca la correlazione latenza ↔ costo ↔ tool, che è esattamente ciò che serve per leggere i numeri di E16 (task) e D10/D11 (routing) in produzione. F17 non aggiunge comportamento: aggiunge la sua osservabilità.

## 4. Trasformazione

### 4.1 Feature e dipendenze

- `crates/ibis-logger/Cargo.toml`:

```toml
[features]
default = []
otel = ["dep:opentelemetry", "dep:opentelemetry_sdk", "dep:opentelemetry-otlp", "dep:tracing-opentelemetry"]

[dependencies]
opentelemetry = { version = "…", optional = true }
opentelemetry_sdk = { version = "…", features = ["rt-tokio"], optional = true }
opentelemetry-otlp = { version = "…", features = ["http-proto", "grpc-tonic"], optional = true }
tracing-opentelemetry = { version = "…", optional = true }
```

Le versioni esatte si scelgono all'implementazione e si pinnano in `Cargo.lock`; il vincolo è che `tracing-opentelemetry` sia compatibile con `tracing 0.1` (workspace) e che `opentelemetry-otlp` esponga entrambi i transport.

- `crates/ibis-server/Cargo.toml`: feature pass-through `otel = ["ibis-logger/otel"]`.
- Il build standard **non** cambia albero delle dipendenze: la verifica è in CI (`cargo tree -p ibis-server | grep opentelemetry` deve essere vuoto senza feature).

### 4.2 Config e composizione del subscriber

```json
"telemetry": {
  "otlp": {
    "enabled": false,
    "endpoint": "http://127.0.0.1:4318",
    "protocol": "http/protobuf",
    "service_name": "ibis",
    "sample_ratio": 0.1,
    "headers": {}
  }
}
```

| Chiave | Default | Clamp/validazione | Semantica |
|---|---|---|---|
| `enabled` | `false` | — | Accende l'exporter (richiede il build con feature `otel`). |
| `endpoint` | `http://127.0.0.1:4318` | vuoto → default | Base URL del collector. |
| `protocol` | `http/protobuf` | `grpc` \| `http/protobuf`; ignoto → default + warn | Transport OTLP. |
| `service_name` | `ibis` | trim, cap 64 | `service.name` della resource. |
| `sample_ratio` | `0.1` | `[0,1]` | Head sampling parent-based. `1.0` per debug locale. |
| `headers` | `{}` | — | Header statici (es. auth). **Possono contenere segreti**: il file di config è locale, non va committato. |

Composizione (sostituisce il `fmt().finish()` attuale, mantenendo byte-identico l'output `fmt`):

```rust
let registry = tracing_subscriber::registry()
    .with(LevelFilter::from_level(max))       // un solo filtro per entrambi i layer
    .with(fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(sink));
#[cfg(feature = "otel")]
let registry = registry.with(otel_layer_opt);  // Option<Layer> = None quando spento
let _ = tracing::subscriber::set_global_default(registry);
```

`init` resta idempotente (first call wins) e continua a non toccare stdout. Il provider OTel è conservato in un `OnceLock<TelemetryGuard>` interno; `shutdown_telemetry(5s)` fa `provider.shutdown()` e ignora l'errore (loggato a warn).

**Feature spenta, config accesa:** warn una volta `"telemetry.otlp.enabled is true but this build has no otel support (feature 'otel'); telemetry stays off"`. Stessa filosofia del loop di consolidamento non avviato (`consolidation.rs:47-52`).

### 4.3 Span model e attributi (whitelist)

Tutti i nomi sono costanti; nessun valore dinamico nel nome dello span.

**`mcp.tool`** — creato in `call_tool`, instrumentando il future (`Instrument`), non con `enter()` attraverso await:

| Attributo | Tipo | Fonte | Note |
|---|---|---|---|
| `mcp.tool.name` | string | `request.name` | 58 valori. |
| `mcp.tool.read_only` | bool | `annotation_of(name).read_only` | Tabella statica. |
| `ibis.project` | string | `args.project_name`, sanitizzato | Cap 120; assente se non c'è. |
| `ibis.actor` | string | `actor_context` in `dispatch` (`Span::current().record`) | Cap 120; `unknown` ammesso. |
| `ibis.runtime_mode` | string | `cfg.runtime_mode` | `personal`/`server`/`plugin`. |
| `mcp.tool.outcome` | string | `ok` \| `error` \| `panic` | Registrato dopo; lo status dello span è `Error` sugli ultimi due. |

**`ai.generate`** — in `Client::generate_content`:

| Attributo | Tipo | Fonte |
|---|---|---|
| `ai.provider` | string | `r.name()` |
| `ai.route_hint` | string | `config.route_hint` |
| `ai.temperature` | f64 | `config.temperature` |
| `ai.max_output_tokens` | int | `config.max_output_tokens` |
| `ai.finish_reason` | string | `candidate.finish_reason` (post-call) |
| `ai.tokens.prompt` / `.completion` / `.cached` | int | `usage_metadata` (post-call, se presente) |
| `ai.error.kind` | string | variante di `AiError` (mai il messaggio: può contenere il prompt) |

**`ai.embed`** — in `embed_text`/`batch_embed_text`: `ai.provider`, `ai.embeddings.count`, `ai.embeddings.dim`, `ai.error.kind`.

**`memory.consolidate`** — nel runner E16 (o in `run_pass` quando il task store è spento):

| Attributo | Tipo | Fonte |
|---|---|---|
| `ibis.project` | string | parametro |
| `ibis.task.id` | string | `task:⟨…⟩` (assente senza task store) |
| `ibis.task.recoveries` | int | riga task |
| `ibis.items` / `ibis.summaries` / `ibis.deferred` / `ibis.fallbacks` / `ibis.failed` | int | report della passata |
| `ibis.outcome` | string | `completed` \| `failed` \| `requeued` |

**Regola di cardinalità**: nessun attributo è un set aperto non bounded. `ibis.project` è bounded dai repo configurati; `ibis.actor` è un'identità (cap 120); i tool sono 58; i provider sono un enum. Un test puro verifica che l'insieme delle chiavi prodotte sia esattamente la whitelist.

### 4.4 Cosa non si esporta (e perché)

- **Contenuti**: titoli, contenuti di memoria, prompt, risposte, estratti di codice. Il collector non è il posto della conoscenza del cliente.
- **Messaggi d'errore del provider**: possono riecheggiare il prompt. Si esporta `ai.error.kind`; il messaggio resta nel log locale.
- **Path assoluti**: `ibis.project` è il nome logico; nessun `C:\Users\…`.
- **Identità forti**: `ibis.actor` è l'attore già usato nelle righe condivise (non un'email, salvo che l'operatore la usi come attore). Documentato.
- **Query e parametri dei tool**: solo il nome del tool.

### 4.5 Shutdown, stdio e compatibilità

- **stdio**: l'exporter scrive su rete, mai su stdout; gli errori interni OTel vanno a `tracing` (feature `internal-logs` / error handler), non a `println!`. Il test `stdio_framing_with_otel_enabled` asserisce stdout vuoto durante una tool call.
- **Shutdown**: nella sequenza di `main.rs:700-726`, dopo lo stop dei worker e prima della chiusura del DB: `ibis_logger::shutdown_telemetry(Duration::from_secs(5))`. Un flush fallito è un warn.
- **Batch processor**: `BatchSpanProcessor` con coda bounded (default dell'SDK); collector irraggiungibile → span droppati, warn una volta, nessun blocco.
- **Test**: `init` è globale e idempotente; i test che vogliono l'exporter usano `opentelemetry_sdk::testing::InMemorySpanExporter` in un processo di test dedicato (o `tracing::subscriber::with_default`), perché il default globale è già preso dal primo test che chiama `init`.

### 4.6 Sampling

`ParentBased(TraceIdRatioBased(sample_ratio))`. Un daemon con molte tool call read-only non deve pagare un export per ognuna; `1.0` è la scelta per il debug locale. Le passate (`memory.consolidate`) hanno uno span radice proprio e seguono lo stesso sampler. Nessun tail sampling in v1 (richiederebbe un collector).

---

## 5. Casi limite

| Caso | Comportamento |
|---|---|
| Feature `otel` assente, config accesa | Warn esplicito, telemetria spenta, resto invariato. |
| Feature presente, `enabled=false` | Nessun provider, nessun layer, nessun costo. |
| Collector irraggiungibile | Span droppati dal batch processor, warn una volta, tool call mai fallita. |
| `sample_ratio=0` | Nessuno span esportato; il layer resta montato (costo minimo). |
| `sample_ratio` fuori range | Clamp + warn. |
| `protocol` sconosciuto | Default `http/protobuf` + warn. |
| Endpoint vuoto | Default loopback. |
| stdio + exporter | stdout vuoto (framing intatto); export su rete. |
| Shutdown con collector lento | Flush bounded 5s, poi esce. |
| Tool che panica | Span `mcp.tool` con `outcome=panic`, status Error (`catch_tool_panic` già esistente). |
| `usage_metadata` assente | Attributi token omessi (non zero). |
| Provider che restituisce `NaN` nei token | Sanitizzato a 0 (il tipo è int). |
| Due `init` nel processo | First call wins (comportamento attuale). |
| Header con segreto nel config | Supportato ma documentato come non committabile. |
| Build con feature + test senza collector | I test usano l'in-memory exporter, non la rete. |

---

## 6. Test

**Unit (`ibis-logger`)**

| Test | Asserzione |
|---|---|
| `config_defaults_off` | `TelemetryConfig::default().otlp.enabled == false`, endpoint loopback. |
| `protocol_unknown_falls_back` | `"carrier-pigeon"` → `http/protobuf` + warn. |
| `sample_ratio_clamped` | `-1` → 0, `2` → 1, `NaN` → default. |
| `feature_off_warns_once` | Con `enabled=true` e feature spenta: un solo warn, nessun provider. |
| `fmt_output_is_unchanged` | Il layer `fmt` produce le stesse righe del subscriber attuale (fixture). |
| `shutdown_without_provider_is_noop` | `shutdown_telemetry` senza exporter non panica e ritorna subito. |
| `stdio_stdout_stays_empty` | Con sink `Discard`, una riga di log non tocca stdout. |

**Unit (attributi, funzioni pure)**

| Test | Asserzione |
|---|---|
| `tool_attributes_match_whitelist` | L'insieme delle chiavi è esattamente quello di §4.3. |
| `tool_attributes_never_contain_content` | Un args con `text`/`content`/`query` non produce attributi con quei valori. |
| `project_is_sanitized_and_capped` | Newline rimossi, cap 120. |
| `read_only_comes_from_the_annotations_table` | `memory_search` → true, `memory_add` → false. |
| `ai_error_exports_kind_only` | Il messaggio d'errore non compare negli attributi. |
| `span_names_are_stable_constants` | `mcp.tool`, `ai.generate`, `ai.embed`, `memory.consolidate`. |

**Feature-gated (`--features otel`)**

| Test | Asserzione |
|---|---|
| `in_memory_exporter_receives_one_tool_span` | Una tool call → 1 span `mcp.tool` con nome/read_only/outcome. |
| `in_memory_exporter_receives_ai_span` | Una generazione → 1 span `ai.generate` con provider e token. |
| `sampled_out_spans_are_not_exported` | `sample_ratio=0` → 0 span. |
| `shutdown_flushes_pending_spans` | Span creato e shutdown → presente nell'exporter. |

**CI**

| Job | Comando |
|---|---|
| `otel-build` (nuovo) | `cargo build -p ibis-server --features otel` + `cargo test -p ibis-logger --features otel` + `cargo tree -p ibis-server \| grep -q opentelemetry && exit 1 \|\| true` (senza feature). |
| `test` (esistente) | Invariato: il build default non linka OTel. |

---

## 7. Accettazione

1. **Default inerte**: build e config di default → zero dipendenze OTel nell'albero, zero exporter, output `fmt` byte-identico.
2. **Correlazione**: con un collector, una tool call produce uno span `mcp.tool` con figli `ai.generate`/`ai.embed` (relazione padre-figlio visibile nel collector).
3. **Nessun contenuto**: il test della whitelist dimostra che prompt/contenuti/titoli non lasciano il processo.
4. **stdio intatto**: stdout vuoto con exporter attivo (framing MCP).
5. **Shutdown**: flush ≤ 5s, collector lento non blocca l'uscita.
6. **Config onesta**: feature assente + config accesa → warn, non silenzio.
7. **Nessun nuovo tool**: 58 invariati.
8. **Nessuna regressione**: A1, observations e live suite verdi.

---

# Parte III — G19: proiezione memoria → CLAUDE.md / AGENTS.md

---

## 0. Ancoraggi verificati

| Ancoraggio | Fatto verificato |
|---|---|
| `crates/ibis-search/src/memory/projection.rs:14-66` | `write_memory_projection`: rigenera `.ibis/memory/<kind>/*.md` via `memory_export` (team + current), `.gitignore` con `observation/` e `working_state/`. |
| `crates/ibis-search/src/memory/export.rs:33-99` | `memory_export`: filtri `status`, `visibility` (default team), `kind`, `project`; esclude `store='derived'` (`:51-59`); proiezione con `branch`, `confidence`, `created_at`, `updated_at`, `details`. |
| `crates/ibis-search/src/memory/mod.rs:88-115` | `Memory`: `id`, `kind`, `title`, `content`, `status`, `confidence`, `visibility`, `owner`, `tags`, `branch`, `valid_*`, `created_at`, `updated_at`, `store`, `details`. |
| `crates/ibis-search/src/memory/mod.rs:126+` | `memory_from_row`: decode tollerante (campi mancanti → default). |
| `crates/ibis-server/src/registry.rs:383-394` | Spec `memory_export`: `project_name`, `kind`, `status`, `visibility`, `format`, `actor`. **Nessun filtro branch.** |
| `crates/ibis-server/src/state.rs:246-307` | `resolve_memory_projection_root` + `maybe_project_memory`: la proiezione server-side conosce la root del repo solo per i repo configurati. |
| `crates/ibis-core/src/config.rs:21-41` | `memory.projection_enabled` (default false); `--team` la forza on. |
| `crates/ibis-cli/src/client.rs:417-631` | `MemoryAction`: add/search/expand/update/delete/history/rule/rules/state/failures/why/session/capture/share/bootstrap-git-store/rebuild-git-notes. Nessun comando di proiezione doc. |
| `crates/ibis-cli/src/client.rs:1423-1439` | `execute_cli(args, caller: FnOnce(ToolCallPayload))`: il caller (in `main.rs:105-111`) stampa; la libreria non riceve l'output del tool. |
| `crates/ibis-cli/src/mcp.rs:110-177` | `call_server_tool` / `call_mcp_tool`: aprono la sessione, invocano, **stampano** e ritornano `()`. |
| `crates/ibis-server/src/main.rs:100-117` | Il binario delega a `execute_cli`; `eval` è intercettato prima (`:87-92`). |
| `crates/ibis-search/src/memory/import.rs:68-126` | `memory_import` accetta JSON, non markdown: nessun read-back da CLAUDE.md/AGENTS.md. |
| `README.md` (§What it is not) | "Not a markdown sync bus": il vincolo di prodotto che G19 deve rispettare. |

---

## 1. Sintesi delle decisioni (le 8 domande chiuse)

| # | Domanda | Decisione |
|---|---|---|
| 1 | Dove vive il renderer | **Client-side** (crate `ibis-cli`), non nel server: il team server non può scrivere nei repo dei client. Il server resta la fonte dei dati (`memory_export`), il CLI possiede il file. |
| 2 | Nuovo tool? | **No.** `memory_export` è già la query giusta; il comando CLI la compone e rende. Nessun cambio di schema del tool, nessuna fixture. |
| 3 | Ownership del file | Solo la regione fra marker. Fuori dai marker il file è dell'utente e non viene mai toccato (salvo il newline finale necessario all'append). |
| 4 | Marker | `<!-- ibis:begin memory -->`, `<!-- ibis:project: <name> -->`, `<!-- ibis:end memory -->`. Un blocco per file; progetto diverso → rifiuto. |
| 5 | Determinismo | Output byte-identico a parità di memorie: nessun timestamp nel blocco, ordinamento totale (kind → confidence desc → title → id), troncamenti fissi. Due run → nessun diff. |
| 6 | Contenuto | `kind ∈ {rule, constraint, gotcha}`, `status=current`, `visibility=team`, progetto richiesto; derivati esclusi (già server-side); `branch` filtrabile (`--branch`, righe vuote incluse). Cap 20 per kind. |
| 7 | Sicurezza del file | Scrittura atomica (tmp + rename nella stessa dir), EOL preservato, marker injection sanificata nel contenuto, `--check` per la CI, `--dry-run` per vedere senza scrivere. |
| 8 | Read-back | **Mai.** Nessun import da questi file. Il one-way è ciò che tiene il prodotto fuori dal business del sync bidirezionale. |

---

## 2. Aggancio

- **Dati**: `SearchService::memory_export` (`export.rs:33-99`), tool `memory_export` (`registry.rs:383-394`).
- **Proiezione esistente**: `write_memory_projection` (`projection.rs:18-66`) — modello di rendering e di ownership rigenerabile, non riusato direttamente (quella scrive N file, G19 un blocco).
- **CLI**: `MemoryAction` (`client.rs:417-631`), `execute_cli` (`:1423-1439`), trasporto (`mcp.rs:110-177`), dispatch del binario (`main.rs:100-117`).
- **Config**: `memory.projection_enabled` (`config.rs:21-41`) — **non** usata da G19 (il comando è esplicito).
- **Governance**: `ActorContext::LOCAL` + `visibility=team` (default di export) — nessuna riga personale nel file.

## 3. Flusso attuale

1. La memoria vive su SurrealDB; `.ibis/memory/<kind>/*.md` è una proiezione **rigenerabile** per la revisione team, scritta dal server solo se `memory.projection_enabled` e la root del repo è risolvibile (`state.rs:288-307`).
2. Un agente che lavora nel repo (Claude Code, Copilot CLI, Cursor, Codex…) legge `CLAUDE.md`/`AGENTS.md`, non `.ibis/memory/`. Oggi non esiste alcun ponte: la conoscenza durabile resta invisibile a chi non chiama i tool ibis.
3. `memory_export` sa già produrre esattamente i dati necessari (regole/constraint/gotcha, team, current, per progetto), ma il CLI non ha un comando che lo usi e non è in grado di ricevere l'output del tool (`execute_cli` consegna il payload, non il risultato).
4. Il vincolo "not a markdown sync bus" è rispettato perché non esiste alcun percorso di lettura: nessuno importa markdown. G19 deve aggiungere solo la direzione opposta, con ownership esplicita.
5. `memory_export` non filtra `branch`: il CLI deve farlo client-side (il campo è nella projection).

**Gap funzionale:** i dati ci sono, il formato di destinazione è standard de facto, ma mancano un comando, un renderer deterministico e un protocollo di merge che non distrugga il file dell'utente.

## 4. Trasformazione

### 4.1 Comando e seam CLI

Nuovo sottocomando nel gruppo esistente:

```text
ibis memory project --target agents-md|claude-md [--project <name>] [--path <file>]
                    [--branch <name>] [--check] [--dry-run]
```

- `--target` obbligatorio: `agents-md` → `AGENTS.md`, `claude-md` → `CLAUDE.md`.
- `--path` default: `<cwd>/<target file>`; `--project` default: basename della directory del file (o `--project` esplicito).
- `--branch`: include le righe con `branch` vuoto **o** uguale (case-insensitive). Default: nessun filtro.
- `--check`: non scrive; exit 0 = blocco presente e aggiornato, 1 = mancante o stantio, 2 = errore di parsing/uso.
- `--dry-run`: stampa il blocco renderizzato su stdout, non scrive.

**Seam**: il CLI deve ricevere l'output del tool. Refactor minimo e a parità di byte:

1. `mcp::call_mcp_tool` / `call_server_tool` restituiscono `Result<String, CliError>` (il testo già impaginato da `print_tool_content`, scritto in un `String` invece che su stdout). Su `is_error`: `Err(CliError::Transport("Tool returned an error:\n{text}"))`.
2. `execute_cli` accetta `F: FnOnce(ToolCallPayload) -> Result<String, CliError>`; per i comandi normali stampa il testo su stdout (parità), per `memory project` lo passa al renderer.
3. `main.rs:105-111` diventa `execute_cli(argv, |payload| runtime.block_on(call_server_tool(payload)))`.

Il test di parità (`print_tool_content` su `String` vs fixture) protegge l'output di tutti gli altri comandi.

### 4.2 Selezione del contenuto

Payload: `memory_export { project_name, status: "current", visibility: "team", format: "json" }` (nessun `kind`, filtro client-side). Il server applica già:

- `status = current` e `visibility = team` (`export.rs:39-50`);
- esclusione `store='derived'` (`:54-59`) — le osservazioni non finiscono nel doc;
- scope progetto (`:69-72`).

Filtro client-side:

```text
kind ∈ {rule, constraint, gotcha}
AND (branch vuoto OR branch eq_ignore_case(--branch))     // solo con --branch
```

Ordinamento totale: `kind` (rule=0, constraint=1, gotcha=2) → `confidence` desc → `title` lowercase asc → `id` asc. Cap: 20 per kind, 60 totali. Le righe con `title` e `content` entrambi vuoti sono scartate (non producono una riga vuota).

### 4.3 Formato del blocco e rendering deterministico

```markdown
<!-- ibis:begin memory -->
<!-- ibis:project: ibis-assistant -->
## Ibis memory

Durable team knowledge for this project. Generated by `ibis memory project`; edits inside the markers are overwritten.

### Rules
- **No direct DB writes from tools** — every mutation goes through the search service so governance and edges stay consistent.
- **Pin the SurrealDB sidecar version** — the live gate runs v4.0.0 only.

### Constraints
- **Python 3.11 only** — the AST rule runner uses `match` statements.

### Gotchas
- **`math::max` is gone in 4.x** — use `math::max` only on arrays; scalar comparisons inline.
<!-- ibis:end memory -->
```

Regole di rendering (tutte deterministiche):

1. Header fisso (`## Ibis memory` + una riga di spiegazione): nessun timestamp, nessun hash, nessun conteggio variabile.
2. Sezioni `### Rules` / `### Constraints` / `### Gotchas` presenti solo se non vuote.
3. Riga: `- **<title>** — <one line>`. `title` = `title` o, se vuoto, prima riga del content; cap 120. `one line` = content con whitespace collassato, prima frase/riga, cap 200 + `…` se troncata.
4. Sanificazione: `<!--` → `&lt;!--`, `-->` → `--&gt;`, CR/LF → spazio, tab → spazio, caratteri di controllo rimossi. Una memoria non può chiudere o aprire un marker.
5. Insieme vuoto: blocco con header + `_No current rules, constraints or gotchas._` (niente sezioni). Il blocco resta stabile, non appare/scompare.
6. EOL: quello del file esistente (rilevato); LF per un file nuovo. Il blocco è composto con l'EOL scelto.
7. Dimensione: cap implicito 60 righe × ~320 byte; oltre 32 KB un warn (config patologica, non un errore).

### 4.4 Merge con marker (matrice)

Il parser cerca le righe marker **esatte** (nessuna indentazione, case-sensitive). Stato del file → azione:

| `begin` | `end` | Progetto | Azione |
|---|---|---|---|
| 0 | 0 | — | Append in coda: una riga vuota di separazione, poi il blocco. Se il file non finisce con newline, se ne aggiunge uno. |
| 1 | 1 | uguale o assente | Sostituzione esatta della regione (marker inclusi), resto del file intatto. |
| 1 | 1 | diverso | **Rifiuto**: `"AGENTS.md already contains an ibis memory block for project '<altro>'; remove it or use a different file"`. |
| 1 | 0 / 0 | 1 | **Rifiuto**: `"unbalanced ibis memory markers"`. |
| >1 | qualsiasi | — | **Rifiuto**: `"multiple ibis memory blocks"`. |
| marker dentro un fence di codice | — | — | Il parser è line-based e non interpreta markdown: un marker dentro un fence è comunque un marker. Documentato; il rifiuto su ambiguità è la difesa. |

Idempotenza: la seconda esecuzione produce byte identici (test snapshot). La sostituzione non aggiunge/rimuove righe vuote fuori dalla regione.

### 4.5 Scrittura atomica, EOL, `--check` / `--dry-run`

- Scrittura: `<file>.ibis-tmp` nella stessa directory → `std::fs::rename` (su Windows sostituisce; su Unix atomico). Un errore a metà non lascia il file troncato.
- Permessi: il rename preserva il file di destinazione? No: il nuovo file prende i permessi di default. Per non perdere l'eseguibilità (irrilevante per `.md`) e per coerenza, si copiano i permessi del file esistente prima del rename (best-effort; su errore si scrive comunque, con warn).
- `--check` confronta la regione esistente con il render: nessuna scrittura, exit code come §4.1. È il modo per usarlo in CI senza sorprese.
- `--dry-run` stampa il blocco completo (marker inclusi) e un'intestazione su stderr con il file di destinazione; nessuna scrittura.

### 4.6 Governance e limiti (perché non è un sync bus)

- **Solo team**: `visibility=team` è il default di export e il CLI non espone `--visibility`. Le memorie personali non finiscono mai in un file potenzialmente committato.
- **Solo current**: una regola superseded/archiviata sparisce al run successivo. La supersession è la governance, non una nota a piè di pagina.
- **Derivati esclusi**: le osservazioni C8 restano fuori (già server-side); il doc contiene solo conoscenza durevole scritta da umani/agenti.
- **Nessun read-back**: `memory_import` prende JSON; nessun percorso legge il blocco. La frase del README resta vera e va precisata: "one-way projection with an owned marker region".
- **Ownership**: il file è dell'utente; ibis possiede solo la regione. Un conflitto di merge git sulla regione si risolve rigenerando (il contenuto è derivato).
- **Segreti**: il contenuto è memoria team già condivisa nel DB; il comando non aggiunge superficie. Il warn su file > 32 KB è anche un segnale di accumulo anomalo.

---

## 5. Casi limite

| Caso | Comportamento |
|---|---|
| File inesistente | Creato con solo il blocco (LF), nessun titolo inventato. |
| File vuoto | Append del blocco. |
| File senza newline finale | Newline aggiunto prima dell'append (unica modifica fuori regione, documentata). |
| CRLF | Preservato in tutto il file e nel blocco. |
| Marker sbilanciati | Rifiuto, nessuna scrittura. |
| Due blocchi | Rifiuto. |
| Blocco di un altro progetto | Rifiuto con il nome del progetto trovato. |
| Memoria con `<!-- ibis:end memory -->` nel testo | Sanificata: il marker non è riconoscibile. |
| Memoria con markdown complesso (liste, code fence) | Ridotta a una riga (whitespace collassato, prima frase): il blocco resta leggibile e bounded. |
| Titolo duplicato | Entrambe le righe, ordinate per id (la dedup è del DB, non del renderer). |
| Nessuna memoria team | Blocco con la riga esplicita "No current…". |
| `--branch` senza corrispondenze | Blocco vuoto esplicito (stesso caso). |
| `--check` su file mancante | Exit 1, messaggio su stderr. |
| `--check` su file con marker rotti | Exit 2. |
| Progetto sconosciuto | `memory_export` restituisce 0 righe → blocco vuoto; il nome progetto resta nel marker (nessun errore: il progetto può esistere solo nel DB remoto). |
| Server offline | Errore di trasporto del CLI, nessuna scrittura (il file resta intatto). |
| `--path` su directory | Errore d'uso (exit 2). |
| File non scrivibile | Errore, nessun tmp lasciato (cleanup best-effort). |
| Concorrenza (due run) | Rename atomico: vince l'ultimo; il contenuto è deterministico, quindi il risultato è lo stesso. |

---

## 6. Test

**Unit — renderer (`ibis-cli/src/memory_project.rs`)**

| Test | Asserzione |
|---|---|
| `render_is_deterministic` | Stesso input → stessi byte; input permutato → stesso output (ordinamento totale). |
| `render_orders_rule_constraint_gotcha` | Ordine delle sezioni e tie-break confidence/title/id. |
| `render_caps_20_per_kind` | 25 regole → 20 righe. |
| `render_sanitizes_marker_injection` | `<!--` e `-->` neutralizzati; il parser non vede marker nel contenuto. |
| `render_collapses_whitespace_and_truncates` | Newline/tab → spazio; cap 200 con `…`. |
| `render_title_falls_back_to_first_line` | Title vuoto → prima riga del content, cap 120. |
| `render_empty_set_is_explicit` | Riga "No current…", nessuna sezione. |
| `render_uses_the_file_eol` | LF e CRLF producono blocchi coerenti. |

**Unit — merge**

| Test | Asserzione |
|---|---|
| `merge_appends_when_no_markers` | Separazione di una riga vuota; resto intatto. |
| `merge_replaces_between_markers` | Prefix/suffix byte-identici; regione nuova. |
| `merge_is_idempotent` | Seconda run → file byte-identico. |
| `merge_refuses_single_marker` | Errore, nessuna scrittura. |
| `merge_refuses_duplicate_blocks` | Errore. |
| `merge_refuses_other_project` | Errore con il nome trovato. |
| `merge_preserves_outside_bytes` | Snapshot di prefix/suffix. |
| `merge_preserves_crlf` | Byte di EOL invariati fuori e dentro la regione. |
| `merge_creates_missing_file_with_lf` | Contenuto = solo blocco. |
| `merge_handles_missing_final_newline` | Newline aggiunto una volta; seconda run idempotente. |
| `merge_writes_atomically` | Un errore simulato dopo il tmp non tocca il file. |
| `check_exit_codes` | 0 aggiornato, 1 stantio/mancante, 2 parse error. |

**Unit — comando**

| Test | Asserzione |
|---|---|
| `project_payload_is_export_json` | `memory_export` con project/status/visibility/format attesi, nessun `kind`. |
| `project_defaults_to_cwd_and_target_file` | `agents-md` → `AGENTS.md`, `claude-md` → `CLAUDE.md`. |
| `project_branch_filter` | Righe vuote + uguali incluse, altre escluse. |
| `project_dry_run_does_not_write` | File assente dopo il run. |
| `execute_cli_returns_tool_text` | Il seam restituisce il testo e il comando normale lo stampa (parità). |

**Live (riuso)**

| Test | Asserzione |
|---|---|
| `export_excludes_personal_and_derived` | Già coperto da C8/live estate: 3 righe team current, 0 personali, 0 derivate. |
| `project_from_seeded_estate` | Con un fake caller che restituisce l'export di un'estate seminata, il blocco contiene esattamente le 3 righe attese. |

**Contract**

| Test | Asserzione |
|---|---|
| `tool_contract_test` | 58 invariati; `memory_export` schema invariato. |
| `discovery_snapshot_test` | Nessuna rigenerazione (nessun cambio di descrizione). |

---

## 7. Accettazione

1. **Idempotenza**: due esecuzioni consecutive → file byte-identico.
2. **Ownership**: nessun byte fuori dalla regione cambia (snapshot), salvo il newline finale necessario all'append.
3. **Ambiguità rifiutata**: marker sbilanciati, doppi blocchi e progetto diverso non producono scritture.
4. **Contenuto corretto**: solo `rule`/`constraint`/`gotcha` team current; personali e derivate assenti (test su estate seminata).
5. **Determinismo**: nessun timestamp/hash nel blocco; ordinamento totale.
6. **CI-ready**: `--check` con exit code documentati; `--dry-run` senza scritture.
7. **Nessun nuovo tool**: 58 invariati, `memory_export` invariato.
8. **Nessun read-back**: `memory_import` non conosce i file; il README è aggiornato con la precisazione one-way.
9. **Nessuna regressione**: A1 e observations verdi.

---

# Parte IV — G20: adapter harness standardizzati

---

## 0. Ancoraggi verificati

| Ancoraggio | Fatto verificato |
|---|---|
| `docs/ide-integrations.md:11-20` | Matrice superfici: VSIX (auto/attach/spawn), Cursor plugin (stdio/http), Copilot plugin (stdio/http), MCP App, Claude Desktop (mcp-bridge), CLI. |
| `docs/ide-integrations.md:29-35` | Attore: header solo loopback, argomento `actor` per LAN; consolidamento solo daemon. |
| `plugin/mcp.json` | `stdio` spawn `ibis -mode stdio -runtime-mode plugin` + `RUNTIME_MODE=plugin`. |
| `cursor-plugin/mcp.json` | Stessa forma, senza `type: stdio`. |
| `vscode-extension/package.json:307-329` | `ibis.runtime.mode` (auto default), `ibis.runtime.mcpUrl` default `http://127.0.0.1:3030/mcp`. |
| `vscode-extension/extension.js:293-301` | `resolveBackendSelection`: `auto`/`attach` → URL, `spawn` → child. |
| `crates/ibis-server/src/mcp_server.rs:59-68` | `IbisServer { router, state, with_logging, plugin_binding_done }`. |
| `crates/ibis-server/src/mcp_server.rs:107-109` | `tool_visible`: oggi solo il gate ticketing. |
| `crates/ibis-server/src/mcp_server.rs:181-201` | `list_tools`: `retain(!is_ticketing_tool)` quando non configurato. |
| `crates/ibis-server/src/mcp_server.rs:216-237` | `call_tool`: rifiuto actionable per tool nascosto. |
| `crates/ibis-server/src/registry.rs:1042-1068` | `TICKETING_TOOLS` + `is_ticketing_tool`: precedente di "gruppo". |
| `crates/ibis-server/src/registry.rs:1038-1040`, `:1076-1099` | `annotation_of` + `annotated_tool`: titolo, `readOnlyHint`, `_meta.ui` curator per 3 tool. |
| `crates/ibis-server/src/sampling.rs:54-92` | `client_declares_sampling` + `create_message`: la capability è già letta da `peer_info()`. |
| `crates/ibis-server/src/state.rs:577-595` | Catena `ide_sampling`: sampling → lm_bridge → ollama (`FallbackChain`). |
| `crates/ibis-server/src/state.rs:502-570` | Cloud pool (`gemini`/`openai_compat`/`claude`/`cursor`) + `use_ide_provided_models`. |
| `crates/ibis-core/src/config.rs:396-423` | `ReasoningConfig` (`provider`, `use_ide_provided_models`). |
| `crates/ibis-server/src/resources.rs:269-273` | `peer_connection_id`: identità di connessione già disponibile per dedupe. |

---

## 1. Sintesi delle decisioni (le 7 domande chiuse)

| # | Domanda | Decisione |
|---|---|---|
| 1 | Cosa è realmente duplicato | **Non** le annotazioni (una tabella, `TOOL_ANNOTATIONS`) né la UI curator (una funzione, `annotated_tool`): sono già centralizzate. È duplicato il **wiring di connessione** (tre manifest + default VSIX + doc) e la **policy di sampling** per host. G20 attacca quelli, non inventa un'astrazione per ciò che è già unico. |
| 2 | Profili runtime | `config.harness`: `default` + lista ordinata di profili con `match` (substring case-insensitive sul `clientInfo.name`), `sampling` (`auto`/`never`), `hidden_tool_groups`. Primo match vince; sconosciuto/assente → default. |
| 3 | Default | Profilo default = comportamento attuale: nessun tool nascosto, sampling `auto` (catena capability-based esistente). La fixture discovery resta byte-identica. |
| 4 | Gruppi di tool | Tabella statica `TOOL_GROUPS` in `registry.rs` accanto a `TICKETING_TOOLS`: `ticketing` (già gated), `settings`, `optimize`. Un gruppo nascosto è invisibile in `list_tools`, assente in `get_tool`, rifiutato in `call_tool` con messaggio actionable. |
| 5 | Sampling | `auto` = catena attuale; `never` = il leg `mcp_sampling` si dichiara non connesso (`connected() == false`) e la catena passa a lm_bridge/ollama. Policy per sessione, ultimo chiamante vince sulle sessioni HTTP concorrenti (stessa limitazione già documentata in `sampling.rs:11-14`). |
| 6 | Fonte unica del wiring | `harness/profiles.json` committato: per ogni superficie, connessione, attore, manifest. Un test di conformità confronta i manifest reali (`plugin/mcp.json`, `cursor-plugin/mcp.json`, default VSIX) con il descrittore. Niente codegen in v1. |
| 7 | Perché non di più | "Annotations" e "UI" sono già un punto solo; aggiungere un livello di indirezione sopra `TOOL_ANNOTATIONS` sarebbe duplicazione, non rimozione. `instructions` per-host non è fattibile: `get_info()` non riceve il `clientInfo` (`mcp_server.rs:150-175`). |

---

## 2. Aggancio

- **Superfici e wiring**: `docs/ide-integrations.md`, `plugin/mcp.json`, `cursor-plugin/mcp.json`, `vscode-extension/package.json` + `extension.js:293-301`.
- **Server**: `IbisServer` (`mcp_server.rs:59-68`), `tool_visible` (`:107-109`), `list_tools` (`:181-201`), `call_tool` (`:216-260`), `get_info` (`:150-175`).
- **Gruppi**: `TICKETING_TOOLS` (`registry.rs:1042-1068`), `TOOL_ANNOTATIONS` (`:734-1034`).
- **Sampling**: `SamplingSessionRegistry` (`sampling.rs`), catena in `state.rs:577-595`.
- **Identità di connessione**: `resources::peer_connection_id` (`:269-273`).
- **Config**: `Config` root (`config.rs:523-612`).

## 3. Flusso attuale

1. Ogni host ha il suo wiring: il VSIX risolve `auto`/`attach`/`spawn` (`extension.js:293-301`); Cursor e Copilot hanno un `mcp.json` che spawna stdio; Claude Desktop usa `mcp-bridge`; la CLI risolve loopback dal config.
2. Il server non sa (e non usa) chi è il client, tranne per la capability `sampling` (`sampling.rs:56-59`). Non esiste alcun profilo per host.
3. Le annotazioni sono già centralizzate (`TOOL_ANNOTATIONS`) e applicate a ogni tool (`annotated_tool`, `:1076-1089`); la UI curator è un `_meta.ui` per 3 tool. Non c'è duplicazione da rimuovere qui.
4. L'unica visibilità condizionata è il gruppo ticketing, deciso dalla config del server, non dal client.
5. La matrice di connessione vive in tre posti (due manifest + i default VSIX) più la documentazione; nulla verifica che restino coerenti.
6. Il sampling è una catena unica per processo: `ide_sampling` → lm_bridge → ollama. Un host che dichiara `sampling` ma lo implementa male non ha un interruttore per-sessione.

**Gap funzionale:** manca un punto dichiarativo in cui la relazione host → (superficie tool, policy sampling, wiring) sia scritta una volta e verificata; e manca un interruttore per non usare il sampling di un host specifico.

## 4. Trasformazione

### 4.1 Descrittore delle superfici (`harness/profiles.json`)

Fonte unica, committata, versionata:

```json
{
  "version": 1,
  "surfaces": [
    {
      "id": "vscode",
      "connection": { "default": "auto", "attach_url": "http://127.0.0.1:3030/mcp" },
      "actor": "header+argument",
      "manifest": "vscode-extension/package.json"
    },
    {
      "id": "cursor",
      "connection": {
        "default": "stdio",
        "stdio": { "command": "ibis", "args": ["-mode", "stdio", "-runtime-mode", "plugin"], "env": { "RUNTIME_MODE": "plugin" } }
      },
      "actor": "argument",
      "manifest": "cursor-plugin/mcp.json"
    },
    {
      "id": "copilot-plugin",
      "connection": {
        "default": "stdio",
        "stdio": { "command": "ibis", "args": ["-mode", "stdio", "-runtime-mode", "plugin"], "env": { "RUNTIME_MODE": "plugin" } }
      },
      "actor": "argument",
      "manifest": "plugin/mcp.json"
    },
    { "id": "claude-desktop", "connection": { "default": "bridge", "bridge": "mcp-bridge -url <endpoint>" }, "actor": "argument" },
    { "id": "cli", "connection": { "default": "loopback" }, "actor": "env+argument" }
  ]
}
```

Test di conformità (`crates/ibis-server/tests/harness_matrix_test.rs`): per ogni superficie con `manifest`, il file reale deve corrispondere ai campi dichiarati (comando/args/env per i manifest JSON; `ibis.runtime.mode` default e `ibis.runtime.mcpUrl` default per il `package.json` VSIX). Le superfici senza manifest (Claude Desktop, CLI) sono verificate solo per la presenza nel descrittore e nella doc. Niente parsing di markdown: la prosa resta umana, i file macchina sono pinnati.

### 4.2 Profili runtime (`config.harness`)

```json
"harness": {
  "default": { "sampling": "auto", "hidden_tool_groups": [] },
  "profiles": [
    { "match": ["visual studio code"], "name": "vscode", "sampling": "auto", "hidden_tool_groups": [] },
    { "match": ["cursor"], "name": "cursor", "sampling": "auto", "hidden_tool_groups": [] },
    { "match": ["copilot"], "name": "copilot", "sampling": "never", "hidden_tool_groups": ["settings"] }
  ]
}
```

Esempio illustrativo, **non** il default: `Config::default()` ha `profiles: []` e `default` vuoto, quindi nessun host cambia comportamento finché l'operatore non scrive il blocco.

- `match`: substring case-insensitive sul `clientInfo.name`; l'ordine della lista è l'ordine di precedenza.
- `sampling`: `auto` (default) | `never`. Valore ignoto → `auto` + warn.
- `hidden_tool_groups`: sottoinsieme di `TOOL_GROUPS`; gruppo ignoto → warn + ignorato.
- Un profilo senza `name` usa l'id del match; il nome serve solo ai log.

### 4.3 Risoluzione dal `clientInfo`

```rust
pub fn resolve_harness_profile<'a>(cfg: &'a HarnessConfig, client_name: Option<&str>) -> &'a HarnessProfile
```

- `client_name` = `peer_info().map(|i| i.client_info.name.clone())` (owned: il profilo è risolto per richiesta, non trattenuto oltre).
- Assente (client pre-initialize, lifecycle `server/discover`) → `default`.
- La risoluzione è una funzione pura, testabile con una matrice di nomi.
- Log: `debug` per chiamata; `info` al primo avvistamento per connessione (`peer_connection_id` + profilo), per non spammare.

### 4.4 Enforcement

**Tool visibility** — `IbisServer::tool_visible(name)` diventa:

```rust
fn tool_visible(&self, name: &str, profile: &HarnessProfile) -> bool {
    let config_visible = self.state.ticketing_configured() || !is_ticketing_tool(name);
    config_visible && !profile.hides(group_of(name))
}
```

- `list_tools`: `retain(|t| self.tool_visible(&t.name, profile))`.
- `get_tool`: stessa condizione (un tool nascosto è assente anche ai lookup).
- `call_tool`: rifiuto con messaggio actionable che nomina il gruppo e la chiave di config (`"tool 'settings_apply' is hidden by the '<profile>' harness profile (harness.profiles[].hidden_tool_groups)"`).
- `TOOL_GROUPS` statica: `ticketing` (riusa `TICKETING_TOOLS`), `settings` (`settings_get`, `settings_apply`, `settings_open_ui`), `optimize` (`optimize_knowledge`). Ogni tool appartiene a zero o un gruppo; il test di contratto verifica che i gruppi siano disgiunti e che ogni nome esista in `TOOL_SPECS`.

**Sampling** — `McpSamplingProvider` riceve una cella di policy (`Arc<AtomicU8>`: `Auto=0`, `Never=1`) condivisa con `AppState`; `connected()` restituisce `policy != Never && registry.connected()`. Il dispatcher aggiorna la cella quando risolve un profilo (solo su `call_tool`, dove la sessione è certa). La catena (`FallbackChain`) salta il leg non connesso come già fa oggi (`state.rs:583-592`).

**Nessun cambio a `get_info`**: `instructions` resta unico. La ragione è tecnica, non estetica: `get_info()` non ha il contesto della richiesta (`mcp_server.rs:150-175`), quindi un'istruzione per-host richiederebbe di spostare la negoziazione, con rischio sui client esistenti. Fuori v1.

### 4.5 Cosa non si duplica (e perché è un risultato, non una rinuncia)

- **Annotazioni** (`title`, `readOnlyHint`): una tabella, un punto di applicazione (`annotated_tool`). Un profilo per-host che le cambiasse creerebbe due verità sullo stesso tool.
- **UI curator** (`_meta.ui.resourceUri`): un link per 3 tool, ignorato dai client che non supportano MCP Apps. Nessun beneficio nel renderlo condizionato.
- **Prompt**: `prompts.rs` è già unico; le differenze di prefisso (`/mcp.<server>.<prompt>`) sono lato host e non richiedono config.
- **Istruzioni MCP**: uniche per scelta (§4.4).

---

## 5. Casi limite

| Caso | Comportamento |
|---|---|
| `clientInfo` assente | Profilo `default` (comportamento attuale). |
| Nome client sconosciuto | `default`. |
| Due profili matchano | Vince il primo della lista (ordine esplicito in config). |
| `hidden_tool_groups` con gruppo ignoto | Warn una volta, gruppo ignorato; gli altri valgono. |
| Gruppo `ticketing` nascosto dal profilo e non configurato | Nascosto una volta (intersezione), nessun doppio filtro. |
| Tool nascosto chiamato da un client con lista stantia | Errore actionable (pattern già esistente, `mcp_server.rs:221-237`). |
| `sampling=never` su un host che dichiara sampling | Il leg si dichiara non connesso; la catena usa lm_bridge/ollama. |
| `sampling=never` senza fallback funzionale | Errore del provider come oggi (nessuna nuova semantica). |
| Due sessioni HTTP con profili diversi | Ultimo chiamante vince sulla cella sampling (documentato); la visibilità tool è per-richiesta e quindi corretta. |
| `settings_*` nascosto al VSIX | **Attenzione**: il comando "Open Settings" del VSIX chiama il tool `settings_open_ui` (`extension.js:745-779`), quindi un profilo `vscode` con `hidden_tool_groups: ["settings"]` rompe quella UI. Il default non nasconde nulla; la doc del profilo lo segnala esplicitamente. |
| Descrittore e manifest divergono | Test di conformità rosso (drift bloccato). |
| Superficie senza manifest | Verificata solo per presenza; documentata come tale. |
| `clientInfo.name` con newline/unicode | Matching su stringa normalizzata (trim + lowercase); nessun attributo di log non sanificato. |
| Build con `harness` assente dal config | `#[serde(default)]` → default = comportamento attuale. |

---

## 6. Test

**Unit — risoluzione profilo**

| Test | Asserzione |
|---|---|
| `profile_matrix` | `"Visual Studio Code"` → vscode; `"Cursor"` → cursor; `"copilot-cli"` → copilot; `"unknown"`/`None` → default. |
| `first_match_wins` | Due `match` sovrapposti → il primo. |
| `unknown_sampling_falls_back_to_auto` | `"sometimes"` → auto + warn. |
| `unknown_group_is_ignored` | `["nope"]` → nessun gruppo nascosto + warn. |
| `default_profile_hides_nothing` | `hidden_tool_groups` vuoto; `sampling=auto`. |

**Unit — visibilità**

| Test | Asserzione |
|---|---|
| `groups_are_disjoint_and_known` | Ogni nome in `TOOL_GROUPS` esiste in `TOOL_SPECS`; nessun nome in due gruppi. |
| `hidden_group_is_absent_from_list` | `list_tools` non contiene i tool del gruppo. |
| `hidden_tool_call_is_rejected` | Errore actionable con nome profilo e chiave config. |
| `ticketing_gate_still_applies` | Non configurato + profilo default → nascosto come oggi. |
| `default_profile_keeps_the_fixture` | `discovery_snapshot_test` invariato. |

**Unit — sampling**

| Test | Asserzione |
|---|---|
| `never_makes_the_leg_disconnected` | `connected() == false` anche con registry connesso. |
| `auto_keeps_the_capability_check` | `connected()` = registry.connected(). |
| `policy_change_is_visible_to_the_provider` | La cella condivisa propaga. |

**Conformità descrittore**

| Test | Asserzione |
|---|---|
| `profiles_json_is_valid` | Schema minimo: version, surfaces, id unici. |
| `plugin_manifest_matches` | `plugin/mcp.json` == superficie `copilot-plugin`. |
| `cursor_manifest_matches` | `cursor-plugin/mcp.json` == superficie `cursor`. |
| `vscode_defaults_match` | `ibis.runtime.mode` default `auto` + `mcpUrl` default == superficie `vscode`. |
| `every_expected_surface_is_declared` | La lista statica delle superfici attese nel test (vscode, cursor, copilot-plugin, claude-desktop, cli) coincide con gli id del descrittore; la prosa di `ide-integrations.md` resta umana. |

**Live / regressione**

| Gate | Asserzione |
|---|---|
| `tools_discovery_test` | Il flusso `list_changed` del ticketing resta verde. |
| `live-tests` | Suite esistente verde (G20 non tocca il DB). |
| `retrieval_eval` / `observations_eval` | Invariati. |

---

## 7. Accettazione

1. **Default identico**: con `Config::default()` la superficie tool e la catena di sampling sono quelle di oggi (fixture discovery invariata, 58 tool al netto del gate ticketing).
2. **Profilo efficace**: un profilo con `hidden_tool_groups=["settings"]` nasconde i tre tool settings a `list_tools`/`get_tool` e li rifiuta in `call_tool` con messaggio actionable.
3. **Sampling**: `sampling=never` rimuove il leg sampling e lascia intatta la catena di fallback.
4. **Drift bloccato**: modificare `plugin/mcp.json` o i default VSIX senza aggiornare il descrittore fa fallire il test di conformità.
5. **Nessun tool nuovo**: 58 invariati, nessun cambio di schema.
6. **Nessuna regressione**: A1, observations e live suite verdi.
7. **Documentazione**: `docs/ide-integrations.md` linka il descrittore come fonte dei wiring; `docs/ai_and_settings.md` riceve `harness.*`.

---

# Ordine di implementazione

Nessun commit in questa fase. Step ordinati per dipendenza e per possibilità di shippare senza rischio.

| Step | Contenuto | File | Prerequisiti | Test |
|---|---|---|---|---|
| **1. E16 — modello e schema** | `task_status`, tabella `task`, indici (UNIQUE `active_key`), clamp config `tasks.*`, modello puro + SQL builders | `ibis-db/src/schema.rs`, `ibis-optimization/src/tasks.rs`, `ibis-core/src/config.rs` | — | unit modello + probe live UNIQUE/NONE |
| **2. E16 — store e runner** | `DbTaskStore`, claim/heartbeat/fencing/recovery/prune, `PassCheckpoint`, wiring in `AppState` e shutdown | `ibis-server/src/tasks.rs`, `state.rs`, `main.rs`, `assembly.rs` | 1 | unit fake store + live lease/reclaim |
| **3. E16 — superfici** | Tool `memory_consolidate` (id nel testo, no doppio spawn), risorse `ibis://tasks`, notifiche, descrizione tool + fixture | `tools_memory.rs`, `resources.rs`, `registry.rs`, `mcp_server.rs` | 2 | contract + resources protocol + live risorse |
| **4. F17 — logger** | Feature `otel`, `TelemetryConfig`, composizione registry, `shutdown_telemetry`, CI `otel-build` | `ibis-logger/*`, `config.rs`, `main.rs`, `ci.yml` | — (indipendente) | unit config + feature-gated in-memory |
| **5. F17 — span** | `mcp.tool` in `call_tool`, `ai.generate`/`ai.embed` in `Client`, `memory.consolidate` nel runner | `mcp_server.rs`, `tools.rs`, `ibis-ai/src/client.rs`, `ibis-server/src/tasks.rs` | 2, 4 | whitelist attributi + in-memory |
| **6. G19 — renderer e merge** | Modulo `memory_project`, marker, ordinamento, sanificazione, scrittura atomica, `--check`/`--dry-run` | `ibis-cli/src/memory_project.rs` | — | unit renderer + merge matrix |
| **7. G19 — comando e seam** | `MemoryAction::Project`, seam `execute_cli`/`call_server_tool` (parità output), docs | `ibis-cli/src/client.rs`, `mcp.rs`, `main.rs`, `README.md` | 6 | unit comando + parità output |
| **8. G20 — descrittore** | `harness/profiles.json`, test di conformità dei manifest | `harness/profiles.json`, `tests/harness_matrix_test.rs` | — | conformità |
| **9. G20 — profili runtime** | `harness.*` config, risoluzione `clientInfo`, `TOOL_GROUPS`, filtro, policy sampling | `config.rs`, `mcp_server.rs`, `registry.rs`, `state.rs`, `sampling.rs` | 8 | matrice profili + visibility + sampling |
| **10. Docs e coda** | `ai_and_settings.md` (`tasks.*`, `telemetry.*`, `harness.*`), `ide-integrations.md`, `memory_layer.md` (one-way), CHANGELOG `[Unreleased]`, WQ-5 | docs | 1–9 | — |

**Shippabilità intermedia:** gli step 1–3 sono rilasciabili senza F17/G19/G20 (E16 da solo vale il lease e l'osservabilità). Gli step 4–5 sono dietro feature+config, quindi inerti di default. Gli step 6–7 sono un comando CLI opt-in. Gli step 8–9 hanno default = comportamento attuale. Nessuno step richiede una migrazione o un cambio di superficie tool.

---

# Rischi e mitigazioni

| Rischio | Gravità | Mitigazione |
|---|---|---|
| **Lease scaduto durante una chiamata LLM lunga** → doppia passata | alta | Heartbeat in task tokio indipendente (10s su lease 30s) + fencing su `lease_epoch`; il vecchio owner non scrive. |
| **UNIQUE + `NONE` non si comporta come atteso su 4.0** | media | Probe live dedicato; piano B con sentinel `done:{id}`; nessuna presunzione nella specifica. |
| **Crash fra `add_memory` e l'archiviazione** → catena di supersessioni | media | `max_recoveries` limita i giri; C9 riconosce il merge; follow-up: archiviazione nella transazione di `add_memory`. |
| **Task orfani dopo un downgrade/rollback** | bassa | I task sono inerti per i runtime senza E16; il prune li rimuove; nessun dato di memoria dipende da loro. |
| **Costo OTel in build di produzione** | media | Feature default off; CI verifica l'albero senza feature; sampling 0.1. |
| **PII/contenuti nella telemetria** | alta | Whitelist di attributi con test; errori solo per `kind`; niente prompt/contenuti/path assoluti. |
| **Exporter che blocca lo shutdown** | media | Flush bounded 5s; batch processor non bloccante. |
| **G19 sovrascrive modifiche manuali** | alta | Ownership a marker; rifiuto su ambiguità; `--check`/`--dry-run`; idempotenza byte-identica. |
| **Marker injection da una memoria** | media | Sanificazione di `<!--`/`-->` nel contenuto + test dedicato. |
| **G19 su file con EOL misti** | bassa | EOL rilevato e preservato; il blocco usa lo stesso EOL; test CRLF. |
| **G20 nasconde un tool per errore** | media | Default nessun gruppo nascosto; config esplicita; errore actionable; test che il profilo default non cambia la fixture. |
| **`clientInfo.name` inaffidabile** | media | Match per substring con default; log del profilo risolto; nessuna decisione di sicurezza basata sul nome (solo superficie/policy opt-in). |
| **Policy sampling per-sessione su HTTP condiviso** | bassa | Ultimo chiamante vince, documentato (stessa limitazione del peer sampling); il caso reale è stdio/plugin (una sessione). |
| **Drift descrittore/manifest** | bassa | Test di conformità in CI. |
| **Scope creep di E16 verso le approvazioni** | media | §4.8 esplicito: fuori v1 con motivazione; lo schema (`requested_by`, `queued`) lascia la porta aperta. |

---

# Fuori scope / follow-up

- **E16 — approvazioni con expiry**: §4.8. Richiede un client che negozi un consenso umano e una decisione di prodotto su quali mutazioni approvare. Lo schema è pronto (`requested_by`, `queued`, `expires_at` come campo futuro).
- **E16 — archiviazione atomica dell'osservazione**: spostare l'`UPDATE` di archiviazione nella transazione di `add_memory` chiude la finestra di §4.3. Tocca `persist_consolidation` e `add_memory`: follow-up dedicato, non un requisito del task store.
- **E16 — task per altre lane**: ingest completo, proiezione, RAFT. Il modello è generico (`kind`), ma ogni lane deve dichiarare i propri `params`/`checkpoint`; niente task generici "vuoti".
- **E16 — cancellazione esplicita**: nessuna superficie in v1 (lo shutdown requeuea). Un `kind='cancel'` o un comando CLI richiederebbe un tool o una risorsa scrivibile: rimandato.
- **F17 — metriche e log OTLP**: solo span in v1. Le metriche si derivano; esportare i log duplicherebbe il sink `fmt`.
- **F17 — tail sampling / collector-side policies**: richiede un collector; fuori dal prodotto.
- **F17 — span DB/query**: cardinalità e rumore; il costo DB è già misurabile dai tempi dei tool.
- **G19 — codegen del blocco nel server**: il server non scrive nei repo dei client (team server). Il CLI è la superficie corretta.
- **G19 — `--target cursor-rules|windsurf-rules|…`**: stesso renderer, formati diversi. Solo dopo che `agents-md`/`claude-md` sono in uso.
- **G19 — branch corrente automatico**: richiede `git` nel CLI; v1 filtra solo su `--branch` esplicito.
- **G19 — import dal blocco**: mai, per contratto (one-way).
- **G20 — codegen dei manifest dal descrittore**: v1 verifica, non genera. Il codegen richiederebbe un passo di build nel VSIX e nei plugin.
- **G20 — istruzioni per-host**: `get_info()` non ha il contesto; richiederebbe un cambio di negoziazione.
- **G20 — profili per capability invece che per nome**: il nome è l'unico segnale disponibile oggi; le capability (sampling, apps) sono già usate dove servono.
- **B5**: assorbito da DiskANN in WQ-7 (nessuna colonna int8), come già registrato.

---

# Riepilogo dipendenze

```text
C8 (observations_pass, cap 10, backlog derivato) ──consumatore──► E16 (task envelope, budget ledger)
F4-B3 (loop daemon, nota "lease in-process") ──chiude──► E16 (lease di store, cross-process)
batch_manager (batch_status, ticker, shutdown) ──pattern──► E16 (stato per riga, sweep, stop)
sidecar_ledger (identità di processo) ──pattern──► E16 (lease_owner / epoch)
schema 4.0 (DEFINE TYPE, IF NOT EXISTS, UNIQUE+NONE) ──DDL──► E16 (task_status, task)
resources/subscriptions (F0.4) ──superficie──► E16 (ibis://tasks)
D10/D11 (routing, shadow) ──dati──► F17 (attributi route/tier)
CostTracker (bucket system:*) ──fonte billing──► F17 (token, non USD)
E16 (task id, recoveries) ──attributi──► F17 (memory.consolidate)
TOOL_ANNOTATIONS (F0.2) ──dato──► F17 (mcp.tool.read_only) e G20 (nessuna duplicazione)
memory_export (filtri status/visibility/kind, esclusione derivati) ──dato──► G19 (blocco doc)
memory.projection (pattern rigenerabile) ──pattern──► G19 (ownership della regione)
CLI execute_cli / call_server_tool ──seam──► G19 (output del tool al renderer)
TICKETING_TOOLS + tool_visible (F0.6) ──pattern──► G20 (TOOL_GROUPS, filtro per profilo)
sampling registry (F0.5) ──policy──► G20 (sampling auto/never)
ide-integrations.md + manifest ──fonte──► G20 (harness/profiles.json)
A1 + eval observations ──gate──► tutte (nessuna regressione)
tool_contract_test (58) ──gate──► tutte (nessun nuovo tool)
```
