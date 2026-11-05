# Piano operativo — remediation SurrealDB 4 e copertura test

**Data:** 2026-11-05  
**Repository:** `ibis-assistant`  
**Baseline di review:** `9451eea75093bb6aaf3c056ef9d5dcbe89a9ac34`  
**Commit esaminati:** `ec3cf748` (spec WQ-7), `153f4815` (allineamento gate), `ad00490b` (implementazione), `9451eea7` (chiusura documentale).  
**Destinatario:** agente esecutore con accesso al workspace e possibilità di modificare codice, test, CI e documentazione.

**Esito esecuzione (05/11):** implementato — F01–F13 chiusi con test di
regressione unitari e live (`fix(db)` `79bde42`, `fix(ingest-code)` `5c0e146`,
`fix(server)` `fde13c1`, `fix(logs)` `49038ff`, `fix(search)` `9f6c66d`,
`test(eval)` `622644c`, `ci` `90a3391`), eval rieseguito offline (additive +
rrf k ∈ {1,10,60}, [verdetto](./2026-11-05-wq7-fusion-verdict.md), [baseline
A1](./2026-11-02-a1-retrieval-eval-plan.md)). Live test richiesti in CI con
`IBIS_LIVE_REQUIRED=1` (job `live-tests`). Deviazioni documentate nella
sezione "Note di esecuzione" in fondo. Gate sezione 11 verde (fmt, clippy
`-D warnings`, `cargo test --workspace`, `git diff --check`, batteria live su
sidecar 4.0.0 con `IBIS_LIVE_REQUIRED=1`, zero skip). Review indipendente
(research-only, agente non autore) su `9451eea..HEAD`: zero blocker, F01–F13
tutti chiusi con test discriminanti; i 2 MAJOR trovati dalla review sono
stati corretti in consegna con nuovi test di regressione (`fix(ingest-code)`
finalize atomico `e16aa43`, `fix(search)` attribuzione RRF per rank `b14c077`
— report completo in fondo, §15).

## Note di esecuzione — scostamenti dal piano

1. **F02 rendering:** le id canoniche usano il codec SDK (`to_sql`) invece
   della concatenazione manuale — probe live: rendering `'src/a-b.rs'` /
   `"with space's file.rs"` / `'a\nb.rs'`, round-trip lossless su tutti i
   path collidenti e ostili (incluso tentativo di injection inertizzato).
   La chiave repo resta nel formato derivato da `sanitize_id` (convenzione
   record repo, fuori dal perimetro F02).
2. **F10 trattamento dei chunk storici:** definito esplicitamente — dopo un
   pass riuscito i chunk della versione precedente vengono **eliminati**
   (`DELETE … WHERE file = $file AND hash != $hash`): con chiavi
   `[file, start_line]` non esiste dimensione di storia; i residui di
   versione sarebbero duplicati obsoleti raggiungibili dal canale vettore.
   Il test live `version_round_trip_leaves_a_coherent_manifest` lo pinna.
3. **F07 famiglia:** oltre a `commit_chunk.updated_at` (dichiarato) e al
   binding delle variabili (`smart_query`), il writer legava `commit_date`
   come `datetime` su un campo schema `option<string>` — il writer ora passa
   la stringa RFC3339 verbatim come da commento schema (il read path
   converte con `type::datetime`).
4. **F11 + beneficio:** la separazione offset-lette/offset-confermato corregge
   anche il salto di linee non ancora processate al flush parziale del
   ticker (l'offset salvato copre solo il batch davvero chiuso).
5. **D2 calibrazione:** `calibrate_lexical_score(s) = s/(s+3)` scelta dopo
   sweep (monotona, limitata, ordine intra-canale invariante alla scala —
   test proprietà); il verdetto `additive` è riconfermato dalle misure
   post-fix, non precostituito.
6. **F09 propagate:** `ingest_error` ora propaga l'errore DB e il batch lo
   riporta (`process_batch_sync` → `err`) — il tailer non avanza più l'offset
   dopo un persist fallito (aggancio naturale a F11).

## 1. Mandato

Correggere tutti i finding confermati elencati sotto e integrare test unitari, integration e live che li riproducano. Non limitarsi al diff WQ-7: alcuni difetti sono preesistenti ma devono essere risolti nello stesso intervento.

Questo documento è self-contained. Prima di modificare codice:

1. leggere `docs/planning/2026-11-05-surrealdb-4-upgrade.md`;
2. leggere i file indicati in ciascun work package e i test esistenti;
3. consultare la documentazione ufficiale SurrealDB 4.0 quando serve confermare semantica SurrealQL;
4. trasformare ogni finding in una riga della propria checklist: finding, test riprodotto, fix, test finale, evidenza live;
5. verificare che ogni nuovo test di regressione fallisca prima del fix per il motivo atteso.

Vincoli:

- non indebolire expected, metriche o assertion per ottenere test verdi;
- non sopprimere errori con nuovi default silenziosi o warning ignorati;
- non riscrivere il design WQ-7 senza necessità: conservare `source_file:[repo,file_path]` e `file_chunk:[file_id,start_line]`;
- non considerare validi i live test skippati in CI;
- non ridecidere il vincitore `additive`/RRF prima di aver corretto entrambi e rieseguito l'eval;
- fare commit piccoli e verificabili, lasciando il working tree pulito alla fine;
- documentare eventuali scostamenti dal piano e la relativa evidenza tecnica.

Riferimenti ufficiali principali:

- <https://surrealdb.com/releases/4.0>
- <https://surrealdb.com/docs/surrealql/datamodel/ids>
- <https://surrealdb.com/docs/surrealql/statements/define/field>
- <https://surrealdb.com/docs/surrealql/statements/define/indexes>
- <https://surrealdb.com/docs/surrealql/functions/database/search>
- <https://surrealdb.com/docs/surrealql/functions/database/vector>
- <https://surrealdb.com/docs/surrealql/statements/remove>

## 2. Finding da chiudere

| ID | Origine | Finding |
|---|---|---|
| F01 | WQ-7 | Nel fallback ogni chunk ha `start_line=0`; la chiave `[file,start_line]` fa collassare tutti i chunk sullo stesso record. |
| F02 | WQ-7 | `sanitize_id(path)` rende non univoche le chiavi `source_file` (`src/a-b.rs`/`src/a/b.rs`, `Foo.rs`/`foo.rs`). |
| F03 | WQ-7 | Il dedupe globale precedente a RRF impedisce la somma dei contributi cross-channel. |
| F04 | WQ-7 | BM25 grezzo entra nella fusione `additive` insieme a score vettoriali calibrati. |
| F05 | WQ-7 | `memory_id_for_subject` cerca soltanto nel contenuto e non più nel titolo. |
| F06 | WQ-7 | L'indice `file_chunk.content` non alimenta un canale della fusione `ask_project`. |
| F07 | Preesistente | `apply_commit_txn` scrive `commit_chunk.updated_at` non dichiarato e chiama `execute` scartando tutte le variabili bound. |
| F08 | WQ-7 | Un chunk riutilizzato su una nuova versione conserva il vecchio embedding mentre contenuto/hash cambiano. |
| F09 | WQ-7 | Il log analyzer scrive ancora ID `source_file` scalari, rifiutati dal tipo array dello schema. |
| F10 | WQ-7 | Il cache check `count()>0` può lasciare chunk V1/V2 mescolati dopo V1 → V2 → V1. |
| F11 | Preesistente | Dopo un batch log fallito, il successo di un batch successivo salva un offset oltre il primo e lo salta definitivamente. |
| F12 | Preesistente | Il pruning ignora errori e cancella file/chunk senza atomicità, creando orfani non più raggiunti dal cleanup. |
| F13 | Latente | `migrate_with` non ordina le migrazioni: `[3,2]` termina a versione 2 e riapplica 3 al boot seguente. |

Non correggere come bug i `REMOVE FIELD in/out` delle relation senza nuove prove. Un probe live 4.0.0 ha mostrato che gli endpoint relation restano presenti dopo rimozione e ridefinizione. Aggiungere invece un test di caratterizzazione/idempotenza dello schema dopo il seed.

## 3. Organizzazione agentica

| Ruolo | Ownership | Dipendenze |
|---|---|---|
| Coordinatore | integrazione, review, commit e gate | tutte |
| Agente A — DB | `crates/ibis-db` | nessuna |
| Agente B — Ingest code | `crates/ibis-ingest-code` | contratto ID di A |
| Agente C — Commit/log | `crates/ibis-server/src/assembly.rs`, `crates/ibis-ingest-logs` | schema e ID di A |
| Agente D — Retrieval/eval | `crates/ibis-search`, `crates/ibis-optimization/src/eval.rs`, `evaluations/retrieval` | ID stabili, ingest per E2E |
| Agente E — Validazione | CI, live harness, report e documentazione | fix A–D integrati |

Evitare modifiche concorrenti agli stessi file. Il coordinatore risolve i punti di sovrapposizione ed è l'unico responsabile dell'integrazione finale. Ogni agente consegna: diff, test aggiunti, comandi eseguiti, risultati e rischi residui.

## 4. Fase 0 — Baseline e riproduzione

Prima dei fix, ogni agente aggiunge i test di regressione nel proprio perimetro e registra il fallimento atteso. I test non devono limitarsi a verificare stringhe SQL quando il bug riguarda il comportamento del datastore.

Requisiti specifici:

- ingest: verificare record, ID, contenuti, hash ed embedding risultanti;
- RRF: usare lo stesso documento in almeno due canali;
- versioning: controllare ogni chunk dopo V1 → V2 → V1, non l'assenza di scritture;
- live test: database scratch distinti e cleanup garantito;
- failure injection: verificare sia lo stato immediato sia un eventuale retry.

Aggiungere anche un test che esegue `init_schema`, semina relation e liste materializzate, riesegue `init_schema` e verifica che endpoint e relazioni restino coerenti.

## 5. Work package A — DB, identità e migrazioni

**File principali:** `crates/ibis-db/src/ids.rs`, `schema.rs`, `migrate.rs`, `crates/ibis-db/tests/live_surreal.rs`.

### A1 — Chiavi `source_file` lossless (F02)

Sostituire la costruzione manuale basata su `sanitize_id(path)` con un record ID strutturato:

- primo componente: repository;
- secondo componente: path repo-relative normalizzato con `/`;
- conservare maiuscole, spazi, Unicode e caratteri speciali;
- usare il codec/rendering SDK per gli identificatori interpolati, non concatenare manualmente stringhe non escaped;
- mantenere compatibili validazione e round-trip degli ID letti dal driver.

Test obbligatori:

- `src/a-b.rs` contro `src/a/b.rs`;
- `Foo.rs` contro `foo.rs` su filesystem case-sensitive;
- path con spazi, apostrofi, virgole, parentesi quadre, backslash e Unicode;
- round-trip ID → rendering → parsing;
- payload che simulano chiusura del literal e statement aggiuntivi;
- coexistenza live dei due file che prima collidevano.

### A2 — Schema `commit_chunk` (parte di F07)

Dichiarare `commit_chunk.updated_at TYPE option<datetime>`, coerentemente con il writer esistente, salvo motivare e documentare la rimozione del writer. Verificare live l'`UPSERT` completo e una seconda inizializzazione dello schema dopo il seed.

### A3 — Migrazioni ordinate (F13)

Rendere `migrate_with` deterministico: validare ID univoci, ordinare per ID crescente, persistere la versione soltanto dopo il successo di ciascuna migrazione. In alternativa rifiutare esplicitamente input non ordinato, ma non mantenere il comportamento attuale.

Test: `[3,2]`, duplicati, migrazione già applicata, errore intermedio senza avanzamento versione, seconda esecuzione no-op.

**Gate A:** `cargo test -p ibis-db --lib --tests` e live DB verdi.

## 6. Work package B — Ingest code

**File principali:** `crates/ibis-ingest-code/src/ingest.rs`, `crates/ibis-ingest-code/tests/ingest_pipeline.rs` e nuovi live test opportuni.

### B1 — Unicità chunk fallback (F01)

Calcolare il vero `start_line` di ogni chunk nel fallback; non assegnare più zero a tutti. Conservare una numerazione corretta anche quando vengono scartati chunk vuoti.

Test:

- più chunk fallback nello stesso file con ID tutti distinti;
- live ingest con almeno 25 chunk: 25 record e contenuti attesi;
- oltre 100 chunk per coprire più batch;
- file senza parser/AST e righe oltre il limite di chunking.

Il test `ingest_codebase_batching` non è sufficiente se conta soltanto le transazioni: estenderlo a ID e contenuti.

### B2 — Embedding stale (F08)

Quando hash o contenuto cambiano sulla stessa chiave, invalidare il vecchio embedding prima di riportare il chunk a `pending`. Conservare invece gli embedding validi durante il resume della stessa versione.

Test:

- V1 con embedding → V2 sullo stesso start-line: nessun vecchio embedding disponibile per V2;
- resume stessa versione senza perdita degli embedding validi;
- batch fallito senza persistenza indefinita del vettore precedente;
- query live che non recupera il contenuto V2 tramite il vettore V1.

### B3 — Versioning coerente (F10)

Rimuovere `count()>0` come prova di completezza della versione. La soluzione semplice preferita:

- early return soltanto se la versione su disco è già coerente;
- su hash cambiato, ricostruire il manifest completo dei chunk;
- aggiornare `source_file.hash` dopo aver accodato correttamente tutti i chunk;
- non usare un singolo chunk residuo come cache hit.

Test V1 → V2 → V1 con start-line spostate e numero di chunk differente. Al termine ogni chunk richiesto da V1 deve avere contenuto/hash V1; nessun chunk V2 deve risultare corrente. Definire esplicitamente il trattamento degli eventuali chunk storici.

Sostituire l'attuale test che richiede semplicemente zero scritture al revert: quella assertion premia il difetto.

### B4 — Pruning atomico (F12)

Eseguire cancellazione chunk e file in una transazione e propagare gli errori. Testare errore sul primo e sul secondo `DELETE`, assenza di eliminazioni parziali, retry completo e assenza live di chunk orfani.

**Gate B:** test unitari e live ingest verdi, inclusi revert e failure injection.

## 7. Work package C — Commit, log e tailer

**File principali:** `crates/ibis-server/src/assembly.rs`, `crates/ibis-ingest-logs/src/analyzer.rs`, `tailer.rs`, `crates/ibis-ingest-logs/tests/live_logs.rs`.

### C1 — Transazione commit (F07)

In `apply_commit_txn`:

- sostituire `execute(&sql)` con `smart_query(&sql, vars)`;
- mantenere una sola transazione atomica;
- verificare che ogni placeholder abbia una variabile;
- non completare commit quando la transazione fallisce.

Test unitari: capture di SQL e variabili, chunk singolo/multiplo, data presente/assente, commit completed/failed, ID invalidi senza disallineamento delle variabili.

Test live:

- pending commit → chunk con contenuto/hash/data corretti → completed;
- fallimento → `batch_status='failed'` e `batch_error` corretto;
- errore SQL → rollback completo e commit ancora pendente;
- nessun errore per `updated_at` dopo il fix dello schema.

Nota: SurrealDB 4.0 valuta un parametro non bound come `NONE`, non necessariamente come errore. Il test deve quindi verificare i valori, non soltanto il successo della query.

### C2 — ID canonici nel log analyzer (F09)

Sostituire l'ID scalare con il costruttore canonico usando `LogAnalyzer.project`. Il `RELATE` deve puntare allo stesso `source_file` scritto da code/git ingest.

Test:

- anomalia con `file='src/lib.rs'`;
- path con spazi e coppie che prima collidevano;
- esistenza del record canonico e dell'arco `related_to`;
- risultato presente in `recent_failures(project)`;
- nessun duplicato scalare;
- errore DB non interpretato come successo.

### C3 — Offset retry-safe (F11)

Separare offset di lettura e offset confermato. Al primo batch fallito:

- non salvare il nuovo offset;
- non processare batch successivi che possano rendere permanente il salto;
- ritentare il batch fallito prima di avanzare.

Test con AI `errore → successo`: secondo batch non oltrepassa il primo, retry del primo, persistenza soltanto dopo successo, restart prima del retry, successo di N+1 soltanto dopo conferma di N.

**Gate C:** test commit/log/tailer e live test verdi.

## 8. Work package D — Retrieval e fusione

**File principali:** `crates/ibis-search/src/search.rs`, `retrieval_boost.rs`, `memory/mod.rs`, test di `ibis-search`, `crates/ibis-optimization/src/eval.rs`, `evaluations/retrieval`.

### D1 — RRF con identità documento (F03)

Refactor delle strutture interne per rappresentare `(channel_id, document_id, rank, payload)`.

Regole:

- niente duplicati dello stesso documento dentro uno stesso canale;
- lo stesso documento può comparire in più canali;
- RRF somma tutti i contributi;
- dedupe del board soltanto dopo la fusione;
- `source_kind` pubblico invariato;
- `channel_id` interno disponibile per eval e diagnostica.

Test: documento vector+title+content, documento solo lessicale, rank differenti, verifica numerica della somma, una sola occorrenza finale, canali vuoti senza divisione per zero.

### D2 — Calibrazione BM25 (F04)

Eliminare l'uso diretto del BM25 grezzo nel blend con cosine/decay. Scegliere trasformazione e peso lessicale tramite eval; non introdurre un coefficiente arbitrario soltanto per preservare il vecchio golden set. Una trasformazione monotona limitata è un candidato, non una decisione già presa.

Test:

- BM25 alto non domina soltanto per unità di misura;
- ranking invariante rispetto a una scala costante degli score grezzi;
- vector forte contro match lessicale debole;
- match lessicale forte assente dal canale vettoriale;
- score finiti e ordinamento deterministico.

### D3 — Canale `file_chunk_text` (F06)

Aggiungere una query FULLTEXT su `file_chunk.content` a `ask_project`, con ID, path, hash, contenuto e current hash. Integrarla sia in `additive` sia in RRF, senza cambiare il `source_kind` pubblico del risultato.

Test live con chunk senza embedding trovato lessicalmente e con chunk presente in entrambi i canali.

### D4 — Subject su title/content (F05)

Ripristinare la ricerca del subject anche sul titolo, preservando visibilità, ranking BM25, binding e tie-break deterministico.

Test: match solo titolo, solo contenuto, entrambi, memory personale di altro attore esclusa, nessun match.

**Gate D:** test unitari e live search verdi prima dell'eval comparativo.

## 9. Dataset ed eval del ranking

Espandere il golden set con casi che rendano il full-text osservabile:

1. memory con `embedding=NONE` e match nel titolo;
2. memory senza embedding e match nel contenuto;
3. file chunk senza embedding e match lessicale;
4. documento presente in vector e full-text;
5. conflitto fra alta similarità vettoriale e alto BM25;
6. memory personale che non deve emergere;
7. memory superseded/stale esclusa;
8. path con caratteri speciali per verificare gli expected ID.

Il report deve conservare il `channel_id` interno: `top_channel='memory'` non distingue vector, title e content e non dimostra che il full-text abbia contribuito.

Eseguire dopo i fix:

- `additive`;
- RRF con `k ∈ {1,10,60}`;
- embedder offline obbligatorio;
- Ollama quando disponibile come controllo aggiuntivo.

I nuovi case devono avere assertion puntuali. MRR/recall aggregate e il floor `0.75` non bastano a certificare la copertura. Aggiornare `search.fusion`, baseline e `2026-11-05-wq7-fusion-verdict.md` soltanto dopo le misure, senza precostituire il vincitore.

## 10. CI e live test obbligatori

Estendere il job che avvia SurrealDB `v4.0.0` per eseguire i live test rilevanti, ad esempio:

```powershell
cargo test -p ibis-db --test live_surreal
cargo test -p ibis-ingest-code --tests
cargo test -p ibis-ingest-logs --test live_logs
cargo test -p ibis-search --test live_memory_estate
```

Configurare esplicitamente `IBIS_SPIKE_SURREAL_URL`. Introdurre una modalità come `IBIS_LIVE_REQUIRED=1`:

- locale senza sidecar: skip consentito;
- CI con sidecar richiesto: connessione fallita o test skippato = job fallito.

Adattare i test nuovi agli harness esistenti invece di creare infrastrutture duplicate. Verificare dai log CI il numero di test effettivamente eseguiti.

## 11. Gate finale

Eseguire dal repository root:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git diff --check
```

Poi, contro sidecar SurrealDB 4.0.0:

- tutti i live test dei crate modificati;
- retrieval eval offline;
- verifica che i nuovi test non risultino skippati;
- controllo dei report e delle metriche per-case;
- review indipendente dei diff da parte di un agente non autore dei fix.

Se il full workspace supera il timeout del tool, suddividere per crate senza dichiarare il gate superato finché tutti i comandi non terminano. Non interpretare un timeout come test verde.

## 12. Sequenza dei commit

Sequenza raccomandata, modificabile soltanto per esigenze di dipendenza:

1. `fix(db): lossless composite ids and migration ordering`
2. `fix(ingest-code): unique chunks, embedding invalidation and coherent versions`
3. `fix(server): bind commit transaction variables`
4. `fix(logs): canonical file links and retry-safe offsets`
5. `fix(search): correct cross-channel fusion and lexical coverage`
6. `test(eval): exercise lexical-only and cross-channel retrieval`
7. `ci: require live SurrealDB integration tests`
8. `docs: republish verified WQ-7 baseline and verdict`

Includere i test di regressione nel commit del rispettivo fix. Ogni commit integrato deve lasciare build e test pertinenti verdi.

## 13. Definition of Done

Consegnare soltanto quando tutte le condizioni sono soddisfatte:

- [x] F01–F13 hanno ciascuno almeno un test di regressione.
- [x] I comportamenti SurrealQL critici hanno copertura live.
- [x] Nessun live test richiesto in CI risulta skippato.
- [x] Nessun chunk fallback collide.
- [x] Nessun path distinto collide e i path speciali fanno round-trip.
- [x] Nessun contenuto nuovo usa un embedding precedente.
- [x] V1 → V2 → V1 produce chunk coerenti.
- [x] Commit e log persistono tutti i valori bound.
- [x] Tailer e pruning non perdono dati dopo errori.
- [x] RRF somma i contributi cross-channel e deduplica il board finale.
- [x] BM25 non entra grezzo nel blend.
- [x] Title/content e `file_chunk` lessicale sono coperti.
- [x] Le migrazioni sono ordinate e idempotenti.
- [x] Baseline e documentazione contengono soltanto misure rieseguite.
- [x] `cargo fmt`, clippy, workspace test, live test ed eval sono verdi.
- [x] La review indipendente non presenta finding bloccanti aperti.
- [x] Il working tree è pulito.

## 14. Report finale richiesto

Il report di consegna deve includere:

1. commit hash e sintesi di ogni fix;
2. mappa F01–F13 → test unitari/live che li chiudono;
3. comandi eseguiti e risultati, inclusi eventuali test non eseguiti;
4. numeri della nuova eval per embedder/fusione/k;
5. decisione finale sul default di fusione e relativa evidenza;
6. modifiche CI e prova che i live test siano stati eseguiti;
7. rischi residui o deviazioni dal piano;
8. review finale indipendente e stato del working tree.

Non dichiarare il lavoro completato se manca una di queste evidenze o se un finding resta senza test.

## 15. Report di consegna (05/11)

### 15.1 Commit

| Commit | Sintesi |
|---|---|
| `79bde42` | fix(db): id lossless (F02), `commit_chunk.updated_at` (F07a), migrazioni ordinate (F13) |
| `5c0e146` | fix(ingest-code): chunk fallback unici (F01), embedding invalidation (F08), versioning (F10), pruning atomico (F12) |
| `fde13c1` | fix(server): bind variabili transazione commit (F07b) |
| `49038ff` | fix(logs): id canonici analyzer (F09), offset retry-safe (F11), propagate errori persist |
| `9f6c66d` | fix(search): fusione cross-channel (F03), calibrazione BM25 (F04), canale `file_chunk_text` (F06), subject su titolo (F05) |
| `622644c` | test(eval): dataset 20 case, `absent_ids`, canali per caso, flag `--rrf-k` |
| `90a3391` | ci: job `live-tests` con `IBIS_LIVE_REQUIRED=1` |
| `bb20052` | fix(ingest-code): retry conflitti transazione SurrealKV (scritture idempotenti) |
| `5daf532` | test(ai): `default_api_url_substituted` deterministico con Ollama locale attivo |
| `58537b4` | docs: republish verified WQ-7 baseline and verdict |
| `e16aa43` | fix(ingest-code): finalize atomico hash UPSERT + cleanup stale chunk (post-review) |
| `b14c077` | fix(search): attribuzione best-channel in RRF per rank, non per ordine di query (post-review) |

### 15.2 Mappa F01–F13 → test

Nomi di test (greppabili) con file di appartenenza; tutti discriminanti sul
codice pre-fix (verificato dalla review indipendente per ispezione).

| Finding | Test di regressione | Live |
|---|---|---|
| F01 | `chunk_content_matrix`, `chunk_content_start_lines_are_unique` (`ibis-ingest-code/src/ingest.rs`) | `fallback_chunks_get_unique_start_lines`, `chunks_span_multiple_batches` (`tests/ingest_live.rs`) |
| F02 | `test_source_file_id_is_lossless`, `test_is_safe_record_id_array_forms`, `test_sanitize_id` (`ibis-db/src/ids.rs`) | `source_file_ids_are_lossless_and_collision_free` (`ibis-db/tests/live_surreal.rs`, probe injection inerte) |
| F03 | `ask_project_rrf_fusion_sums_cross_channel_ranks`, `ask_project_additive_sums_cross_channel_scores`, `rrf_fuse_sums_cross_channel_contributions` (`ibis-search`) | eval r16/r17 + dataset cross-channel |
| F04 | `calibrate_lexical_is_bounded_monotone_and_scale_order_invariant` (`retrieval_boost.rs`), `ask_project_additive_sums_cross_channel_scores` | eval baseline additive |
| F05 | `memory_id_for_subject` title/content (unit, `ibis-search/src/memory/mod.rs`) | `subject_lookup_finds_title_matches` (`tests/live_memory_estate.rs`) |
| F06 | `ask_project_additive_sums_cross_channel_scores` (canale `file_chunk_text`), `ask_project_rrf_fusion_sums_cross_channel_ranks` (canale in RRF) | `lexical_file_chunk_channel_reaches_ask_project` (`tests/live_memory_estate.rs`) |
| F07 | `apply_commit_txn_binds_every_variable` (`ibis-server/src/assembly.rs`), re-init schema nel test `commit_chunk_upsert_persists_all_values_and_schema_reinit_stays_coherent` | `commit_txn_persists_every_bound_value`, `commit_txn_failure_marks_failed_and_sql_error_rolls_back` (`ibis-server/tests/live_commit_txn.rs`) |
| F08 | `ingest_codebase_versioning` (assert `embedding = NONE` sul riuso, `tests/ingest_pipeline.rs`) | `version_round_trip_leaves_a_coherent_manifest` (`tests/ingest_live.rs`) |
| F09 | `ingest_error_links_canonical_source_file`, `ingest_error_collision_free_paths`, `ingest_error_failure_surfaces_as_batch_error` (`ibis-ingest-logs/src/analyzer.rs`) | `anomaly_links_canonical_source_file` (`tests/live_logs.rs`) |
| F10 | `ingest_codebase_versioning` (manifest hash avanzato, cleanup incluso) + `finalize_transaction_is_atomic_and_propagates` (post-review) | `version_round_trip_leaves_a_coherent_manifest` (V1→V2→V1) |
| F11 | `failed_batch_retries_before_offset_advances`, `restart_before_retry_does_not_skip` (`ibis-ingest-logs/src/tailer.rs`) | `offset_roundtrip_on_log_file_record` (`tests/live_logs.rs`) |
| F12 | `prune_transaction_is_atomic_and_propagates` (iniezione su entrambe le DELETE) | `prune_removes_files_and_chunks_without_orphans` (`tests/ingest_live.rs`) |
| F13 | `unsorted_migrations_apply_in_id_order`, `duplicate_migration_ids_are_rejected`, `failed_migration_does_not_advance_version` (`ibis-db/src/migrate.rs`) | `migrations_apply_in_id_order_live` (`tests/live_surreal.rs`) |

### 15.3 Comandi ed esiti

| Comando | Esito |
|---|---|
| `cargo fmt --all -- --check` | pulito (exit 0), anche dopo le fix post-review |
| `cargo clippy --workspace --all-targets -- -D warnings` | pulito (exit 0) |
| `cargo test --workspace` | exit 0, 0 failed — eseguito due volte: pre e post fix review |
| `git diff --check` | pulito |
| `cargo test -p ibis-ai --lib runner` | 11 passed (verifica pre-commit di `5daf532`) |
| Batteria live (sidecar SurrealDB **4.0.0**, `ws://127.0.0.1:38000`, `IBIS_LIVE_REQUIRED=1`, `--test-threads=1`): `-p ibis-db --test live_surreal -p ibis-ingest-code --tests -p ibis-ingest-logs --test live_logs -p ibis-search --test live_memory_estate -p ibis-server --test live_commit_txn` | exit 0 — live eseguiti: `live_surreal` 11, `ingest_live` 4, `live_logs` 5, `live_memory_estate` 12, `live_commit_txn` 2 (34 test live, zero skip: con `IBIS_LIVE_REQUIRED=1` una connessione fallita è panic). L'unico `ignored` presente nella batteria è `generate_discovery_fixture` (generatore di fixture per design, non è un test live SurrealDB). Eseguita due volte, pre e post fix review. |

Test non eseguiti: nessuno rispetto al perimetro del piano (l'eval è coperto
dalle run pre-consegna §15.4; nessun numero citato dipende dall'attribuzione
di canale corretta in §15.5, quindi le fix post-review non richiedono
re-run dell'eval).

### 15.4 Eval (offline, sidecar 4.0.0, 20/20 case)

| Configurazione | MRR | recall@5 | recall@15 |
|---|---|---|---|
| **additive (default)** | **0.950** | **0.975** | **1.0** |
| rrf k=1 | 0.842 | 0.950 | — |
| rrf k=10 | 0.918 | 0.925 | — |
| rrf k=60 | 0.908 | 0.925 | — |

Gli 8 case nuovi del dataset fanno tutti MRR 1.0 con canale attribuito;
zero violazioni `absent_leaks`. Report: `artifacts/eval/remediation-*.json`,
baseline committata `evaluations/retrieval/baseline-4.0.0-offline-additive.json`.

**Decisione fusione:** `additive` resta default — riconfermata dalle misure
post-fix (non precostituita): a parità di recall@15, additive batte tutte le
configurazioni RRF su MRR e recall@5 ([verdetto
WQ-7](./2026-11-05-wq7-fusion-verdict.md)).

### 15.5 Fix post-review (2 MAJOR, chiusi in consegna)

1. **Finestra crash del cleanup stale-chunk** (`e16aa43`): l'UPSERT dell'hash
   file e la `DELETE … hash != $hash` erano statement separati; un crash
   fra i due avanzava l'hash con chunk stantii ancora presenti, e il prossimo
   pass sarebbe tornato presto (`current_db_hash == hash`) lasciandoli per
   sempre. Ora le due scritture sono un'unica transazione atomica
   (`BEGIN TRANSACTION; UPSERT …; DELETE …; COMMIT TRANSACTION;`) — la
   finestra non esiste più e l'invariante "hash = versione coerente" è
   atomico. Test: `finalize_transaction_is_atomic_and_propagates`
   (iniezione fallimento sulla parte cleanup: nessun UPSERT hash può
   viaggiare senza il cleanup nella stessa statement); `ingest_codebase_versioning`
   aggiornato a pinnare la transazione unica.
2. **Attribuzione canale degenere in RRF** (`b14c077`): la strength
   `1.0 / score.max(1.0)` è sempre 1.0 (score di canale ≤ 1), quindi il
   "best channel" era sempre il primo in ordine di query. Ora la strength è
   il rank reale (`1/rank`) letto dalle stesse `rank_lists` che alimentano
   `rrf_fuse` — il ranking RRF non cambia (era già corretto), cambia solo la
   diagnostica `channel`, ora affidabile. Test:
   `ask_project_rrf_best_channel_follows_rank_not_query_order` (rank-1 in un
   canale successivo deve battere rank-2 nel primo canale); il tie-break a
   parità di rank resta deterministico (primo canale in ordine).

### 15.6 CI e prova live

`90a3391` aggiunge il job `live-tests`: SurrealDB **v4.0.0** pinnato (immagine
Docker + health-check), `IBIS_SPIKE_SURREAL_URL` puntato sul container,
`IBIS_LIVE_REQUIRED=1` che converte lo skip in failure, step separati per
crate che falliscono il job. Prova di esecuzione reale: batteria live locale
§15.3 con la stessa variabile (skip impossibile). NB: consegna senza push per
vincolo del mandato — l'evidenza di esecuzione è la run locale sopra; la run
CI si produca al primo push sul repo (`terenzif/ibis-assistant`).

### 15.7 Rischi residui (review indipendente, nessun blocker)

- **MINOR — coda tailer illimitata** (`ibis-ingest-logs/src/tailer.rs`): con
  AI/DB giù a lungo i batch pendenti si accumulano nel `VecDeque` senza cap
  né backoff (retry a ogni tick). Serve una decisione di design (cap o
  backoff) — follow-up dedicato, fuori dal perimetro remediation.
- **MINOR — collisione backslash su POSIX** (`ibis-db/src/ids.rs::normalize_rel_path`):
  `\` → `/` collidono su filesystem dove `\` è un carattere legittimo del
  nome file. Trade-off documentato nel docstring, coerente col piano; edge
  residuo del tema F02.
- **MINOR — `absent_leaks` non-gating** (`ibis-optimization/src/eval.rs`):
  le violazioni sono registrate nel report ma non fanno scendere sotto la
  soglia `--min-recall-5`. Oggi zero leak; renderle gating è un cambio di
  contratto del CLI eval A1 (exit code), proposto come follow-up.
- NIT: `DbError::Decode` riusato per id migrazione duplicato;
  asserzione loosa `contains("2")` in `live_logs` (mitigata dai check esatti
  dei record); tie-break `source_rank` (content prima di title) non pinna il
  vincitore in unit test; attribuzione additive O(board×canali) — irrilevante
  alle window attuali.
- L'attribuzione additive e i numeri eval non dipendono dalla fix §15.5.2
  (solo diagnostica RRF): nessun numero citato in §15.4 è invalidated.

### 15.8 Review indipendente e stato

Review research-only da agente non autore sul diff `9451eea..HEAD` (piano,
diff per-crate, workflow CI, baseline JSON, doc): F01–F13 tutti chiusi con
test discriminanti (nessun assert indebolito, nessun test solo-SQL dove il
bug è comportamentale, harness live senza skip impliciti, DB scratch con
cleanup); nessun panic/race/SQL-injection (id via rendering SDK + validatore
escape-aware con probe live di injection inerte); numeri doc/JSON coerenti
(MRR 0.950 / r@5 0.975 / r@15 1.0, per-channel 20 ✓, deviazioni oneste).
Esiti: 0 blocker, 2 MAJOR → chiusi in consegna (§15.5), 2 MINOR + 5 NIT →
rischi residui (§15.7). Working tree pulito a consegna avvenuta.
