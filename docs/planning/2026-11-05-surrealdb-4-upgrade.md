# WQ-7 — SurrealDB 4, upgrade greenfield

**Data:** 2026-11-05
**Registrato in:** [master work queue](./2026-11-02-master-work-queue.md) come WQ-7.
**Stato:** spec approvata. Nessun codice in questo commit. L'esecuzione è lo stream WQ-7, prima del gate R7 e prima di C9.
**Fonti:** [SurrealDB 4.0.0](https://surrealdb.com/releases/4.0) (3 novembre 2026), [post di rilascio](https://surrealdb.com/blog/surrealdb-4-the-database-and-memory-for-every-agent).

## Perché ora, e perché da zero

Il prodotto non ha una release pubblica. Il datastore di sviluppo si ricrea. Non c'è un upgrade 3.x → 4.0 da operare: un database nuovo nasce già a compatibility level 4, e le primitive sotto costano uno schema solo adesso. Farle dopo, con i dati, è la migrazione che questo stream rifiuta.

Pin attuale da sostituire insieme (un client 3.3 rifiuta un server 4.0, e il server 4.0 rifiuta quel client):

- crate `surrealdb` **3.3.4** nel workspace e in `ibis-db`, `ibis-server`, `ibis-ingest-code`, `ibis-optimization`
- sidecar e immagine **v3.3.5** (`deploy/Dockerfile`, job CI `retrieval-eval`)

Primo gesto dell'esecuzione: verificare che il crate `surrealdb` 4.0.0 sia su crates.io. Se non c'è, lo stream si ferma. Non si mescola il client 3.3.4 col binario 4.0.0.

## Fuori perimetro

Wire PostgreSQL/MySQL, cookie auth, bucket, indici spaziali, SurrealDS ed estensioni non hanno un call site in questo prodotto. `BEGIN ISOLATION LEVEL SERIALIZABLE` non è selezionabile: SurrealKV lo rifiuta.

## Criticità nel codice

Il nuovo esecutore e il protocollo 4.0 rompono il codice scritto sulla 3.3, anche a datastore vuoto.

1. **`LIMIT` senza `ORDER BY` non è più “i primi per chiave”.** Una scan parallela restituisce un sottoinsieme qualsiasi. `ORDER BY` esplicito in `crates/ibis-server/src/assembly.rs` (batch `pending`), in `crates/ibis-search/src/retrieval_boost.rs` (`LIMIT 50` sui simboli) e in ogni altro `LIMIT` il cui insieme oggi è accidentale.
2. **Lo score ibrido non è un piano d’indice.** `ask_score_expr` (`vector::similarity::cosine * 0.7 + decay`) in `crates/ibis-search/src/search.rs`, e le copie in `commit_meaning.rs`, `memory/mod.rs`, `memory/askref.rs`, `memory/asof.rs`, `crates/ibis-optimization/src/memory_consolidate.rs`, non usa l’indice. Il rango del canale diventa `ORDER BY vector::distance::cosine(embedding, $vec) LIMIT k`. Il decay, se resta, è un rescore sulla finestra già restituita e non può far rientrare una riga esclusa dall’indice.
3. **La baseline A1 della 3.3.5 non è un vincolo.** MRR 0.917 / recall@5 0.958 / recall@15 1.0 è stata misurata su sidecar 3.3.5. DiskANN e le somme in ordine diverso sulle distanze cambiano il recall. WQ-7 pubblica la baseline nuova nel piano A1. La soglia CI `--min-recall-5 0.75` resta il pavimento anti-disastro e si rialza se la baseline nuova è larga.
4. **Compatibility level 4 dal primo boot.** `crates/ibis-db/src/schema.rs` deve essere legale su un database creato dalla 4.0 (tabelle vuote, `DEFINE … IF NOT EXISTS`). Ogni cambio di campo successivo, a tabella popolata, usa `MIGRATE EXISTING ROWS`.
5. **Gli id composti non passano dal wrapper 3.0.** `crates/ibis-db/src/ids.rs` formatta gli id con angoli `⟨⟩` per la compatibilità SurrealDB 3.0. `source_file` e `file_chunk` diventano id array; quel wrapper non si estende a loro.
6. **CLI del binario scaricato.** `crates/ibis-db/src/process/mod.rs` lancia `surreal start`. Sul binario GitHub la forma stabile è `surreal db start` (`start` da solo è deprecato e il warning finisce in `surreal.log`). Dentro l’immagine ufficiale il comando resta `start`: il job CI non copia il launcher. Il probe versione passa a `surreal db version --json`. `parse_version_output` in `crates/ibis-db/src/process/install.rs` assume `surreal <versione>`; un formato diverso, con `db_auto_update: true` in `config_master.json`, riscarica a ogni boot.
7. **Asset Windows.** La release 4.0.0 pubblica `surreal-v4.0.0.windows-amd64.exe` (non più uno zip). `install_asset` sposta già un `.exe`; il test degli asset in `install.rs` va allineato. Il timeout di download è 5 minuti, il binario è ~274 MB: va verificato.

## Primitive che entrano in questo stream

### DiskANN, e B5 si chiude qui

Le cinque tabelle embedding in `schema.rs` (`file_chunk`, `commit_chunk`, `error_type`, `memory`, `reasoning`) passano da `HNSW … DIST COSINE` a DiskANN. La 4.0 cammina codici a 8 bit e chiude con un rerank esatto. È la quantizzazione che B5 voleva come colonna int8 più scorer Rust: l’engine non ha distanza di Hamming, e quella colonna non si scrive. B5 esce dalla coda.

Tipo memorizzato: probe live di `array<f16>`. Se `DEFINE INDEX` DiskANN rifiuta `f16`, il campo è `array<f32>` e la quantizzazione resta quella dell’indice. Non si parte da `f32` per prudenza.

### Tipi nominati

`DEFINE TYPE` per le unioni oggi scritte come `ASSERT $value IN […]`: `memory.kind`, `memory.status`, `memory.visibility`, `batch_status`. I campi referenziano il nome. L’ordine della union è l’ordine di sort.

### Id array

Parti nominate dove la chiave è composta e le letture sono per prefisso:

- `source_file` — `[repo, path]`
- `file_chunk` — `[file, start_line]`

`commit` (hash), `memory` e `issue` (numero) restano id scalari: la chiave è già atomica. Le lookup per path diventano range sulla chiave. Gli indici secondari che duplicano quella chiave (`file_path`, `file_version`) cadono.

### Canale lessicale

Indice full-text su `memory` (title + content) e `file_chunk.content`. Il rango è `ORDER BY search::score(1) DESC LIMIT k`. Sostituisce `string::contains` in `crates/ibis-search/src/memory/mod.rs`. È un canale della fusione.

### Fusione, e A2 si chiude qui

Si rimisura `ibis eval retrieval` (offline e, se Ollama c’è, ollama) con i canali DiskANN più il canale lessicale. Si rituna RRF (k e pesi) su quella baseline. Il vincitore diventa `search.fusion`. `additive` non resta il default per inerzia: sulla 3.3.5 era già dominato dalla scala del canale, e un quinto canale lo aggrava. Il verdetto e i numeri nuovi si scrivono in `docs/planning/2026-11-02-a1-retrieval-eval-plan.md` e in una nota accanto a `docs/planning/2026-11-03-a2-rrf-validation.md`.

### Grafo materializzato

`REFERENCE … MATERIALIZE INTO` su ogni arco che il read path attraversa: `supersedes`, `is_type_of`, `related_to`, `authored`, `implements`, `changed`, `calls`, `contains`, `has_memory`, `has_reasoning`, `has_commit_chunk`. Il costo di write in ingest è il prezzo della feature. `recent_failures`, `why_changed`, blast radius e dead code leggono la lista sul record.

### Trace e finestra candidati

Il `query` RPC con `trace` sugli strumenti di grafo (`analyze_blast_radius`, `why_changed`, expand della memoria). La risposta cita le relazioni percorse. E15, dopo, definisce solo come mostrarla.

Una finestra candidati (knob, sopra 15) è condivisa dal rescore temporale e dal reranker A3. A3 non riallargherà il `LIMIT` da sola.

Sul client WebSocket, risultati shaped (`application/vnd.surrealdb.shaped` o subprotocol `shaped`) se il crate 4.0 li espone. I payload grossi sono chunk e memorie.

## Accettazione

- `cargo test` del workspace e clippy `-D warnings` sul crate toccati.
- `ibis eval retrieval` sul dataset golden, embedder offline, contro sidecar `v4.0.0`. Report committato come baseline nuova. Soglia CI aggiornata solo se 0.75 non è più un pavimento onesto.
- Job `retrieval-eval` e `deploy/Dockerfile` pinnati a `v4.0.0`, stesso numero del crate.
- Uno smoke dello spawn: `surreal db start` sul binario GitHub, `start` nell’immagine ufficiale, probe `--json` che non riscarica a versione uguale.

## Cosa non si rifà negli stream dopo

| Stream | Dopo WQ-7 |
|---|---|
| WQ-3 R7 | L’immagine certificata è `v4.0.0`. Il gate non parte sull’immagine 3.3.5. |
| WQ-5 A2, B5 | Assorbiti. Il passo successivo di WQ-5 è C9. |
| A3 reranker | Consuma la finestra candidati di questo stream. |
| A4 query expansion | Due embedding diventano due ricerche DiskANN. La specifica non cambia. |
| C7 ontologia | Estende i `DEFINE TYPE` e gli archi già materializzati. Non ridisegna gli id e non reintroduce `ASSERT` su kind/status/visibility. |
| C8 observations | Le firme entity-pair si leggono dalle liste materializzate, `calls` e `changed` compresi. |
| C9 entity resolution | Il vicino più prossimo usa `ORDER BY vector::distance::cosine` più il canale full-text per gli alias. |
| B6 | Se arriva, memorizza `array<f16>` o `array<f32>` a seconda del probe. |
| D10–D13 | Nessun aggancio all’engine. |
| WQ-4 D1 / D3 | D1 pinna l’immagine 4.0. D3 confronta l’agente con la baseline A1 di questo stream. |
| WQ-6 Track B | Le tabelle Beacon, quando B3 viene promossa, copiano id array dove la chiave è composta, DiskANN, full-text, `DEFINE TYPE` e archi materializzati. |
