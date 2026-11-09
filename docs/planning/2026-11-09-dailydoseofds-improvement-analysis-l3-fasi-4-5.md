# Analisi funzionale L3 — fasi 4–5 (C8, E14, E15)

**Data:** 2026-11-09
**Predecessori:**
1. [2026-11-01-dailydoseofds-improvement-analysis.md](./2026-11-01-dailydoseofds-improvement-analysis.md) — interventi C8, E14, E15
2. [2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md](./2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md) — §1.3, §1.5, §2.4, §2.7
3. [2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md) — formato e stile delle fasi 0–3
4. [2026-11-02-master-work-queue.md](./2026-11-02-master-work-queue.md) — WQ-5 (stato)

**Stato:** fasi 0–3 complete (ultimo commit `d74b63e`). Questo documento specifica le fasi 4–5 limitatamente a C8, E14 ed E15.

**Aggiornamento 09/11 (sera):** **C8 step 1–4** ed **E15** implementati in run parallela (core deterministico + trust/citazioni; zero LLM nei due stream). Unit test e live test verdi (incluse idempotenza/split/merge e read-only end-to-end); fixture discovery rigenerata. Restano **E14** (consumatore: budget/no-progress/completion, config `search.agentic.*`) e **C8 step 5–6** (sintesi LLM `local:M`, hook `refresh_stale_after_ingest`, config `ai.observations`, eval observations + gate A1).

**Review fixes 09/11 (notte):** chiusi i finding della review: binding raw dei record `source_file` array-key per la famiglia `symbol`; protezione overflow/estrazione troncata (niente archive/supersede/reactivate su finestre parziali); `valid_until = NONE` e `confidence` aggiornata nelle transizioni strutturali; embedding retry sul contenuto LLM salvato; quoting sicuro dei target `derived_from` (`issue:⟨IBIS-42⟩`) e escaping del blocco `EVIDENCE:`; `CITATION_PROTOCOL` E14.5 e `stop_reason=MaxSteps`; live suite serializzata con copertura symbol/issue/split/merge/reactivation. `cargo fmt`, `cargo test --workspace` e `cargo clippy --workspace --all-targets -- -D warnings` verdi.

**E14 done 09/11 (dopo la review):** implementato il consumatore (branch `feat/e14`, commit `b0328b6`) sopra l'E15 revisionato: config `search.agentic.*` con clamp/warn (wiring `state.rs`/`main.rs`, chiavi documentate in `ai_and_settings.md`); budget a token grezzi con riserva output 2000 e costo registrato anche senza usage; no-progress su query normalizzata (un nudge actionable, poi `no_progress`; A/B/A non è un doom loop); completion check `off`/`warn`/`enforce` che consuma `assess()` senza ricalcolare la coverage, con un solo repair turn sullo stesso route `local:L` (miglior coverage vince, tie → originale); short-circuit query vuota; AI error best-effort dopo ≥1 step (`ai_error`/`context_only`); freno DB `search_unavailable`; ripristinata la test estate del parser (63 test agentic). Verifiche: `cargo fmt`, `cargo test --workspace` e `cargo clippy --workspace --all-targets -- -D warnings` verdi; baseline A1 invariata (21/21, MRR 0.952 / r@5 0.976 / r@15 1.0). Resta **C8 step 5–6**.

**Struttura:** due specifiche prodotte in parallelo e riconciliate in questo merge:
- **Parte I — C8** (indipendente): pattern recognition Observations deterministico.
- **Parte II — E14 + E15** (accoppiati): E15 produce citazioni/trust/coverage, E14 li consuma per budget, no-progress e completion check.

**Metodo:** ogni affermazione sul flusso attuale è ancorata a codice letto in prima persona al commit `d74b63e`. I numeri di riga sono verificati a quel commit; il codice resta la fonte di verità. Formato per intervento: **Aggancio → Flusso attuale → Trasformazione (numerata) → Casi limite → Test → Accettazione**.

---

# Parte I — C8: Observations deterministiche

---

## 0. Ancoraggi verificati

| Ancoraggio | Fatto verificato |
|---|---|
| `crates/ibis-db/src/schema.rs:117` | `DEFINE TYPE memory_kind` include già `observation` (10 valori). |
| `crates/ibis-db/src/schema.rs:239-284` | Tabella `memory` SCHEMAFULL: `details` FLEXIBLE (`:250`), `store` (`:275`), `policy_applied` (`:276`), `provenance.agent` (`:258`), `provenance.tool` (`:260`), `embedding array<f16>` (`:277`). |
| `crates/ibis-db/src/schema.rs:476-480` | `derived_from` è RELATION con `in record<memory>` e `out record<source_file \| file_chunk \| commit \| issue>`: **memory non è un target valido**. Il campo `out` è `REMOVE`+`DEFINE` a ogni boot, quindi estendere l'unione è effettivo anche su DB già creati. |
| `crates/ibis-db/src/schema.rs:408-412` | `has_memory` repo→memory, materializzato su entrambi i lati. |
| `crates/ibis-db/src/schema.rs:470-474` | `supersedes` memory→memory, materializzato su `supersedes_edges` / `superseded_by_edges`. |
| `crates/ibis-db/src/schema.rs:386-392` | `implements` commit→issue (`in`=commit, `out`=issue), materializzato. |
| `crates/ibis-db/src/schema.rs:371-379` | `changed` commit→source_file con `impact`/`added`/`deleted`. |
| `crates/ibis-db/src/schema.rs:425-431` | `calls` symbol→symbol con `file`/`line`, materializzato. |
| `crates/ibis-db/src/schema.rs:344-346` | Tabella `symbol`: dichiara **solo** `name` e `file`; **`kind` non è dichiarato**. |
| `crates/ibis-ingest-code/src/ingest.rs:773-776` | L'ingest scrive `UPSERT symbol SET name, kind, file` + `RELATE file->contains->symbol`: scrive un campo non dichiarato (rischio latente pre-esistente, non di C8). |
| `crates/ibis-ingest-code/src/ingest.rs:753` | L'ingest scrive `file_chunk.symbol_name` (dichiarato in `schema.rs:168`): è la fonte affidabile per la firma simbolo. |
| `crates/ibis-db/src/schema.rs:461-467` | DiskANN su `memory.embedding`; FULLTEXT BM25 su `memory.title`/`memory.content`. |
| `crates/ibis-db/src/schema.rs:483-491` | Indici: `memory_kind`, `memory_status`, `memory_visibility`, `memory_project`, `memory_store (project, store)`. |
| `crates/ibis-search/src/memory/mod.rs:66-68` | `STORE_GIT = "git"`, `STORE_SURREAL = "surreal"`. |
| `crates/ibis-search/src/memory/mod.rs:74-110` | Il modello `Memory` espone `store` e `details`; **non** espone `provenance`. |
| `crates/ibis-search/src/memory/mod.rs:391-401` | `update_memory` rifiuta le modifiche ai claim git-owned (`store == 'git'`): è il precedente esatto del guard read-only. |
| `crates/ibis-search/src/memory/mod.rs:536-558` / `:562-596` | `delete_memory` (soft) e `memory_delete` (hard) con guard git-owned. |
| `crates/ibis-search/src/memory/mod.rs:643` | `list_memories` proietta `store` e `details` (il VSIX pre-disabilita i git-owned su `store`, `:638-641`). |
| `crates/ibis-search/src/memory/mod.rs:700` | `search_memory` richiede `embedding != NONE`. |
| `crates/ibis-search/src/memory/mod.rs:780` | `search_memory` proietta `details` ma **non** `store`. |
| `crates/ibis-search/src/memory/mod.rs:1132-1146` | `authorize_memory_mutation`: owner-only per personal e team. |
| `crates/ibis-search/src/memory/add.rs:104-117` | Governance della supersession: personal owner-only. |
| `crates/ibis-search/src/memory/add.rs:164-182` | Store decision + divieto git→surreal + **stesso kind** per `supersedes`. |
| `crates/ibis-search/src/memory/add.rs:303-324` | Transazione: `UPSERT repo` → `CREATE memory` → `RELATE has_memory` → `RELATE derived_from` → `RELATE supersedes` + update old. |
| `crates/ibis-search/src/memory/claim_store.rs:53-70` | `claim_store_decision`: git solo per `decision|rule|fix`; `observation` → **sempre surreal**. |
| `crates/ibis-search/src/memory/claim_store.rs:312-327` | `policy_applied` è il livello della personal-claims policy (`stub`/`in_repo`), non un marker di versione. |
| `crates/ibis-search/src/memory/capture.rs:40-55` | `capture_observation` scrive `kind=observation`, `visibility=team`, `confidence=0.5`, `provenance.tool=capture_observation`. |
| `crates/ibis-search/src/memory/insights.rs:73-205` | `memory_insights`: clustering kind+primo tag, split opzionale per cosine 0.82, label LLM `local:S`; nessuna persistenza. |
| `crates/ibis-search/src/memory/lifecycle.rs:164-301` | `refresh_stale_memories_for_repo`: candidati da `provenance.source_hash/source_path` + fallback `derived_from`→source_file filtrato per prefisso `source_file:` (`:229`). |
| `crates/ibis-search/src/memory/lifecycle.rs:308-370` | `promote_dirty_anchors_for_repo`: solo `promote_on_commit = true`. |
| `crates/ibis-search/src/memory/gitbound.rs:248-316` | `refresh_orphaned_git_bounds`: solo `introduced_in_oid != NONE`. |
| `crates/ibis-search/src/memory/gitbound.rs:227-242` | `supersede_old_set` scrive `invalidated_at_oid = HEAD`: non riusabile per righe derivate. |
| `crates/ibis-optimization/src/memory_consolidate.rs:160-296` | `consolidate_loop`: drain oldest-first, una osservazione per volta, dedup, distill, persist, noise decay. |
| `crates/ibis-optimization/src/memory_consolidate.rs:345-347` | `select_observations`: `kind='observation' AND status='current'` — **nessun filtro `store`**. |
| `crates/ibis-optimization/src/memory_consolidate.rs:311-337` | `pending_observation_projects`: stessa WHERE, usata dal daemon per decidere se girare. |
| `crates/ibis-optimization/src/memory_consolidate.rs:424-428` | `find_duplicate_memories` esclude già `kind != 'observation'`. |
| `crates/ibis-optimization/src/memory_consolidate.rs:31-32` | Hint D10: `local:M` (team) / `local_only:M` (personal). |
| `crates/ibis-optimization/src/memory_consolidate.rs:723-749` | `normalize_title`: lowercase, separatori path→spazio, collapse spazi, strip punteggiatura finale (C9). |
| `crates/ibis-optimization/src/memory_consolidate.rs:846-860` | `DISTILL_INSTRUCTIONS` byte-stabile + `build_distill_prompt` (D13). |
| `crates/ibis-server/src/assembly.rs:317-363` | `refresh_stale_after_ingest`: invalidate_ancestors → index_claim_store → refresh_stale_memories → refresh_orphaned_git_bounds → promote_dirty_anchors → prune_file_at_commit_cache. |
| `crates/ibis-server/src/consolidation.rs:78-117` | Pass daemon: `pending_observation_projects` → `consolidate_loop` per progetto (20 item), poi `maybe_project_memory`. |
| `crates/ibis-server/src/tools_memory.rs:938-978` | `memory_consolidate` MCP: spawn detached, knob C9/C7/D11 iniettati nell'`Optimizer`. |
| `crates/ibis-server/src/registry.rs:365-371` | Tool `memory_consolidate` (project_name, max_items). |
| `crates/ibis-server/tests/tool_contract_test.rs:91-102` | Il registry registra **esattamente 58** tool. |
| `crates/ibis-server/tests/discovery_snapshot_test.rs:198-213` | Il fixture `tests/fixtures/discovery_tools.json` va rigenerato a ogni cambio di descrizioni (comando documentato). |
| `crates/ibis-ai/src/types.rs:45-66` | `ResponseCacheCallSite` ha **solo** 3 varianti (`LogAnalyze`, `RuleSynth`, `AstRuleGen`): D12 non è opt-in da C8. |
| `crates/ibis-ai/src/router.rs:92-99` | `local:<tier>` → provider del tier poi cloud; `local_only:<tier>` → tier, mai cloud. |
| `crates/ibis-core/src/config.rs:182-210` | `ai.consolidation` (opt-in, off di default). |
| `crates/ibis-search/src/memory/projection.rs:14` | `.ibis/memory/.gitignore` ignora già `observation/` e `working_state/`. |
| `crates/ibis-search/src/memory/export.rs:51-68` | `memory_export`: WHERE status+visibility(+kind/project), **nessun filtro `store`**. |
| `crates/ibis-search/src/memory/import.rs:68-126` | `memory_import` ricrea via `add_memory` con `provenance_tool=memory_import` e **non** copia `store`. |

---

## 1. Sintesi delle decisioni (le 8 domande chiuse)

| # | Domanda | Decisione |
|---|---|---|
| 1 | Firma deterministica | 4 famiglie (`fix_root_cause`, `issue`, `symbol`, `file`) su tuple normalizzate; `signature_hash = sha256("ibis.observation.v1\0project\0scope\0family\0key")[..32]`; 7 query SurrealQL di estrazione, zero LLM. |
| 2 | Connected components | Union-find bipartito memoria↔firma; ordinamento totale per (priorità famiglia, chiave); min 2 membri, hub ≤ 10, cap 12 membri/componente, 25 cluster/repo, 500 memorie esaminate; grafo vuoto → 0 cluster; DB degradato → errore non distruttivo. |
| 3 | Persistenza | `kind=observation`, `status=current|stale`, `visibility` per scope, `store=derived`, `policy_applied=observation.v1`, `provenance.agent=ibis.observation_engine`, `provenance.tool=memory_observations`, 11 chiavi `details`, edge `derived_from`→memory (estensione unione) + `has_memory`, embedding alla sintesi. |
| 4 | Read-only | `store = 'derived'`: estende l'asse SoT/mutabilità già usato per `git`, non un campo nuovo né `provenance`. Guard in update/delete/share/supersede/import; hard delete consentito solo a un attore locale fidato (manutenzione). |
| 5 | Rigenerazione | `reconcile_observations` deterministico nell'hook ingest (dopo `promote_dirty_anchors`, prima di `prune_file_at_commit_cache`) in modalità `InvalidateOnly`; sintesi LLM solo nel pass di consolidamento. Idempotenza: `signature_hash`→id, `evidence_hash`→skip LLM. Split/merge con `supersedes`; nessun TTL. |
| 6 | LLM summary | `local:M` / `local_only:M`, temp 0.0, 512 token, prompt D13; il modello scrive **solo** name+summary, la membership è nostra; validazione id estranei; fallback deterministico; costo su `system:memory_observations`, ≤ 10 sintesi/pass. |
| 7 | Rapporto con consolidate/insights | Estensione di `consolidate_loop` (nessun nuovo tool → contratto resta 58); `memory_insights` resta la vista similarity e viene disaccoppiata (esclude `store='derived'`); due esclusioni obbligatorie in `select_observations`/`pending_observation_projects`. |
| 8 | Superficie di lettura | `memory_list(kind=observation, store=derived)` + `store` nella projection di `memory_search`; `memory_expand` funziona già; export esclude i derivati; nessun nuovo tool; payload additivo. |

---

## C8 — Pattern recognition Observations deterministico

### Aggancio

- **Grafo**: `derived_from` (memory→source_file|file_chunk|commit|issue, `schema.rs:476-480`), `implements` (commit→issue, `:386-392`), `changed` (`:371-379`), `calls` (`:425-431`), `has_memory` (`:408-412`), `supersedes` (`:470-474`).
- **Memoria tipizzata**: `details` FLEXIBLE (`schema.rs:250`) con la rubrica C7 (`crates/ibis-search/src/memory/details.rs:62-70`: `fix → root_cause`).
- **Consolidamento**: `Optimizer::consolidate_loop` (`memory_consolidate.rs:160-296`) e il pass daemon (`consolidation.rs:78-117`).
- **Lifecycle post-ingest**: `refresh_stale_after_ingest` (`assembly.rs:317-363`).
- **Governance**: `store` git/surreal (`mod.rs:66-68`), owner-only (`mod.rs:1132-1146`), immutabilità git-owned (`mod.rs:391-401`).
- **Lettura**: `list_memories` (`mod.rs:599-658`), `search_memory` (`mod.rs:662-838`), `memory_expand` (`mod.rs:372-378` restituisce `derived_from`), canale memory di `ask_project` (`search.rs:403-441`).
- **Routing/costi**: `local:M`/`local_only:M` (`memory_consolidate.rs:31-32`), `CostTracker` con bucket `system:*` (`memory_consolidate.rs:519-532`).
- **Eval**: `RetrievalEvalRunner` e dataset `evaluations/retrieval/dataset.yaml` (A1).

### Flusso attuale

1. `consolidate_loop` drena le osservazioni grezze **una alla volta**, oldest-first (`memory_consolidate.rs:175-177`, `select_observations` `:339-391`).
2. Per ciascuna: `find_duplicate_memories` fa top-3 cosine sulle memorie **dello stesso topic** (`:393-475`), con soglia C9 (`:469-473`) e governance personal/team bindata (`:406-423`).
3. `distill_observation` (`:477-577`) produce **una** memoria tipizzata (`create|merge|noise`), con `details` C7 validati (`:553-575`).
4. `persist_consolidation` (`:624-696`) archivia l'osservazione sorgente e collega `supersedes`.
5. **Non esiste alcuno stadio cross-memoria**: la domanda "cosa collega questi 3 fix?" non ha un artefatto che la rappresenti. `find_duplicate_memories` confronta una osservazione con memorie simili, mai due memorie durevoli tra loro.
6. `memory_insights` (`insights.rs:73-205`) raggruppa per kind+primo tag, con split cosine opzionale (`:286-314`) e label LLM (`:207-247`), ma **non persiste nulla** e non usa il grafo: è una vista di similarità, non di struttura.
7. Il grafo che servirebbe è già materializzato (tabella §0) ma nessun job lo attraversa per produrre conoscenza.
8. Il lifecycle post-ingest (`assembly.rs:324-361`) aggiorna staleness/anchors ma non ha un passo "deriva/rigenera osservazioni".
9. La governance ha già un asse di immutabilità (`store=git`, `mod.rs:391-401`) e la browse UI lo consuma (`mod.rs:638-641`), ma non esiste un valore per "riga derivata da altre memorie".

**Conseguenza verificata (bug di interazione da chiudere in C8):** una volta introdotte righe `kind=observation AND store=derived`, `select_observations` (`memory_consolidate.rs:345-347`) e `pending_observation_projects` (`:313`) le tratterebbero come osservazioni grezze: il consolidamento le distillerebbe di nuovo e il daemon le userebbe come backlog perpetuo. Servono due esclusioni esplicite.

**Vincolo di schema verificato:** `derived_from.out` non include `memory` (`schema.rs:480`), quindi l'edge "osservazione→memoria sorgente" non è oggi esprimibile.

### Trasformazione

#### 4.1 Modello: famiglia, firma, componente, evidence hash

Definizioni (tutte deterministiche, nessun LLM):

- **Famiglia** (`Family`): l'asse strutturale del pattern. Quattro in v1:
  - `fix_root_cause` — `fix` con lo stesso `details.root_cause` normalizzato (dipendenza C7).
  - `issue` — memorie ancorate a commit che implementano la stessa issue (`commit→implements→issue`).
  - `symbol` — memoria ancorata a un file che contiene un simbolo (`file_chunk.symbol_name`) il cui nome compare come token nel titolo/contenuto della memoria.
  - `file` — memorie ancorate allo stesso `source_file` (path repo-relative).
- **Firma** (`Signature`): coppia `(family, key)` con `key` normalizzata. È il "nodo entità-coppia" del pattern Zep.
- **Componente** (`Component`): connected component del grafo bipartito memoria↔firma. È il cluster che diventa **una** osservazione.
- **Anchor signature**: la firma che identifica la componente, scelta con un ordine totale: `family_priority(fix_root_cause=0, issue=1, symbol=2, file=3)` poi `key` lessicografica. Se una firma di priorità più alta entra nella componente, l'anchor cambia e la riga viene sostituita (split/merge, §4.7).
- **evidence_hash**: hash del contenuto semantico dell'evidenza (non di `updated_at`).

#### 4.2 Estrazione deterministica (SurrealQL)

Le query sono **proiezioni**: famiglia, normalizzazione, union-find e hash girano in Rust (deterministici e unit-testabili). L'unico filtro SQL è quello espresso. `$kinds` è un array bindato (precedente: `insights.rs:101-110`); `<max>` è un intero clampato interpolato (precedente: `memory_consolidate.rs:357`); `repo:⟨…⟩` è costruito da `repo_record_id()` (`mod.rs:173-178`). Se un array bindato è vuoto (`$memory_ids`, `$commits`, `$files`, `$issues`), la query corrispondente viene saltata (un `IN []` non deve mai essere interpretato come "tutto").

Scope: ogni query riceve il predicato di scope — `visibility = 'team'` per lo scope team, `visibility = 'personal' AND owner = $owner` per lo scope personale.

**Bounded set:** Q1 è l'unica query che scandisce `memory` e restituisce al più `max_members_examined` (500) id. Q2/Q3 sono limitate a `$memory_ids` (il set di Q1); Q4 a `$commits` (da Q3); Q5 a `$issues` (da Q4); Q6 a `$files` (da Q2). Nessuna query attraversa l'intero progetto: il costo del reconcile è lineare nel set bounded, non nella dimensione dello store.

```sql
-- Q1: memorie eleggibili (una riga per memoria, con details C7 e ancore denormalizzate).
SELECT id, kind, title, content, confidence, visibility, owner,
       details,
       details.root_cause AS root_cause,
       provenance.source_path AS source_path,
       provenance.introduced_in_commit AS introduced_in
FROM memory
WHERE project = repo:⟨<project_key⟩>
  AND status = 'current'
  AND kind IN $kinds                       -- decision, fix, constraint, rule, gotcha, design, fact, reasoning
  AND (store = NONE OR store != 'derived') -- NONE incluso: `!=` da solo non matcha NONE
ORDER BY id
LIMIT <max_members_examined>;              -- 500

-- Q2: archi memoria -> source_file (out è union: discrimino per `path`,
-- poi ricontrollo il prefisso `source_file:` in Rust — precedente lifecycle.rs:221-231).
SELECT in AS memory_id, out AS file_id, out.rel_path AS rel_path, out.path AS path
FROM derived_from
WHERE in IN $memory_ids                    -- bounded dal set di Q1
  AND out.path != NONE
ORDER BY memory_id, rel_path;

-- Q3: archi memoria -> commit.
SELECT in AS memory_id, out AS commit_id, out.date AS commit_date
FROM derived_from
WHERE in IN $memory_ids                    -- bounded dal set di Q1
  AND out.date != NONE
ORDER BY memory_id, commit_id;

-- Q4: commit -> issue (implements, in=commit out=issue — schema.rs:386-392).
SELECT in AS commit_id, out AS issue_id
FROM implements
WHERE in IN $commits                       -- array bindato, limitato dagli archi di Q3
ORDER BY commit_id, issue_id;

-- Q5: chiavi canoniche delle issue.
SELECT id, provider, external_key FROM issue WHERE id IN $issues;

-- Q6: simboli per file. Fonte: file_chunk.symbol_name (dichiarato schema.rs:168,
-- scritto ingest.rs:753). NON uso la tabella `symbol`: schema.rs:344-346 non
-- dichiara `kind`, che l'ingest scrive (ingest.rs:773-776) — dipendenza non sicura.
SELECT file AS file_id, symbol_name, start_line, end_line
FROM file_chunk
WHERE file IN $files
  AND symbol_name != NONE
  AND symbol_name != ''
ORDER BY file_id, start_line;

-- Q7: osservazioni derivate esistenti (diff di riconciliazione).
SELECT id, status, details.signature_hash AS signature_hash,
       details.evidence_hash AS evidence_hash, details.members AS members,
       details.family AS family, confidence, owner, visibility
FROM memory
WHERE project = repo:⟨<project_key⟩>
  AND kind = 'observation'
  AND store = 'derived'
ORDER BY id;
```

Se una qualsiasi query fallisce, l'intero pass abortisce con `Err` (nessuna estrazione parziale interpretata come "evidenza mancante", §5).

#### 4.3 Normalizzazione, tupla e hash

```text
normalize_key(s):
  lowercase → trim → collapse whitespace → strip punteggiatura finale [.!?:;,]
  → cap 200 rune
  (stesse regole di C9, memory_consolidate.rs:723-749, senza la sostituzione
   dei separatori path: qui i path restano leggibili)

normalize_path(p):
  replace '\\' → '/' → strip "./" iniziale → normalize_key

normalize_symbol(s):
  lowercase → trim   (il matching CamelCase avviene sui token, non sulla chiave)
```

Tuple delle firme:

| Famiglia | Firma | Eleggibilità |
|---|---|---|
| `fix_root_cause` | `fix_root_cause\|<normalize_key(details.root_cause)>` | `kind='fix'`, `root_cause` presente e non in stoplist |
| `issue` | `issue\|<provider_lower>#<external_key_trim>` | esiste `memory→commit` (Q3 **oppure** `provenance.introduced_in` da Q1) e `commit→issue` (Q4), `external_key` non vuoto |
| `symbol` | `symbol\|<normalize_path(rel_path)>#<normalize_symbol(symbol_name)>` | esiste un `derived_from`→source_file e un `file_chunk` del file il cui `symbol_name` compare come token in titolo/contenuto |
| `file` | `file\|<normalize_path(rel_path)>` | esiste `derived_from`→source_file **oppure** `provenance.source_path` non vuoto |

Stoplist deterministica per `root_cause` (evita hub banali): `["", "none", "n/a", "na", "unknown", "tbd", "misc", "various"]` dopo `normalize_key`.

Token matching per `symbol`: si riusa la tokenizzazione di `retrieval_boost::tokenize_query` (`retrieval_boost.rs:238-274`, CamelCase split, lowercase) sul testo `title + " " + content` e si confronta con i token del simbolo; match se **tutti** i token del simbolo compaiono.

Hash (una sola definizione, 32 hex = 128 bit, sufficiente a scala repo):

```text
signature_hash = hex(sha256(
  "ibis.observation.v1\0" + project_key + "\0" + scope + "\0" + family + "\0" + key
))[..32]

scope = "team" | "personal:" + sanitize_id(owner)
project_key = sanitize_id(project_name)
```

`signature_hash` è **identità** della riga: `memory:⟨obs_<signature_hash>⟩`. Un cambio di normalizzazione si gestisce con un bump di versione (`v2`), che rigenera tutto una volta — coerente con il greenfield 4.0 (nessuna migrazione dati).

```text
evidence_hash = hex(sha256(
  "ibis.observation.evidence.v1\0" + canonical_members + "\0" + canonical_signatures + "\0" + canonical_refs
))[..32]

canonical_members  = per membro ordinato per id:
                     id \x1f kind \x1f status \x1f sha256(title) \x1f sha256(content) \x1f sha256(canonical_json(details))
canonical_signatures = firme ordinate, join \x1f
canonical_refs       = evidence_refs ordinate, join \x1f
```

`canonical_json` è un encoder esplicito (chiavi ordinate, nessuno spazio) — non si dipende dall'ordine delle mappe di `serde_json`/`surrealdb`.

#### 4.4 Connected components

**Algoritmo: union-find (disjoint set)** su nodi stringa `m:<memory_id>` e `s:<signature>`, con path compression. Motivazione: l'ordine di traversal non influenza il risultato (BFS dipenderebbe dall'ordine di visita), i merge sono deterministici e la complessità è O((M+S)·α).

Passi:

1. **Hub exclusion (prima dell'unione):** una firma con grado > `hub_degree_max` (10) viene scartata (log `observation_hub_excluded`). Evita che `Cargo.toml`, `root_cause = "unknown"` o un ticket omnibus inghiottano il repo.
2. **Unione:** per ogni memoria in ordine di id, per ogni sua firma in ordine lessicografico, `union(m, s)`.
3. **Raccolta:** per ogni radice, `members` (ordinati per id) e `signatures` (ordinate per `(family_priority, key)`).
4. **Filtri:** componenti con `members.len() < min_cluster_size` (2) scartate; componenti con `members.len() > max_members` (12) **scartate, non troncate** (una osservazione troncata mentirebbe sull'evidenza) con log `observation_component_capped`.
5. **Ordinamento output:** `(member_count desc, anchor_signature asc)`; si persistono i primi `max_clusters_per_repo` (25), il resto è loggato.
6. **Scope:** il pass gira una volta per scope (`team`, poi ogni `personal:<owner>` distinto presente in Q1, cap 8 scope/pass). Nessun cluster misto team/personal né tra owner diversi.

**Grafo vuoto:** Q1/Q2/Q3/Q6/Q7 a 0 righe → 0 componenti. Se Q1 ha 0 memorie eleggibili **e** Q7 ha righe derivate, quelle righe vengono archiviate (l'evidenza non esiste più). **DB degradato** (`db == None`): `Err("database not connected")`, nessuna scrittura, le osservazioni esistenti restano intatte (fail-open non distruttivo).

#### 4.5 Persistenza

Riga (`memory`), valori e motivazione:

| Campo | Valore | Perché |
|---|---|---|
| `id` | `memory:⟨obs_<signature_hash>⟩` | identità deterministica → idempotenza e UPSERT |
| `kind` | `observation` | già nel `memory_kind` (`schema.rs:117`), nessun nuovo tipo |
| `title` | name (≤ 8 parole, ≤ 80 char) | browse/UX |
| `content` | summary + footer deterministico dei membri (§4.8) | il LLM scrive solo la summary; la membership è nostra |
| `status` | `current` / `stale` / `superseded` / `archived` | lifecycle §4.7 |
| `confidence` | `min(0.95, 0.5 + 0.5*(1 - 1/member_count))` arrotondata a 2 decimali | monotona nella dimensione, deterministica, testabile |
| `visibility` | `team` o `personal` (scope) | nessun cluster misto |
| `owner` | `ibis.observation_engine` (team) / owner personale | visibilità e difesa in profondità |
| `tags` | `[family, "observation"]` | filtrabile, family-first per i consumer che usano `tags.first()` |
| `project` | `repo:⟨…⟩` + `RELATE has_memory` | scope di ricerca (`search.rs:762`, `insights.rs:116`) |
| `store` | **`derived`** | read-only (§4.6) |
| `policy_applied` | `observation.v1` | versione del produttore, leggibile senza parsare `details` (non si sovrascrive il significato claim-store, `claim_store.rs:312-327`) |
| `provenance.agent` | `ibis.observation_engine` | audit |
| `provenance.tool` | `memory_observations` | audit + distinzione da `capture_observation` |
| `provenance.source_hash/source_path` | **mai valorizzati** | altrimenti `refresh_stale_memories_for_repo` (`lifecycle.rs:189`) marcherà stale l'osservazione a ogni cambio file, fuori dal controllo di `evidence_hash` |
| `introduced_in_oid` | **mai valorizzato** | `refresh_orphaned_git_bounds` (`gitbound.rs:259`) e `promote_dirty_anchors` (`lifecycle.rs:319`) devono ignorarle per costruzione |
| `valid_until` | `NONE` (current) / `time::now()` (archiviata) | nessun TTL (§4.7) |
| `embedding` | `content` embeddato alla sintesi | §4.8 |

**`details` (C7):** chiavi richieste dalla rubrica `observation`: `signature_hash`, `evidence_hash`, `members`. Perché proprio queste tre: sono le tre invarianti che una osservazione scritta a mano non può fabbricare — identità della firma, identità dell'evidenza, appartenenza. Tutte le altre sono derivate o di comodo.

| Chiave | Tipo | Ruolo |
|---|---|---|
| `signature` | string | forma leggibile `family\|key` (debug/UX) |
| `signature_hash` | string (32 hex) | identità (id) |
| `anchor_signature` | string | firma prioritaria della componente (ordinamento/stabilità) |
| `signatures` | array<string> | tutte le firme della componente ("perché sono insieme") |
| `evidence_hash` | string (32 hex) | gate LLM |
| `family` | string | famiglia prioritaria |
| `members` | array<string> | id dei membri, ordinati |
| `member_count` | number | cap/ordinamento senza ricomputo |
| `evidence_refs` | array<string> | record id del grafo (`source_file:…`, `commit:…`, `issue:…`); sono i target degli edge `derived_from` e entrano nell'hash |
| `rubric` | string | `observation.v1`: versione della rubrica di sintesi (può divergere da `policy_applied` quando cambia il prompt ma non l'estrazione) |
| `generated_by` | string | `llm` \| `fallback` (guida il retry) |

In `details.rs:62-70` la matrice diventa `"observation" => &["signature_hash", "evidence_hash", "members"]`. È sicuro: `validate_memory_details` passa su `details` nullo (`details.rs:84-87`) e `VALID_CONSOLIDATION_KINDS` non include `observation` (`memory_consolidate.rs:46-56`), quindi né `capture_observation` né il distill possono rompersi.

**Edge:**

- `derived_from` osservazione→membro. Richiede l'estensione dell'unione: `DEFINE FIELD out ON TABLE derived_from TYPE record<source_file | file_chunk | commit | issue | memory>` (`schema.rs:480`). È l'unico cambio di schema. Sicuro perché: il campo è già `REMOVE`+`DEFINE` a ogni boot (`schema.rs:477-480`), `refresh_stale_memories_for_repo` filtra `source_file:` (`lifecycle.rs:229`), `why_changed` filtra `in.kind` su 6 kind senza `observation` (`queries.rs:233-236`), `memory_expand` proietta già `derived_from` (`mod.rs:372-378`).
- `derived_from` osservazione→source_file/commit/issue (le evidenze, quando presenti). Unione di `evidence_refs`.
- `has_memory` repo→osservazione, una sola volta (delete+relate nel reconcile, §4.7).
- `supersedes` solo per split/merge (§4.7).

**Transazione di persist (scheletro):**

```sql
BEGIN TRANSACTION;
UPSERT repo:⟨<project_key⟩> SET name = $repo_name;
UPSERT memory:⟨obs_<signature_hash>⟩ SET
  kind = 'observation', title = $title, content = $content, status = 'current',
  confidence = $confidence, visibility = $visibility, owner = $owner, tags = $tags,
  project = repo:⟨<project_key⟩>, store = 'derived', policy_applied = 'observation.v1',
  provenance.agent = 'ibis.observation_engine', provenance.tool = 'memory_observations',
  details.signature = $signature, details.signature_hash = $signature_hash,
  details.anchor_signature = $anchor_signature, details.signatures = $signatures,
  details.evidence_hash = $evidence_hash, details.family = $family,
  details.members = $members, details.member_count = $member_count,
  details.evidence_refs = $evidence_refs, details.rubric = 'observation.v1',
  details.generated_by = $generated_by, embedding = $embedding,
  valid_until = NONE, updated_at = time::now();
DELETE has_memory WHERE in = repo:⟨<project_key⟩> AND out = memory:⟨obs_<signature_hash⟩⟩;
RELATE repo:⟨<project_key⟩>->has_memory->memory:⟨obs_<signature_hash⟩⟩;
DELETE derived_from WHERE in = memory:⟨obs_<signature_hash⟩⟩;
RELATE memory:⟨obs_<signature_hash⟩⟩->derived_from-><ref>;   -- per ogni evidence_ref
COMMIT TRANSACTION;
```

`created_at` non compare nel SET: `UPSERT` su riga esistente non lo tocca, su riga nuova applica il DEFAULT (`schema.rs:278`). I valori di `details` viaggiano su path puntati con variabili bindate (precedente: `add.rs:263-273`), non su JSON interpolato.

**Embedding:** lo calcola solo la fase di sintesi, una volta per rigenerazione, con lo stesso contratto di `SearchService::memory_embedding` (`add.rs:334-353`) → `ai.embed_text`. Nel crate `ibis-optimization` non c'è un `SearchService`: usare `Optimizer.ai.embed_text` (stesso provider/route) con la cache B6 trasparente. L'indice DiskANN `memory_embedding` (`schema.rs:461`) copre la nuova riga senza alcun DDL aggiuntivo. Con B6 attivo la cache L2 (`schema.rs:326-334`, wiring `state.rs:325-349`) evita il ricalcolo quando la summary rigenerata è identica byte per byte. Costo: 1 embedding locale per cluster rigenerato; 0 nel reconcile. Se l'embedding fallisce, la riga si persiste con `embedding = NONE` (visibile a `memory_list` e al canale lessicale FULLTEXT, invisibile a `memory_search` che richiede `embedding != NONE`, `mod.rs:700`) e la sintesi successiva la ritenta (regola §4.7).

#### 4.6 Read-only: la decisione centrale

**Decisione: `store = 'derived'`** (terzo valore dell'asse SoT, accanto a `git`/`surreal`).

Alternative valutate:

| Opzione | Pro | Contro | Esito |
|---|---|---|---|
| **A. Campo dedicato `read_only: bool`** | esplicito | richiede `DEFINE FIELD` + ogni writer lo deve settare; un booleano non dice *perché*; non è proiettato in nessuna read path → il client non può pre-disabilitare | scartata |
| **B. Guard su `provenance.agent`/`tool`** | zero schema | `provenance` **non è proiettato** da `memory_from_row` (`mod.rs:113-134`) → invisibile ai client; `memory_import` non lo preserva (`import.rs:68-126`) | scartata da sola (resta come difesa in profondità) |
| **C. `policy_applied` come marker** | zero schema | è già l'audit della personal-claims policy (`claim_store.rs:312-327`): sovraccaricarlo corrompe quel trail | scartata |
| **D. `store = 'derived'`** | zero schema (`option<string>`, `schema.rs:275`); estende l'asse già usato per l'immutabilità git (`mod.rs:391-401`); già proiettato da `memory_list` (`mod.rs:643`) | un terzo valore da documentare; la UI VSIX oggi pre-disabilita solo `store === 'git'` (`memory-tree.js:225-245`) | **scelta** |

Motivazione: `store` significa "qual è la fonte di verità di questa riga". Per un claim git-owned è il tree; per una memoria normale è la riga stessa; per una osservazione è **l'evidenza** che la genera. La mutabilità discende dalla stessa proprietà; il client riceve il marker `store=derived` e può pre-disabilitare, ma la difesa autoritativa sono i guard server (sotto).

Guard (tutti con messaggio actionable, stile `mod.rs:397-400`):

| Superficie | Punto | Comportamento |
|---|---|---|
| `update_memory` | dopo `memory_for_governance` (`mod.rs:390`) | `Err("observation <id> is derived from its evidence; edit the source memories and let the next pass regenerate it")` |
| `delete_memory` (soft) | dopo `mod.rs:544` | `Err(...)` (un archive verrebbe risuscitato dal reconcile: sarebbe confusione) |
| `memory_delete(hard)` | dopo `mod.rs:575` | consentito **solo** a un attore trusted non vuoto e non remote-untrusted (stessa nozione di `ActorContext::resolve_trusted_actor` / `actor_is_remote_untrusted`, `mod.rs:1150-1162`), con `tracing::warn!(target: "observation_manual_delete", …)`: è la via di manutenzione, la riga è rigenerabile |
| `share_memory` | dopo `share.rs:21` | `Err(...)`: la visibilità fa parte dello scope della firma; condividerla romperebbe il partizionamento |
| `add_memory(supersedes=…)` | dopo `add.rs:109` | se il target è derivato → `Err(...)`: solo il motore può sostituire una osservazione (split/merge), e lo fa con la propria transazione, non con `add_memory` |
| `memory_import(on_conflict=supersede)` | eredita `add_memory` (`import.rs:106-123`) | bloccato dalla regola precedente |
| `memory_export` | `export.rs:51-68` | i derivati sono **esclusi** dalla WHERE con `(store = NONE OR store != 'derived')`: una osservazione senza il suo grafo è priva di significato, e l'import la ricreerebbe scrivibile (`import.rs:68-126` non copia `store`) |

Predicato unico, in `memory/mod.rs` accanto a `STORE_GIT`:

```rust
pub const STORE_DERIVED: &str = "derived";
pub(crate) fn is_derived_observation(memory: &Memory) -> bool {
    memory.store == STORE_DERIVED && memory.kind == "observation"
}
```

**Perché non una tabella separata:** osservazioni e memorie condividono ricerca (DiskANN + FULLTEXT), visibilità, `has_memory`, `memory_expand`, projection e governance. Una tabella dedicata forkerebbe l'intero read path per un artefatto che è, per contratto, una memoria.

**Follow-up VSIX (non bloccante):** `memory-tree.js` tratta come read-only solo `store === "git"` (righe 225/233/245/542). Con `store='derived'` la UI non pre-disabilita il menu di modifica, ma i guard server rifiutano comunque l'operazione con un messaggio actionable. L'aggiornamento VSIX (badge “derived” + menu pre-disabilitato) è un follow-up di implementazione, non un requisito di correttezza.

**Impatto MCP:** nessun nuovo tool (contratto resta 58, `tool_contract_test.rs:91-102`). Cambiano le descrizioni di `memory_update` (`registry.rs:248-249`), `memory_delete` (`:265-266`), `memory_share` (`:373`), `memory_export` (`:382`), `memory_consolidate` (`:365-366`) e lo **schema di input di `memory_list`** (nuovo parametro opzionale `store`). Il fixture `discovery_tools.json` va rigenerato per entrambe le variazioni (`discovery_snapshot_test.rs:198-213`).

#### 4.7 Rigenerazione e idempotenza

Due fasi, un solo algoritmo di diff:

```text
reconcile_observations(project, scope, mode) -> ReconcileReport
  extract (§4.2) → signatures (§4.3) → hub filter → union-find (§4.4)
  → diff vs Q7 → azioni {create, refresh, stale, archive, supersede}
mode = InvalidateOnly | Full
```

**Dove gira:**

1. **Hook ingest** — in `refresh_stale_after_ingest`, **dopo** `promote_dirty_anchors_for_repo` e **prima** di `prune_file_at_commit_cache` (`assembly.rs:348-361`): è il punto in cui status (`refresh_stale_memories_for_repo`) e ancore git (`promote_dirty_anchors_for_repo`) sono definitivi, quindi l'evidenza estratta è coerente. Modalità `InvalidateOnly`: 7 query + union-find, **zero LLM, zero embedding, zero CREATE**; marca `stale` ciò che è cambiato e archivia ciò che è sparito. Errori → `tracing::warn!` e il job di ingest prosegue (stesso contratto degli altri passi, `assembly.rs:330-360`).
2. **Pass di consolidamento** — in coda a `consolidate_loop` (`memory_consolidate.rs:294`), modalità `Full`: reconcile + sintesi delle righe `stale`/`fallback`/senza embedding + create. Vale sia per il tool MCP (`tools_memory.rs:951-974`) sia per il daemon (`consolidation.rs:104-116`). Nota: con `self.ai == None` `consolidate_loop` esce prima (`memory_consolidate.rs:166-168`), quindi senza provider l'unico reconcile attivo è quello dell'hook (`InvalidateOnly`): non nascono nuove osservazioni, ma la staleness resta corretta — coerente col fatto che senza AI non ci sono summary.

**Regola dei due ingressi:** il reconcile è la stessa funzione; l'hook non spende LLM, il pass sì. Così "zero chiamate LLM quando l'evidenza non cambia" è garantito per costruzione, non per fortuna.

**Tabella di idempotenza** (per ogni componente calcolata con `signature_hash` K e `evidence_hash` E):

| Riga K esiste? | `status` | `evidence_hash` | `generated_by` | embedding | Azione |
|---|---|---|---|---|---|
| no | — | — | — | — | solo `Full`: `CREATE` (id `obs_K`) con summary+embedding (fallback se AI assente/fallisce); in `InvalidateOnly` nessuna scrittura |
| sì | `current` | == E | `llm` | presente | **no-op** (zero LLM, zero scritture) |
| sì | `current` | == E | `fallback` | qualsiasi | sintesi (upgrade) |
| sì | `current` | != E | qualsiasi | qualsiasi | `UPDATE` in place (stesso id) + `status='stale'`, poi sintesi |
| sì | `stale` | qualsiasi | qualsiasi | qualsiasi | sintesi, poi `current` |
| sì | `current` | == E | `llm` | `NONE` | solo embedding (retry, zero LLM) |
| sì | `archived` | qualsiasi | qualsiasi | qualsiasi | riattivazione: `status='current'`, `DELETE supersedes WHERE out = obs_K`, sintesi |
| sì | `superseded` | qualsiasi | qualsiasi | qualsiasi | se nessun successore è ancora `current` → riattivazione come sopra; altrimenti no-op (il successore è la verità) |

**Split/merge:**

- Firma/anchor K scomparsa: la riga va `status='superseded'` **se** esiste almeno una componente nuova con `|members_old ∩ members_new| ≥ 2` (nucleo condiviso), altrimenti `status='archived'`. In entrambi i casi `valid_until = time::now()`.
- Per ogni coppia (nuova, vecchia) con nucleo condiviso: `RELATE nuova->supersedes->vecchia` e `UPDATE vecchia SET status='superseded', updated_at=time::now()` — **senza** `invalidated_at_oid` (non si riusa `supersede_old_set`, `gitbound.rs:227-242`, che legherebbe HEAD a una riga non git-bound).
- Merge (una nuova copre due vecchie): due edge `supersedes` dalla nuova.
- Il bound è `≤ 25 nuove × ≤ 25 vecchie` per pass.

**Nessun TTL:** l'artefatto è funzione dell'evidenza; un TTL forzerebbe churn LLM senza guadagno informativo. La staleness è solo per hash.

**Concorrenza:** il daemon esegue i passi in sequenza (`consolidation.rs:59-72`, un solo select) ma l'hook ingest può sovrapporsi. Mitigazione: `UPSERT` sull'id deterministico + delete/relate idempotenti; due reconcile concorrenti scrivono lo stesso contenuto, l'ultimo vince, nessun duplicato.

**Costo del reconcile:** 7 query SQL bounded (≤ 500 memorie, ≤ 500 archi per tipo, ≤ 500 chunk simbolo) + union-find O((M+S)α) in memoria. Nessun embedding, nessun LLM.

#### 4.8 LLM summary

**Hint D10:** `local:M` per i cluster team, `local_only:M` per i personali (stessi costanti del consolidamento, `memory_consolidate.rs:31-32`). Perché M e non `bulk_local`: `bulk_local` non seleziona un tier (è solo local→cloud, `router.rs:92-99`); la mappatura D10 assegna i task di sintesi (consolidation/analyzer/rule_synth) al tier M, e questo è un task di sintesi della stessa classe. Il personale non deve mai uscire dalla macchina → `local_only:M`.

**Prompt stability (D13):** const byte-stabile + contesto variabile + schema finale, come `build_distill_prompt` (`memory_consolidate.rs:846-860`).

```rust
const OBSERVATION_INSTRUCTIONS: &str = r#"Summarize one cluster of related software-project memories that share a structural signature.
Write a name of at most 8 words and a summary of at most 120 words describing the recurring pattern they reveal.
Use only facts stated in the memories. Do not invent files, commits, tickets, identifiers or numbers.
Do not list the memories: the evidence list is appended separately.
"#;
```

Prompt: `OBSERVATION_INSTRUCTIONS` → `SIGNATURE:` (anchor + signatures) → `EVIDENCE:` (membri ordinati per id, ciascuno troncato a 400 char) → `Return only JSON:\n{"name":"...","summary":"..."}`.

**Parametri:** `temperature: 0.0`, `max_output_tokens: 512`, una sola chiamata (retry 1 solo su JSON invalido, come il retry C7 strict `memory_consolidate.rs:563-569`).

**Validazione "solo membri reali":** il modello **non** produce membership. Sul testo restituito:
1. `name` non vuoto, ≤ 80 char; `summary` non vuoto, ≤ 1200 char (troncamento, non errore).
2. Strip dei fence markdown (pattern `parse_consolidation_decision`, `:798-808`).
3. Ogni token che *sembra* un record id (`memory:`, `commit:`, `issue:`, `source_file:`, `file_chunk:`, `symbol:`) deve comparire in `members`/`evidence_refs`; altrimenti risposta **rifiutata** → fallback (una sola retry).
4. `name`/`summary` vuoti → fallback.

**Fallback deterministico** (AI assente, provider in errore, tier M mancante su `local_only`, validazione fallita):

```text
name    = "<family_label>: <anchor_key>"            (cap 80 char)
summary = "<n> memories share <signature>. " + titoli dei primi 5 membri (" · ")
```

Il fallback è una osservazione valida (`generated_by=fallback`, `status=current`): non è un errore, e la sintesi successiva lo upgradea.

**Footer deterministico:** `content = summary + "\n\nMembers:\n" + per ogni membro "- <kind>: <title o prime 80 char>"` (max 12). Così la riga è autosufficiente e il vector match copre anche i termini dei membri.

**Costo e bound:** input ≈ 120 token (istruzioni) + 12 × ~60 token (membri troncati) ≈ 840; output ≤ 512 (tipico ~120). A $0.15/1M blended (`cost.rs`) ≈ $0.00014 per cluster cloud, 0 in locale. Bucket `system:memory_observations` (stesso pattern di `system:memory_consolidation`, `memory_consolidate.rs:519-532`). Cap `max_llm_summaries_per_pass = 10` → ≤ ~$0.0014/pass.

**Perché C8 non è cacheable (D12):**
1. Il tipo lo vieta: `ResponseCacheCallSite` ha tre varianti e la doc dice "the only call sites allowed" (`types.rs:45-55`); C8 non è tra queste, quindi servirebbe estendere l'enum e la semantica.
2. La chiave D12 è `call_site:model:sha256(context+prompt)` (`response_cache.rs:77-90`): il prompt di C8 contiene già tutta l'evidenza, quindi la chiave **è** l'`evidence_hash`. Una cache sopra sarebbe una seconda copia dello stesso dato con hit-rate identico all'idempotenza che C8 ha già — zero guadagno, un layer in più da invalidare.
3. Il gate corretto è `evidence_hash` confrontato prima di costruire il prompt: se è uguale, la chiamata non parte proprio (nessun costo di token, non solo un hit).
4. Il prompt caching cloud (D13) non aiuta: ogni cluster ha un prompt diverso, non esiste un prefisso condiviso oltre le istruzioni (che sono già stabili).

**D11:** il core di C8 non passa da `decide()` perché ogni decisione è chiusa e deterministica per costruzione (famiglia, hub, cap, membership) e la sintesi è generazione libera, non classificazione. Riuso previsto, opzionale e default off: `ai.observations.decide_shadow` misura con una domanda chiusa `observation_accept.v1` ("questa componente è un pattern reale o un artefatto del chaining?") se il tier S concorda con i filtri deterministici, per calibrare le soglie su repo reali. Stesso pattern di `decide_shadow` (`memory_consolidate.rs:235-260`), nessun cambio di comportamento.

#### 4.9 Rapporto con `consolidate_loop` e `memory_insights`

**Cosa resta:**

- `consolidate_loop` continua a fare esattamente ciò che fa (drain → dedup → distill → persist → noise decay).
- `memory_insights` resta la vista di similarità (kind+tag+cosine) e non viene sostituita: risponde a "cosa si somiglia", C8 a "cosa è strutturalmente connesso". Sono complementari.

**Cosa cambia (obbligatorio, altrimenti C8 si auto-alimenta):**

1. `select_observations` (`memory_consolidate.rs:345-347`): aggiungere `AND (store = NONE OR store != 'derived')`.
2. `pending_observation_projects` (`memory_consolidate.rs:313`): stessa esclusione per il backlog grezzo, **più** un secondo ramo per il backlog derivato:
   ```sql
   SELECT project FROM memory WHERE kind = 'observation' AND status = 'current' AND (store = NONE OR store != 'derived') GROUP BY project;
   SELECT project FROM memory WHERE store = 'derived' AND (status = 'stale' OR details.generated_by = 'fallback' OR embedding = NONE) GROUP BY project;
   ```
   Unione deduplicata in Rust (stesso decode `repo:` → nome, `memory_consolidate.rs:323-336`). Senza questo, un progetto con sole osservazioni stantie non farebbe mai girare il pass.
3. `memory_insights` (`insights.rs:91-98`): aggiungere `(store = NONE OR store != 'derived')` alla WHERE base. Le osservazioni derivate hanno la loro superficie; mescolarle ai bucket kind+tag è rumore.
4. `find_duplicate_memories` **non** va toccato: esclude già `kind != 'observation'` (`:424-428`).

**Perché non un nuovo tool:** `memory_consolidate` è già il "sleeptime pass" con trigger MCP e loop daemon; C8 è un secondo stadio dello stesso pass. Un tool dedicato porterebbe il contratto a 59, duplicherebbe la scoperta dei progetti e darebbe all'utente un pulsante che può solo fare ciò che il pass fa già. Il nome/descrizione del tool resta, la descrizione si estende.

#### 4.10 Superficie di lettura

**`memory_list`** — nuovo param opzionale `store`: `git | derived | all` (default `all`; `surreal` non è un valore di filtro perché le righe legacy hanno `store = NONE`, `schema.rs:275`). Il filtro si aggiunge alla WHERE di `mod.rs:642-645`; `store` e `details` sono già proiettati. Ordine invariato (`updated_at DESC`).

Payload di una osservazione (additivo, nessun campo rimosso):

```json
{
  "id": "memory:⟨obs_4f2a…⟩",
  "kind": "observation",
  "title": "Empty discovery root crashes the wizard",
  "content": "<summary>\n\nMembers:\n- fix: …\n- fix: …",
  "status": "current",
  "confidence": 0.75,
  "visibility": "team",
  "owner": "ibis.observation_engine",
  "tags": ["fix_root_cause", "observation"],
  "store": "derived",
  "details": {
    "signature": "fix_root_cause|empty discovery root",
    "signature_hash": "4f2a…",
    "anchor_signature": "fix_root_cause|empty discovery root",
    "signatures": ["fix_root_cause|empty discovery root", "file|src/wizard/discovery.rs"],
    "evidence_hash": "9c1e…",
    "family": "fix_root_cause",
    "members": ["memory:⟨mem_a⟩", "memory:⟨mem_b⟩"],
    "member_count": 2,
    "evidence_refs": ["source_file:[repo:⟨ibis⟩, 'src/wizard/discovery.rs']"],
    "rubric": "observation.v1",
    "generated_by": "llm"
  }
}
```

**`memory_search`** — aggiungere `store` alla projection (`mod.rs:780`): il campo è già nel modello (`mod.rs:102`), quindi il decode funziona senza altre modifiche. Nessun cambio di filtro: le osservazioni sono conoscenza legittima e devono entrare nei risultati semantici.

**`memory_expand`** — nessun cambio: con l'estensione di `derived_from`, `derived_from` restituisce già i membri (`mod.rs:372-378`).

**`ask_project`** — nessun cambio: il canale memory proietta `kind` (`search.rs:441`, `:71-72`), quindi le osservazioni compaiono etichettate come `observation` e contribuiscono al contesto multi-hop. Il canale lessicale le vede anche senza embedding (FULLTEXT `memory_content_text`, `schema.rs:466`).

**`memory_export`** — esclude i derivati (§4.6).

**Projection `.ibis/memory`** — nessun cambio di codice, ma effetto da documentare: la projection usa `memory_export` (`projection.rs:31-43`), quindi con l'esclusione dei derivati le osservazioni non finiscono in `.ibis/memory/observation/` (che è già gitignored, `projection.rs:14`). Le osservazioni grezze (`store='surreal'`) restano proiettate come oggi.

**Nessun nuovo tool.** Contratto 58 invariato; `tool_contract_test.rs:91-102` resta verde. Il fixture `discovery_tools.json` va rigenerato per le descrizioni aggiornate (`discovery_snapshot_test.rs:198-213`).

#### 4.11 Configurazione

Nuovo blocco `ai.observations` in `crates/ibis-core/src/config.rs`, accanto a `ConsolidationConfig` (`:182-210`):

```json
"ai": {
  "observations": {
    "enabled": false,
    "min_cluster_size": 2,
    "max_members": 12,
    "hub_degree_max": 10,
    "max_clusters_per_repo": 25,
    "max_members_examined": 500,
    "max_scopes_per_project": 8,
    "max_llm_summaries_per_pass": 10,
    "families": ["fix_root_cause", "issue", "symbol", "file"],
    "decide_shadow": false
  }
}
```

Default **off**, coerente con `ai.consolidation` (`config.rs:200-209`) e con la filosofia local-first: nessun costo finché l'operatore non opta. `enabled=false` spegne sia l'hook sia il pass; le righe esistenti restano inerti e possono essere ripulite a mano (hard delete consentito, §4.6). `families` accetta solo i quattro valori noti: un valore sconosciuto viene scartato con un warn una tantum (non fa fallire il boot), coerente con `SchemaEnforcement::resolve` (`details.rs:39-49`).

---

### Casi limite

| Caso | Comportamento |
|---|---|
| Grafo vuoto (0 memorie eleggibili, 0 derivate) | 0 componenti, `Ok(0)`, nessuna scrittura. |
| 0 memorie eleggibili ma derivate esistenti | le derivate vengono archiviate (`valid_until=now`): l'evidenza non esiste più. |
| DB non connesso / degradato | `Err("database not connected")`; il chiamante logga warn; nessuna scrittura, righe esistenti intatte. |
| Una delle 7 query fallisce | abort del pass con `Err` (nessuna estrazione parziale). |
| AI assente o tier M non disponibile su `local_only` | fallback deterministico, `generated_by=fallback`, `status=current`; la sintesi successiva ritenta. |
| `fix` senza `details.root_cause` (policy warn C7) | non elegibile per `fix_root_cause`, resta elegibile per `issue`/`symbol`/`file`. |
| `root_cause` in stoplist (`none`, `n/a`, …) | firma scartata (sarebbe un hub). |
| `rel_path` vuoto e `path` assente | famiglia `file`/`symbol` non applicabile per quella memoria. |
| File sparito a HEAD | la memoria diventa `stale` via `refresh_stale_memories_for_repo` (`lifecycle.rs:281-290`) e esce da Q1; il reconcile aggiorna la componente. |
| Issue senza `external_key` | firma `issue` scartata. |
| Simbolo omonimo in più chunk dello stesso file | dedupe per `(rel_path, symbol_name)`; `start_line` minimo come riferimento. |
| Firma con grado > `hub_degree_max` | firma esclusa prima dell'unione, log `observation_hub_excluded`. |
| Componente > `max_members` | componente **scartata** (non troncata), log `observation_component_capped`. |
| Componente con 1 membro | scartata (`min_cluster_size`). |
| Più di 25 componenti | persistite le prime 25 per `(member_count desc, anchor asc)`, resto loggato. |
| Scope personale con owner vuoto | scope saltato (non identificabile). |
| Più di 8 scope/pass | i restanti sono rimandati al pass successivo (log). |
| Due reconcile concorrenti | UPSERT + delete/relate idempotenti: stesso contenuto, nessun duplicato. |
| Embedding fallito | riga con `embedding = NONE`; visibile a list/lexical, non a `memory_search`; retry al pass successivo. |
| LLM restituisce id estranei | risposta rifiutata (1 retry) → fallback. |
| LLM restituisce JSON con fence | strip dei fence, poi parse (pattern `:798-808`). |
| Riga archiviata e firma che riappare | riattivazione stessa id, `DELETE supersedes WHERE out = obs_K`, log `observation_reactivated`. |
| Riga `superseded` con successore ancora valido | non riattivata: il successore è la verità corrente. |
| `memory_update`/`share` su derivata | `Err` actionable (§4.6). |
| Hard delete di una derivata da attore non fidato | `Err`; da attore locale fidato → consentito con warn audit. |
| Import di un export che conteneva osservazioni | impossibile: l'export le esclude. |
| Bump di `observation.v1` → `v2` | tutti gli `signature_hash` cambiano: le vecchie righe vengono archiviate (firma sparita) e le nuove create. Una tantum, documentato. |
| Migrazione 3.x | nessuna: greenfield 4.0, `derived_from.out` è `REMOVE`+`DEFINE` a ogni boot (`schema.rs:477-480`). |
| `symbol.kind` non dichiarato (bug pre-esistente) | C8 non dipende dalla tabella `symbol`; la firma simbolo usa `file_chunk.symbol_name`. Il bug resta fuori scope (nota in §Rischi). |

---

### Test

**Unit — funzioni pure (`ibis-search/src/memory/observations.rs`)**

| Test | Asserzione |
|---|---|
| `normalize_key_matrix` | path, CamelCase, punteggiatura finale, whitespace, unicode, cap 200. |
| `signature_fix_root_cause` | tupla attesa; stoplist → nessuna firma. |
| `signature_file_from_edge_and_from_source_path` | stessa chiave dai due percorsi; path normalizzati. |
| `signature_issue_canonical_key` | provider lowercase + `#` + external_key. |
| `signature_symbol_token_match` | match solo se tutti i token del simbolo compaiono; nessun match su sottostringhe. |
| `signature_hash_is_stable_and_scope_partitioned` | stesso input → stesso hash; progetto/scope diversi → hash diversi; 32 hex. |
| `canonical_json_is_key_sorted` | due oggetti con ordine di inserimento diverso → stesso hash. |
| `evidence_hash_tracks_semantic_fields` | cambia `title`/`content`/`details`/`status` → cambia; l’ordine di input è irrilevante; `updated_at` non è un input. |
| `union_find_merges_shared_memory` | due bucket che condividono una memoria → una componente. |
| `union_find_is_order_independent` | input permutato → stesso output (ids e ordine). |
| `hub_signature_is_excluded` | grado 11 → firma esclusa, nessuna componente gigante. |
| `singleton_is_dropped` | 1 memoria → 0 componenti. |
| `oversized_component_is_dropped_not_truncated` | 13 membri → 0 righe persistite, log. |
| `anchor_priority_is_fix_then_issue_then_symbol_then_file` | anchor atteso. |
| `observation_id_is_deterministic` | `memory:⟨obs_<32hex>⟩`. |
| `confidence_formula` | n=2 → 0.75, n=3 → 0.83, n=12 → 0.95. |
| `fallback_summary_is_deterministic` | name/summary senza AI, nessun errore. |
| `summary_validation_rejects_foreign_ids` | summary con `memory:⟨altro⟩` → rifiuto. |
| `summary_validation_truncates_long_text` | summary > 1200 char → troncata, non errore. |
| `read_only_predicate` | `derived+observation` → true; `derived+fix` → false; `surreal+observation` → false. |

**Unit — integrazione con il codice esistente (fake executor/AI, pattern `ConsolidateDb`/`ConsolidateAi`, `memory_consolidate.rs:1048-1206`)**

| Test | Asserzione |
|---|---|
| `select_observations_excludes_derived` | la WHERE contiene l'esclusione; una riga derivata non entra. |
| `pending_projects_includes_stale_derived` | la seconda query di discovery trova il progetto con sole derivate stantie. |
| `reconcile_marks_stale_only_when_hash_changes` | evidenza invariata → **0 chiamate AI**, 0 UPDATE; evidenza cambiata → 1 stale. |
| `reconcile_creates_fallback_row_without_ai` | AI assente → riga `generated_by=fallback`, `status=current`. |
| `reconcile_archives_vanished_signature` | firma sparita → `archived` + `valid_until`. |
| `split_supersedes_with_shared_core` | vecchia `superseded` + 2 edge `supersedes`. |
| `merge_supersedes_both_olds` | nuova con 2 edge `supersedes`. |
| `reactivation_clears_incoming_supersedes` | `DELETE supersedes WHERE out = obs_K`. |
| `upsert_does_not_duplicate_has_memory` | un solo edge anche dopo 3 reconcile. |
| `update_memory_rejects_derived` / `delete_memory_rejects_derived` / `share_rejects_derived` / `supersede_rejects_derived` | `Err` con messaggio. |
| `hard_delete_derived_requires_trusted_actor` | attore non fidato → `Err`; locale fidato → ok + warn. |
| `export_excludes_derived` | la WHERE contiene `(store = NONE OR store != 'derived')`. |
| `memory_list_filters_by_store` | `store=derived` restituisce solo derivate; `all` invariato. |
| `memory_search_projects_store` | la SELECT contiene `store`. |

**Integration live (SurrealDB 4.0, pattern dei live test esistenti)**

| Test | Asserzione |
|---|---|
| `two_fixes_same_root_cause_yield_one_observation` | 1 riga `kind=observation`, `store=derived`, 2 edge `derived_from`, 1 `has_memory`. |
| `second_pass_is_idempotent` | stesso id, `evidence_hash` uguale, `updated_at` invariato, 0 chiamate AI. |
| `evidence_change_regenerates_same_row` | stesso id, nessun nuovo edge `supersedes`. |
| `personal_and_team_scopes_never_mix` | 2 osservazioni distinte, summary personali con `local_only:M`. |
| `ask_project_sees_the_observation` | il canale memory restituisce `kind=observation`. |
| `read_only_end_to_end` | update/delete/share via servizio → `Err`; la riga resta `current`. |

**Eval (riuso A1)**

| Gate | Asserzione |
|---|---|
| `retrieval_eval_no_regression` | `ibis eval retrieval` con osservazioni abilitate: MRR ≥ baseline − 0.01, recall@5 ≥ baseline − 0.01 (riallineare la baseline al dataset corrente: 21 case in `evaluations/retrieval/dataset.yaml`, baseline WQ-7 0.950/0.975/1.0 su 20). |
| `observations_eval_exact_clusters` | nuovo `evaluations/observations/dataset.yaml` (estate seminata come `eval.rs:837`/`:1303`) con 8 case: stesso root_cause, file condiviso, issue condivisa, hub, singleton, isolamento scope, split, merge. Atteso: membership esatta e `signature_hash` stabili. Runner in-process `ibis eval observations` (nessun nuovo tool MCP). |

**Contract**

| Test | Asserzione |
|---|---|
| `tool_contract_test` | 58 tool invariati (`tool_contract_test.rs:91-102`). |
| `discovery_snapshot_test` | fixture rigenerato con il comando documentato (`:198-213`). |

---

### Accettazione

1. **Determinismo:** due esecuzioni sullo stesso stato producono gli stessi `signature_hash`, `evidence_hash`, id e ordine dei membri (test con input permutato).
2. **Zero LLM a evidenza invariata:** secondo reconcile con evidenza identica → contatore chiamate AI = 0, zero UPDATE su `memory`.
3. **Pattern reale:** due `fix` con lo stesso `root_cause` producono **una** osservazione con 2 edge `derived_from` e `member_count=2`, leggibile con `memory_list(kind=observation, store=derived)` e con `memory_expand`.
4. **Read-only:** `memory_update`, `memory_delete` (soft), `memory_share`, `memory_add(supersedes=…)` e `memory_import(on_conflict=supersede)` su una derivata falliscono con messaggio actionable; l'hard delete funziona solo da attore locale fidato.
5. **Nessuna auto-alimentazione:** `select_observations` e `pending_observation_projects` ignorano le derivate; una passata di consolidamento non tocca le osservazioni.
6. **Governance:** nessun cluster misto team/personal né tra owner; i personali usano `local_only:M`.
7. **Contratto MCP:** 58 tool, contract test verde, fixture discovery rigenerato.
8. **Nessuna regressione retrieval:** gate A1 §Test.
9. **Costo bounded:** ≤ 10 sintesi/pass, bucket `system:memory_observations` popolato e leggibile.
10. **Documentazione:** questo documento è il contratto; WQ-5 lo recepisce con il link e il prossimo passo di implementazione.

---

## Rischi e mitigazioni

| Rischio | Gravità | Mitigazione |
|---|---|---|
| **Chaining/hub explosion** (transitività che inghiotte il repo) | alta | hub exclusion prima dell'unione, cap componente con scarto (non troncamento), `max_clusters_per_repo`, eval dedicata. |
| **Allucinazione nella summary** | media | il LLM non produce membership; footer deterministico; validazione id estranei; fallback; `evidence_hash` come audit. |
| **Read-only troppo rigido** (osservazione sbagliata non correggibile) | media | la correzione è sull'evidenza (le memorie sorgente) e la rigenerazione è automatica; hard delete da attore locale fidato come via di manutenzione; `enabled=false` rende le righe inerti. |
| **Latenza sull'ingest** | media | l'hook fa solo reconcile deterministico (7 query bounded + UF in memoria), niente LLM/embedding; misurare P95 e, se serve, alzare il debounce del job. |
| **Costo LLM** | bassa | `local:M` (cloud solo come fallback), cap per pass, gate `evidence_hash`, bucket di costo dedicato. |
| **Accoppiamento con C7 in `warn`** | bassa | i `fix` senza `root_cause` semplicemente non entrano nella famiglia; le altre famiglie coprono. |
| **Bug pre-esistente `symbol.kind`** (`schema.rs:344-346` vs `ingest.rs:773-776`) | bassa per C8, media per il blast radius | C8 usa `file_chunk.symbol_name`; il bug va tracciato a parte (non si tocca qui). |
| **Estensione `derived_from.out`** | bassa | consumatori verificati (§0); il campo è `REMOVE`+`DEFINE` a ogni boot; `why_changed` filtra i kind; `refresh_stale` filtra `source_file:`. |
| **Drift del fixture discovery** | bassa | rigenerazione documentata nel test stesso. |
| **Doppia verità `details.members` vs edge** | bassa | gli edge sono la verità di grafo, `details.members` la lista ordinata per hash/diff/payload; il reconcile li riscrive insieme nella stessa transazione. |

---

## Ordine di implementazione

Nessun commit in questa fase. Step ordinati per dipendenza e per possibilità di shippare senza LLM.

| Step | Contenuto | File | Test |
|---|---|---|---|
| **1. Fondazione read-only** | `STORE_DERIVED`, `is_derived_observation`, guard in update/delete/share/supersede, hard delete trusted, descrizioni registry, `memory_list(store=…)`, `store` nella projection di `memory_search`, export esclude i derivati | `memory/mod.rs`, `memory/share.rs`, `memory/add.rs`, `memory/export.rs`, `tools_memory.rs`, `registry.rs` | unit guardie + contract + discovery fixture |
| **2. Firme e componenti (pure)** | `observations.rs`: normalizzazione, `Signature`, `signature_hash`, `evidence_hash`, union-find, cap/hub, ordinamento | `crates/ibis-search/src/memory/observations.rs` | unit §Test (prime 16) |
| **3. Estrazione + diff** | 7 query SurrealQL, decode tollerante, diff vs Q7, `ReconcileReport` (senza persist) | `observations.rs` | unit con fake executor |
| **4. Persist + lifecycle** | UPSERT/create/archive/supersede, edge, `details` C7, esclusioni in `select_observations`/`pending_observation_projects`, discovery a due rami | `observations.rs`, `memory_consolidate.rs`, `details.rs` | unit + live idempotenza/split/merge |
| **5. Sintesi LLM** | const D13, prompt, parser, validazione, fallback, costo, `observations_pass` in coda a `consolidate_loop`, wiring config in `tools_memory.rs`/`consolidation.rs` | `crates/ibis-optimization/src/observations.rs` (o `memory_consolidate.rs`), `memory_consolidate.rs`, `tools_memory.rs`, `consolidation.rs`, `raft.rs` | unit LLM/fallback + live |
| **6. Hook, config, eval** | `ObservationsConfig` in `config.rs`, hook in `refresh_stale_after_ingest`, dataset/runner `ibis eval observations`, gate A1 | `config.rs`, `assembly.rs`, `ibis-cli`, `evaluations/observations/` | eval + regressione A1 |

Step 1–4 sono rilasciabili con `ai.observations.enabled=false` e senza alcun costo LLM: sono la parte deterministica, che è anche il valore differenziante. Lo step 5 è l'unico che spende token.

---

## Fuori scope / follow-up

- **E14/E15** (Parte II): C8 v1 **non dipende** da E14/E15 e non tocca `agentic.rs` né il payload di `ask_project`. E15 aggiunge un blocco `EVIDENCE:` opzionale in `capture_observation` (`citations_json`); C8 potrà consumarlo in una fase successiva, ma non è un requisito della v1 (nessuna dipendenza circolare).
- **VSIX `store='derived'`**: il menu di modifica oggi pre-disabilita solo `store === 'git'` (`memory-tree.js:225-245`); il follow-up UI (badge + menu pre-disabilitato per `derived`) è consigliato ma non bloccante, perché i guard server rifiutano le mutazioni.
- **E16 durable task**: C8 è indicato in L1 come primo candidato, ma in questa specifica il pass resta sincrono e bounded (≤ 10 sintesi); il task store si aggancia dopo.
- **Workstream**: la tabella non ha writer (`grep part_of_workstream` → solo letture); C8 non raggruppa per workstream.
- **Fix di `symbol.kind`**: fuori perimetro, tracciato nei rischi.
- **Dedup semantica tra osservazioni di progetti diversi**: non in v1 (la firma include `project_key`).
- **G19 projection**: nessun cambio; `.ibis/memory/observation/` è già ignorato.
- **Reranker/retrieval**: C8 aggiunge righe al canale memory; l'impatto è misurato da A1, nessuna modifica al ranking.

---

## Riepilogo dipendenze

```text
C7 (details.root_cause, rubrica per-kind) ──dati──► C8 firma fix_root_cause + details observation
C9 (normalize_title, canonical ticket ids) ──regole──► C8 normalize_key + chiavi issue
D10 (local:M / local_only:M) ──provider──► C8 sintesi
D11 (shadow decide) ──opzionale──► C8 calibrazione soglie (default off)
D12 (response cache) ──NON applicabile──► C8 (enum chiuso a 3 call-site; chiave = evidence_hash)
D13 (prompt stability) ──convenzione──► C8 OBSERVATION_INSTRUCTIONS
B6 (embedding cache L2) ──cache──► C8 embedding della summary rigenerata
A1 (eval harness) ──gate──► C8 regressione retrieval + runner observations
Schema 4.0 (derived_from REMOVE+DEFINE, memory_kind observation) ──DDL──► C8 (un solo campo esteso)
Governance store/owner-only ──guard──► C8 read-only via store='derived'
```

---

# Parte II — E14 + E15: loop brakes e trust/evidenza

---

## 0. Contratto condiviso E14/E15

### 0.1 Invarianti retrocompatibili

- `AgenticResult` resta `Debug + Clone + Default + Serialize + Deserialize` (agentic.rs:16-22).
- `answer: String`, `sources: Vec<SearchResult>`, `steps: Vec<String>` (wire `reasoning_steps`) restano invariati per nome, tipo e semantica. `steps` continua a contenere ogni turno del modello, incluso l'eventuale turno di riparazione.
- `sources` resta `Vec<SearchResult>` con i wire name esistenti (`code_chunk_id`, `file_path`, `relevance_score`, `is_current`, `source_kind`, `commit_hash`, `status`, `kind`, `channel`, `as_of_ref`, `content_source`, `context`, `content` — search.rs:46-97). **Non esiste un campo `line`**: nessun campo del contratto E14/E15 lo inventa.
- I nuovi campi sono additivi e hanno `#[serde(default)]`: un payload vecchio continua a deserializzarsi; un client vecchio ignora i campi nuovi.
- I 58 tool MCP restano invariati (tool_contract_test.rs:13-102). Le variazioni di schema di input sono due: `capture_observation.citations_json` (§E15.5) e `memory_list.store` (C8 §4.10). Tool count invariato; fixture discovery rigenerata in lockstep per entrambe.
- `ask_project` non è mai response-cached (D12: types.rs:45-55, 68-82; agentic.rs:97-98). E14 non introduce cache di turni, repair o risposte.
- Ogni chiamata del loop usa lo stesso route hint `local:L` (D10: agentic.rs:55-58), incluso il turno di riparazione: nessuno switch di modello intra-ask.
- E15 non chiama mai un LLM. E14 non introduce un verifier LLM: il completion check è deterministico.

### 0.2 `AgenticResult` (payload additivo)

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgenticResult {
    pub answer: String,
    pub sources: Vec<SearchResult>,
    #[serde(rename = "reasoning_steps")]
    pub steps: Vec<String>,

    // E15 — produttore
    #[serde(default)]
    pub citations: Vec<AgenticCitation>,
    #[serde(default)]
    pub trust: AgenticTrust,

    // E14 — flag del loop
    #[serde(default)]
    pub budget_exhausted: bool,
    #[serde(default)]
    pub completion_checked: bool,
    #[serde(default)]
    pub repair_turns: u8,
    #[serde(default)]
    pub invalid_citations: usize,
    #[serde(default)]
    pub search_unavailable: bool,
    #[serde(default)]
    pub stop_reason: AgenticStopReason,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgenticStopReason {
    #[default]
    FinalAnswer,
    MaxSteps,
    BudgetExhausted,
    NoProgress,
    CompletionLowCoverage,
    AiError,
    ContextOnly,
    EmptyQuery,
}
```

Note di contratto:

- `trust` è **sempre serializzato** (anche nello stato di default `low/0/weak`): il payload ha forma stabile e il consumer non deve distinguere “campo assente” da “nessuna evidenza”.
- `citations` è serializzato sempre, anche `[]`.
- `stop_reason` è un enum chiuso con valori snake_case; un payload vecchio senza il campo decodifica a `final_answer`.
- `budget_exhausted` è ridondante con `stop_reason == "budget_exhausted"` per scelta di ergonomia: i consumer semplici possono leggere un booleano senza enumerare i motivi.
- `repair_turns` è 0 o 1 (un solo repair turn); conta i turni di riparazione **tentati**, non necessariamente riusciti.
- `invalid_citations` è il numero di citation id non validi trovati nei claim, deduplicati per claim (E15 lo calcola, E14 lo copia nel flag).

Esempio di payload (`sources` è abbreviato a `[]`: resta un array di `SearchResult` invariati; i campi stringa vuoti delle citation sono omessi da serde):

```json
{
  "answer": "The retry loop is bounded by MAX_RETRIES [S1]. It uses exponential backoff [S2].",
  "sources": [],
  "reasoning_steps": ["THOUGHT: ... SEARCH: \"retry loop\"", "FINAL ANSWER: ..."],
  "citations": [
    {
      "citation_id": "S1",
      "source_id": "file_chunk:...",
      "source_kind": "file_chunk",
      "path": "src/retry.rs",
      "score": 0.91,
      "excerpt": "fn retry(...) { ... }"
    }
  ],
  "trust": {
    "score": 0.867,
    "level": "high",
    "coverage": 0.75,
    "weak_context": false,
    "cited_sources": 3,
    "observed_sources": 3
  },
  "budget_exhausted": false,
  "completion_checked": true,
  "repair_turns": 0,
  "invalid_citations": 0,
  "search_unavailable": false,
  "stop_reason": "final_answer"
}
```

### 0.3 `AgenticCitation`

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgenticCitation {
    #[serde(default)]
    pub citation_id: String,   // "S1".."S20"
    #[serde(default)]
    pub source_id: String,     // SearchResult.id (wire: code_chunk_id)
    #[serde(default)]
    pub source_kind: String,   // file_chunk | commit_chunk | memory | reasoning
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,          // SearchResult.path (wire: file_path)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_hash: String,   // SearchResult.commit_hash
    #[serde(default)]
    pub score: f64,            // SearchResult.score, sanitizzato
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt: String,       // content bounded, 240 rune
}
```

Mapping uno-a-uno dai campi reali di `SearchResult` (search.rs:46-97):

| Campo citation | Sorgente reale | Note |
|---|---|---|
| `citation_id` | assegnato dal catalogo | `S1`..`S20`, prima osservazione |
| `source_id` | `SearchResult.id` | per `file_chunk` è il chunk id; per memory/reasoning è il record id |
| `source_kind` | `SearchResult.source_kind` | vuoto → `"unknown"` (coerente con `format_observation`, agentic.rs:286) |
| `path` | `SearchResult.path` | vuoto per memory/reasoning |
| `commit_hash` | `SearchResult.commit_hash` | vuoto per file chunk HEAD e memory |
| `score` | `SearchResult.score` | `NaN`/inf → `0.0` |
| `excerpt` | `SearchResult.content` | 240 rune, whitespace collassato, `...` se troncato |

`citations` è il **catalogo citabile** (tutte le fonti osservate fino al cap), non solo quelle citate nella risposta: serve a dare id stabili al modello, a mostrare l'evidenza in UI e ad alimentare `capture_observation`. `trust.cited_sources` distingue quante sono state effettivamente citate. I metadati completi restano in `sources`; la citation è un indice leggero, non una copia di `SearchResult`.

### 0.4 `AgenticTrust`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgenticTrust {
    #[serde(default)]
    pub score: f64,            // 0..1, base 1000
    #[serde(default)]
    pub level: TrustLevel,     // high | medium | low
    #[serde(default)]
    pub coverage: f64,         // 0..1, claim-level, base 1000
    #[serde(default)]
    pub weak_context: bool,
    #[serde(default)]
    pub cited_sources: usize,
    #[serde(default)]
    pub observed_sources: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    High,
    Medium,
    #[default]
    Low,
}

impl Default for AgenticTrust {
    fn default() -> Self {
        Self {
            score: 0.0,
            level: TrustLevel::Low,
            coverage: 0.0,
            weak_context: true,
            cited_sources: 0,
            observed_sources: 0,
        }
    }
}
```

- `observed_sources` = numero di entry nel catalogo citazioni **dopo dedup e cap** (≤ 20), non `sources.len()`. Le fonti oltre il cap restano in `sources` ma non sono citabili e non entrano nel denominatore del trust.
- `cited_sources` = numero di entry del catalogo il cui `citation_id` compare in almeno un claim della risposta finale.
- `trust` default è lo stato onesto “nessuna evidenza”: score 0, coverage 0, level low, weak true.

### 0.5 Definizione unica di coverage

**Coverage = claim coperti / claim totali** nella `FINAL ANSWER`. È l'unica definizione usata da E15 e E14.

Definizioni deterministiche:

- **Claim**: segmento di testo della `FINAL ANSWER` che (a) non è un heading Markdown (dopo trim inizia con `#`), (b) non termina con `:` (è un lead-in a una lista), (c) contiene almeno un carattere alfabetico dopo la rimozione dei citation marker, e (d) non è un'abbreviazione isolata della stoplist (`e.g.`, `i.e.`, `etc.`, `vs.`, `cf.`, `et al.`). I blocchi di codice fenced (delimitati da tripla backtick) e gli span di codice inline (delimitati da backtick singolo) vengono rimossi prima della segmentazione: il codice è evidenza, non claim.
- **Segmentazione**: prima per riga (le righe di lista `- `, `* `, `1. ` sono segmenti autonomi), poi per confine di frase su `[.!?。！？]` seguito da whitespace e da maiuscola/digit/`[`/`(`/`"` oppure da fine testo. Decimali (`3.14`), versioni (`v1.2.3`) e URL non vengono spezzati.
- **Claim coperto**: contiene almeno un citation marker valido `[S<n>]` (o `[s<n>]`, o `[S1, S2]`), dove `n` è un id del catalogo. Un marker valido in un claim copre il claim anche se il claim contiene altri marker invalidi.
- **Coverage**: `covered_claims / total_claims`; se `total_claims == 0`, coverage = `0.0`. Il valore è calcolato in base 1000 (vedi §E15.3) e serializzato con 3 decimali.
- **`source_citation_ratio`**: `cited_sources / observed_sources`, è una **metrica diversa** usata solo come input del trust score. Non viene mai chiamata coverage, non è esposta come coverage e non ha un campo nel payload. Questo evita due definizioni di coverage.
- **`invalid_citations`**: marker con id non presente nel catalogo, contati una volta per claim per id (un `[S99]` ripetuto nella stessa frase conta 1; in due claim conta 2). I marker dentro fenced code block o span inline non vengono contati.

La coverage è un **proxy di citazione**, non una verifica di entailment: un claim può citare una fonte sbagliata e risultare coperto. Il trust è un segnale di grounding, non una garanzia di correttezza. Questa limitazione è esplicita e accettata in v1 (L1: “la coverage su testo libero è fuzzy senza un parser di claim; accettare la versione citanze-based”).

**Un solo implementatore**: E15 espone `assess(catalog, answer) -> TrustAssessment` (crate-private), che calcola coverage, `cited_sources`, `invalid_citations` e il trust. E14 non ricalcola nulla: legge `assessment.trust.coverage`, `assessment.covered_claims`, `assessment.total_claims`, `assessment.invalid_citations` e applica solo la policy (soglia, repair, flag). Vedi §0.7.

### 0.6 Citation id: assegnazione, stabilità, cap

- Il catalogo assegna `S1`, `S2`, … **in ordine di prima osservazione** durante il loop ReAct. Un risultato già visto mantiene il suo id; una ricerca successiva non riassegna e non riordina.
- Cap `MAX_CITATIONS = 20`. Oltre il cap la fonte resta in `sources` e nell'osservazione, ma **senza** `[S<n>]`: il prompt dice che solo gli id mostrati sono citabili. Un id oltre il cap è invalido per coverage.
- Chiave di dedup: `SearchResult.id` se non vuoto; altrimenti fallback `source_kind|path|commit_hash`; se anche il fallback è vuoto, la fonte è inidentificabile e viene scartata sia dal catalogo sia da `sources` (non si inventa un id).
- Il catalogo conserva la **prima istantanea** di una fonte (score/excerpt/path/commit). Se la stessa fonte viene riosservata con contenuto diverso, vince la prima: gli id restano stabili nello stesso ask.
- `excerpt`: 240 rune, whitespace collassato (spazi/newline/tab → singolo spazio), trim, `...` se troncato. `content` vuoto → excerpt vuoto ma citation valida (path/commit/id identificano la fonte).
- `score`: se `NaN`/inf → `0.0`; clamp non necessario per la citation (si serializza il valore sanitizzato), il clamp a `[0,1]` avviene solo nel trust.
- Il prompt ReAct istruisce il modello a citare `[S1]`…`[S20]` in `FINAL ANSWER` (vedi §E14.5). La formattazione dell'osservazione antepone `[S<n>]` alla riga della fonte citabile.

### 0.7 Proprietà e ordine di esecuzione

| Fase | Owner | Cosa fa |
|---|---|---|
| 1. Osservazione | E15 | `CitationCatalog::observe(&SearchResult)` assegna/riusa l'id, dedup, cap, excerpt. L'osservazione mostra `[S<n>]`. |
| 2. FINAL ANSWER | E15 | `assess(&catalog, &answer)` calcola coverage, cited/invalid, trust. |
| 3. Policy | E14 | Legge `assessment.trust.coverage` e i conteggi; decide accept / repair / stop. |
| 4. Repair (max 1) | E14 | Chiede al modello di ri-emettere `FINAL ANSWER` con citazioni; il nuovo answer viene riassessato da E15. |
| 5. Payload | E15 | `result.citations = catalog.entries()`, `result.trust = chosen_assessment.trust`; E14 copia i flag. |

- E15 non controlla il loop; E14 non calcola coverage/trust. Nessun doppio conteggio: la coverage è calcolata una volta per ogni answer candidata (originale e, se c'è, riparata), e il trust finale è quello dell'answer scelta.
- Se il turno di riparazione produce una SEARCH, la ricerca entra nel catalogo; il trust finale usa il catalogo finale e l'answer scelta (che potrebbe non citare le nuove fonti: è un segnale conservativo, non un errore).
- `trust.coverage` nel payload è esattamente `coverage_bp / 1000` dell'assessment scelto; E14 non lo ricalcola.

### 0.8 Estensioni e deviazioni dichiarate

1. **Coverage claim-level, non cited/observed.** La ratio cited/observed esiste come `source_citation_ratio` interna al trust. Motivazione: E14 deve gate-are sui claim; una sola definizione di coverage.
2. **`citations` = catalogo citabile**, non solo le fonti citate. Motivazione: id stabili, UI evidenza, capture. `cited_sources` distingue.
3. **`observed_sources` = catalogo post-cap**, non `sources.len()`. Motivazione: oltre il cap la fonte non è citabile; includerla penalizzerebbe il trust per un limite di payload.
4. **`trust` sempre serializzato** (default low/weak), non skippato. Motivazione: forma stabile per i consumer.
5. **`sources` in ordine di prima osservazione.** Il codice attuale usa `HashMap<String, SearchResult>` e `into_values()` (agentic.rs:86-87, 257), quindi l'ordine è arbitrario. Il cambio è deterministico e necessario per gli id stabili; la forma (`Vec<SearchResult>`) non cambia.
6. **Flag E14 aggiuntivi** (`stop_reason`, `search_unavailable`, `invalid_citations`) oltre agli esempi del contratto. Sono consentiti dalla clausola “E14 può aggiungere flag propri”.
7. **`capture_observation.citations_json`**: unica variazione di schema di input; 58 tool invariati, fixture discovery rigenerata.
8. **Budget in token grezzi, non billed.** Il budget conta prompt+candidates grezzi; lo sconto D13 (`cached_tokens`) si applica solo al `CostTracker`. Motivazione: il budget è un freno di compute/contesto, il costo è un segnale di billing.
9. **`max_steps` conta le chiamate al modello, incluso il repair turn.** Non c'è una chiamata extra oltre `max_steps`: il repair è best-effort se resta uno step. Motivazione: costo prevedibile.
10. **`token_budget = 0` disabilita il freno** (opt-out esplicito), non “budget zero”. Motivazione: evitare di brickare l'ask per un misconfig; `max_steps` resta il freno hard.
11. **`at_ref` fuori scope v1** (§E15.7). Nessun trust/citation sul ramo strutturato.
12. **Nessun campo `line`.** L'identità è `source_id` + `path` + `commit_hash`; `SearchResult` non ha `line`.

### 0.9 Riuso dei lavori precedenti

| Lavoro | Come viene riusato |
|---|---|
| **D10** | `AGENTIC_ROUTE_HINT = "local:L"` (agentic.rs:55-58) resta pinnato per tutto il loop e per il repair; nessun model switch intra-task. |
| **D11** | Il completion check è una **bounded decision deterministica** (soglia + repair + stop), non una chiamata `decide()`: D11 è precedente di design, non dipendenza runtime. |
| **D12** | `ask_project`/agentic mai cachati (types.rs:45-55; agentic.rs:97-98); E14 non aggiunge cache di turni/repair. |
| **D13** | `SYSTEM_PROMPT` resta il prefisso byte-stabile; `CITATION_PROTOCOL` è un blocco stabile appeso; branch context variabile dopo; question ultima. Snapshot test (pattern memory_consolidate.rs:2101-2122). |
| **A1** | Nessuna modifica al retrieval di `ask_project`; la baseline `ibis eval retrieval` resta il guardrail. Nessun nuovo harness agentic in v1 (servirebbe un giudice LLM). |
| **B6** | La cache embedding è trasparente al loop; i token di embedding non entrano nel `token_budget` di generazione (sono tracciati su `system:search_vector`). |

### 0.10 Non-obiettivi

- Nessun codice di prodotto in questa fase di specifica; il documento propone modifiche ai file esistenti ma non le applica.
- Nessun trust/citation su `ask_project_at_ref` (v1).
- Nessun badge VSIX (follow-up).
- Nessuna implementazione C8 (Parte I); E15 definisce solo il contratto di evidenza che C8 può consumare.
- Nessun nuovo tool MCP; nessun verifier LLM; nessuna response cache; nessun task durevole (E16).

---

## E15 — Trust score / evidenza nel risultato ask (produttore)

### Aggancio

- `AgenticResult { answer, sources, steps }` — agentic.rs:16-22.
- Raccolta fonti nel loop: `unique_sources: HashMap<String, SearchResult>` (agentic.rs:86-87), inserimento per `r.id` (agentic.rs:226-228), flatten con `into_values()` (agentic.rs:256-257).
- Formattazione osservazione: `format_observation` (agentic.rs:271-331), truncation a 2000 rune (agentic.rs:277-281), label per kind (agentic.rs:282-295), `observation_label` (agentic.rs:334-353).
- Payload MCP: `json_result(&result)` nel ramo agentic di `ask_project` (tools_memory.rs:1298-1301).
- Campi reali di `SearchResult` (search.rs:46-97): `id`/`code_chunk_id`, `path`/`file_path`, `score`/`relevance_score`, `is_current`, `source_kind`, `commit_hash`, `status`, `kind`, `channel`, `as_of_ref`, `content_source`, `context`, `content`. Nessun `line`.
- `capture_observation`: handler (tools_memory.rs:900-934), parametri (capture.rs:9-16), servizio (capture.rs:21-57).
- Ramo `at_ref`: `ask_project_at_ref` (askref.rs:27-35), handler che restituisce `Vec<SearchResult>` (tools_memory.rs:1252-1296, return a 1295), clone-only degradato `{degraded:true, results}` (tools_memory.rs:1280-1294).

### Flusso attuale

1. Il loop ReAct esegue `SEARCH` e accumula le fonti in una `HashMap` keyed su `SearchResult.id` (agentic.rs:86-87, 226-228). La flatten finale usa `into_values()` (agentic.rs:257): l'ordine di `sources` non è deterministico.
2. `format_observation` mostra label, score, source_kind, is_current, status, commit, contesto, issue e commit (agentic.rs:271-331). Non c'è alcun citation id: il modello non ha un modo stabile per riferirsi a una fonte.
3. `AgenticResult` espone solo `answer`, `sources`, `reasoning_steps` (agentic.rs:16-22). Non c'è trust, coverage, weak_context, né evidenza strutturata.
4. Il payload MCP è la serializzazione diretta della struct (tools_memory.rs:1298-1301): nessun campo di qualità.
5. `capture_observation` accetta solo `text` (tools_memory.rs:906-913; capture.rs:9-16) e crea una memoria `kind=observation` con `confidence=0.5` (capture.rs:39-56). Non c'è modo di allegare l'evidenza strutturata (source_id/path/commit/score/excerpt).
6. Il ramo `at_ref` non passa dal ReAct: restituisce direttamente `Vec<SearchResult>` (tools_memory.rs:1295). Non c'è una risposta in linguaggio naturale da validare.

**Gap funzionale:** manca un catalogo citabile stabile, una definizione unica di coverage, un trust score deterministico e un ponte strutturato verso `capture_observation`/C8.

### Trasformazione

#### E15.1 Modello citazioni (catalogo)

1. Nuovo modulo `crates/ibis-search/src/agentic_trust.rs` (dichiarato in `lib.rs`), con:
   - `pub struct AgenticCitation`, `pub struct AgenticTrust`, `pub enum TrustLevel` (contratto §0.3/§0.4).
   - `pub(crate) struct CitationCatalog` con `observe`, `entries`, `valid_ids`, `len`.
   - `pub(crate) enum ObserveOutcome { Cited(String), Uncited, Skip }`:
     - `Cited(id)` = fonte nel catalogo (nuova o già vista), citabile.
     - `Uncited` = cap raggiunto: la fonte resta in `sources` e nell'osservazione ma non ha id.
     - `Skip` = fonte inidentificabile (id e fallback vuoti): scartata da catalogo e `sources`.
   - `pub(crate) struct TrustAssessment { trust, total_claims, covered_claims, invalid_citations, coverage_bp }`.
   - `pub(crate) fn assess(catalog: &CitationCatalog, answer: &str) -> TrustAssessment`.
   - Costanti: `MAX_CITATIONS = 20`, `CITATION_EXCERPT_RUNES = 240`, `MIN_EVIDENCE_SCORE = 0.30`, `WEAK_CONTEXT_MIN_SOURCES = 2`, pesi trust.
2. `CitationCatalog::observe(&SearchResult) -> ObserveOutcome`:
   - calcola la chiave: `id` se non vuoto, altrimenti `source_kind|path|commit_hash`; se vuota → `Skip`.
   - se la chiave è già presente → `Cited(id_esistente)` (stabile); non aggiorna l'entry.
   - se il catalogo ha già `MAX_CITATIONS` entry → `Uncited` (fonte non citabile; resta in `sources`).
   - altrimenti crea `AgenticCitation` con `citation_id = format!("S{}", entries.len()+1)`, `source_id = r.id`, `source_kind = empty→"unknown"`, `path`, `commit_hash`, `score` sanitizzato, `excerpt = bounded_excerpt(&r.content, 240)`; push e ritorna `Cited(id)`.
3. `bounded_excerpt`:
   - collassa ogni run di whitespace (`char::is_whitespace`) in un singolo spazio, trim.
   - prende le prime 240 `char` (rune, non byte); se tronca, appende `...`.
   - `content` vuoto → stringa vuota.
4. `sources` e catalogo condividono la stessa chiave e lo stesso ordine di prima osservazione:
   - `ObservedSource { citation_id: Option<String>, result: SearchResult }` interno al loop; viene pushata per `Cited` e `Uncited`, non per `Skip`.
   - `sources` finale = `observed.iter().map(|o| o.result.clone()).collect()`.
   - `citations` finale = `catalog.entries().to_vec()`.
5. L'osservazione formattata antepone `[S<n>] ` alla riga della fonte citabile; per le fonti oltre il cap non c'è prefisso, mentre le inidentificabili (`Skip`) non entrano nell'osservazione. `format_observation` prende `&[ObservedSource]` (interno, non pubblico).
6. Il prompt ReAct istruisce il modello a citare `[S<n>]` in `FINAL ANSWER` (§E14.5). Un id inventato non è valido: non copre claim, viene contato in `invalid_citations`.

#### E15.2 Trust score deterministico

Formula (tutti i componenti in base 1000, `bp(x) = round(clamp(x,0,1) * 1000)`):

```text
coverage_bp       = bp(covered_claims / total_claims)            // 0 se total_claims = 0
source_ratio_bp   = bp(cited_sources / observed_sources)          // 0 se observed_sources = 0
top_bp            = 0 se catalogo vuoto o max(score) NaN
                  = bp(clamp(max(score), 0, 1)) altrimenti
current_bp        = 0 se observed_sources = 0
                  = 500 se non ci sono fonti file_chunk/memory   // neutro per ask commit-only
                  = bp(current_count / current_kind_sources)
diversity_bp      = bp(min(1, distinct_source_kinds / 3))         // 3 kind saturano
citation_bp       = bp(min(1, cited_sources / 3))                 // 3 citazioni saturano

score_bp = (400*coverage_bp + 200*source_ratio_bp + 150*top_bp
          + 100*current_bp + 100*diversity_bp + 50*citation_bp + 500) / 1000
score    = score_bp / 1000
```

- Tutti i componenti usano il **catalogo citazioni** (post dedup/cap), non `sources`: le fonti oltre il cap non influenzano il trust.
- `current_kind_sources` = entry del catalogo con `source_kind` in `{file_chunk, memory}`; `current_count` = quante hanno `is_current=true`. `commit_chunk` è sempre `false` (search.rs:758-760), `memory` è sempre `true` (search.rs:761-763), `file_chunk` dipende da `hash == HEAD` (search.rs:753-757).
- `distinct_source_kinds` = kind non vuoti distinti nel catalogo; `unknown` conta come un kind.
- `weak_context = observed_sources < 2 || above_threshold < 2`, dove `above_threshold` = entry con `score >= MIN_EVIDENCE_SCORE (0.30)`. Soglia inclusiva.
- `level`: `score_bp >= 700` → `high`; `score_bp >= 450` → `medium`; altrimenti `low`. Se `weak_context && level == high` → `medium` (cap: non si dichiara “high” con meno di 2 fonti sopra soglia).
- `coverage = coverage_bp / 1000`; `score = score_bp / 1000`. Base 1000 = niente drift floating point, numeri di test esatti.
- `observed_sources = catalog.len()` (post-cap); `cited_sources` = id distinti validi citati nei claim.
- `TrustAssessment` porta anche `total_claims`, `covered_claims`, `invalid_citations` per il repair E14; non sono nel payload trust.

Motivazione dei pesi (somma 1000):

| Componente | Peso | Perché |
|---|---:|---|
| `coverage` | 400 | È il segnale diretto di grounding della risposta: il testo è supportato da citazioni? |
| `source_ratio` | 200 | Il modello ha usato l'evidenza disponibile o solo una parte? |
| `top_score` | 150 | Confidenza del retrieval (saturante, grossolana, non una probabilità). |
| `current_ratio` | 100 | Preferisce verità HEAD vs storico/ambiguo; neutro per ask commit-only. |
| `kind_diversity` | 100 | Grounding multi-canale (code + commit + memory) riduce la falsa sicurezza di un solo canale. |
| `citation_count` | 50 | Segnale di quantità, saturato a 3 per non premiare citation spam. |

Nessun LLM, nessuna soglia appresa, nessun pricing: stessi input → stesso output. Le costanti sono versionate nel codice, non configurabili per deployment, per mantenere comparabilità del segnale.

#### E15.3 Coverage (definizione unica, implementata una volta)

1. `assess` rimuove fenced code block e span inline, segmenta i claim (§0.5), estrae i marker con regex `\[\s*([Ss]\d+(?:\s*,\s*[Ss]\d+)*)\s*\]`, normalizza a `usize`.
2. Per ogni claim: `covered` se almeno un id è in `valid_ids`; `invalid` = id non in `valid_ids`, dedup per claim; `cited_ids` = unione degli id validi nei claim.
3. `coverage_bp = bp(covered/total)`; `total=0 → 0`. `cited_sources = cited_ids.len()`.
4. E14 consuma `TrustAssessment` (coverage_bp + counts), non reimplementa la segmentazione. Questo è l'unico punto in cui la definizione di “claim coperto” vive.
5. Limite dichiarato: è una verifica di **presenza di citazione**, non di entailment. Un claim con `[S1]` è coperto anche se `S1` non lo supporta semanticamente. Il trust è un proxy; la UI deve presentarlo come tale.

#### E15.4 Payload MCP

1. `AgenticResult` guadagna i campi di §0.2; `sources` e `reasoning_steps` restano invariati.
2. Il handler `ask_project` (tools_memory.rs:1298-1301) non cambia: `json_result(&result)` serializza i campi nuovi.
3. Lo schema di input di `ask_project` non cambia (registry.rs:443-452): nessuna fixture discovery da rigenerare per `ask_project`.
4. I 58 tool restano (tool_contract_test.rs:13-102). L'unica variazione di schema è `capture_observation.citations_json` (§E15.5), che richiede la rigenerazione della fixture (`discovery_snapshot_test.rs:198-213`).
5. `sources` mantiene i wire name esistenti; `citations` usa i nomi di §0.3; `trust` usa i nomi di §0.4. Nessun `line`.

#### E15.5 Capture observation (ponte verso C8)

1. `capture_observation` guadagna il parametro opzionale `citations_json` (String, JSON array di `AgenticCitation`), in linea con la convenzione esistente dei payload strutturati (`items_json` per `memory_import`, `details` per `memory_add` — registry.rs:175, 398); la descrizione del tool menziona l'evidenza opzionale. String e non Array/Object perché `PropKind` supporta solo String/Number/Boolean (registry.rs:42-50).
2. Handler:
   - assente/vuoto → `Vec::new()`, comportamento byte-identico a oggi.
   - JSON invalido → `error_result("invalid citations_json: ...")`, nessuna scrittura parziale.
   - array valido → parse `Vec<AgenticCitation>` (riusa la stessa struct; `citation_id` è ignorato perché ask-local).
3. `CaptureObservationParams` guadagna `citations: Vec<AgenticCitation>` (capture.rs:9-16). `capture_observation` costruisce il contenuto:
   - se `citations` è vuoto → `text` invariato.
   - altrimenti `text` + `\n\nEVIDENCE:\n` + una riga per citation, max 20 entry, dedup con la stessa chiave del catalogo (`source_id` se non vuoto, altrimenti `source_kind|path|commit_hash`), excerpt 160 rune whitespace-collassato; le citation con `source_id`, `path` e `commit_hash` tutti vuoti vengono scartate:
     ```text
     - [file_chunk] src/retry.rs @ abc1234 score=0.87 id=file_chunk:... excerpt="fn retry(...)"
     - [memory] score=0.72 id=memory:mem_... excerpt="..."
     ```
   - campi vuoti omessi (`path`, `@ commit`, `excerpt=`).
4. **Nessuna duplicazione di memoria**: resta una sola riga `memory` per cattura, con `kind=observation`, `confidence=0.5`, `provenance_tool="capture_observation"` (capture.rs:39-56). L'evidenza è testo dentro `content`, non una tabella/edge separata. C8 può parsare il blocco `EVIDENCE:` in modo deterministico; il formato è stabile e versionato da questa spec.
5. Il blocco è bounded (20 × 160 rune ≈ 3.2k char) per non gonfiare la memoria; un warn logga l'overflow.
6. I campi salvati sono `source_id`, `source_kind`, `path`, `commit_hash`, `score`, `excerpt`; `source_kind` è incluso come prefisso per il consumo C8. `citation_id` non viene salvato (è un id di sessione ask).

#### E15.6 UI/IDE

- v1 specifica solo il payload MCP stabile: `trust.level`, `trust.score`, `trust.coverage`, `trust.weak_context`, `trust.cited_sources`, `trust.observed_sources`, `citations[]`.
- Il badge VSIX (colore/icona in base a `level`, tooltip con coverage/weak_context, pannello “show evidence” da `citations`) è un **follow-up**, non parte della v1. Nessun codice UI in questa fase.
- Il payload è progettato per essere consumato anche da client non-VSIX (Cursor/Claude Desktop): campi piatti, nessun riferimento a path locali obbligatorio, `path`/`commit_hash` opzionali.

#### E15.7 Ambito: solo ask agentic, non `at_ref`

**Decisione: E15 vale solo per `ask_project` agentic in v1.** Motivazioni:

- `ask_project_at_ref` restituisce `Vec<SearchResult>` (tools_memory.rs:1295), non una risposta in linguaggio naturale: non ci sono claim da coprire né una `FINAL ANSWER` da validare.
- Aggiungere trust al ramo at_ref richiederebbe o un envelope `{results, trust}` (breaking: oggi è un array) o campi per-hit (pollution di `SearchResult`, che è condiviso da tutti i canali).
- Il catalogo citazioni è legato al flusso di osservazione ReAct; at_ref non ha osservazioni né citation id.
- Il ramo at_ref ha già segnali strutturati (`score`, `is_current`, `as_of_ref`, `content_source`, `degraded`) che il consumer può usare.

Follow-up v2 (non ora): parametro opzionale `include_trust=true` che restituisce `{results, trust}` oppure un nuovo tool dedicato; richiede una definizione di coverage strutturale (non claim-level) e una decisione di compatibilità.

#### E15.8 Failure mode

| Scenario | Comportamento E15 |
|---|---|
| Zero fonti | catalogo vuoto, `citations: []`, `trust` default (0/low/weak true); risposta accettata, nessun errore. |
| Tutte le fonti sotto 0.30 | `weak_context=true`; il trust può restare medium se coverage/source ratio sono alti (es. caso J), ma il level è capped a medium. |
| DB degradato / search failure | il loop può avere zero fonti o fonti parziali; trust basso/weak; `search_unavailable` è un flag E14. Nessun errore E15. |
| Citazioni invalide/inventate | non coprono claim, non incrementano `cited_sources`; `invalid_citations` li conta; coverage scende. Nessun errore. |
| Risposta senza citazioni | coverage 0, `cited_sources` 0, trust basso; E14 warn/enforce decide se riparare. Non è un errore. |
| `answer` vuota/fallback | `assess` funziona sul testo fallback; coverage 0; trust basso/weak. Nessun panic. |
| Cap 20 superato | le fonti extra restano in `sources` ma non in `citations`; `observed_sources=20`; non citabili. |
| `source_id` vuoto | fallback key `source_kind|path|commit_hash`; se anche il fallback è vuoto → fonte scartata. |
| `content` vuoto | citation valida con `excerpt` vuoto; path/commit/id identificano la fonte. |
| `score` NaN/inf | sanitizzato a `0.0` nella citation; trust usa 0 per quel contributo. |
| Duplicato con contenuto diverso | vince la prima istantanea; id stabile; nessun aggiornamento. |
| `reasoning`/`memory` senza path | `path` vuoto omesso nel JSON; `source_id`/`excerpt` restano. |
| Citazioni in code fence/inline code | ignorate: il codice è evidenza, non claim; non contano come invalid. |
| Marker malformato (`[S1; S2]`, `[Source 1]`) | non riconosciuto: non copre, non è contato invalid. Il prompt insegna il formato `[S1]`. |
| `at_ref` | fuori scope: nessun campo trust/citation; payload invariato. |

### Test

#### E15 — unit test (`crates/ibis-search/src/agentic_trust.rs`)

Catalogo:

- `citation_catalog_assigns_ids_in_first_observation_order`
- `citation_catalog_dedupes_repeated_source_id`
- `citation_catalog_falls_back_to_kind_path_commit_for_empty_id`
- `citation_catalog_skips_unidentifiable_hit`
- `citation_catalog_caps_at_20`
- `citation_catalog_keeps_first_snapshot`
- `citation_excerpt_is_bounded_and_whitespace_collapsed`
- `citation_empty_content_keeps_entry`
- `citation_score_is_sanitized`

Trust matrix (input):

| Caso | Claim coperti/totali | observed | cited | top | current | kind | above 0.30 |
|---|---|---:|---:|---:|---|---:|---:|
| A | 4/4 | 3 | 3 | 0.90 | 2/2 | 3 | 3 |
| B | 2/4 | 4 | 2 | 0.60 | 1/2 | 2 | 4 |
| C | 0/3 | 5 | 0 | 0.20 | 0/3 | 1 | 0 |
| D | 0/0 | 0 | 0 | 0.00 | 0/0 | 0 | 0 |
| E | 3/4 | 3 | 3 | 1.00 | 2/2 | 2 | 3 |
| F | 0/3 | 3 | 0 | 0.80 | 1/1 | 1 | 3 |
| G | 2/2 | 2 | 2 | 0.70 | 0/0 (commit-only) | 1 | 2 |
| H | 1/1 | 1 | 1 | 0.90 | 1/1 | 1 | 1 |
| I | 20/20 | 20 (cap da 25) | 20 | 0.90 | 10/10 | 3 | 20 |
| J | 2/2 | 2 | 2 | 0.20 | 2/2 | 2 | 0 |

Trust matrix (output, base 1000):

| Caso | coverage | source_ratio | top_norm | current_ratio | diversity | citation_norm | score | level | weak |
|---|---:|---:|---:|---:|---:|---:|---:|---|---|
| A | 1.000 | 1.000 | 0.900 | 1.000 | 1.000 | 1.000 | **0.985** | high | false |
| B | 0.500 | 0.500 | 0.600 | 0.500 | 0.667 | 0.667 | **0.540** | medium | false |
| C | 0.000 | 0.000 | 0.200 | 0.000 | 0.333 | 0.000 | **0.063** | low | true |
| D | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 | **0.000** | low | true |
| E | 0.750 | 1.000 | 1.000 | 1.000 | 0.667 | 1.000 | **0.867** | high | false |
| F | 0.000 | 0.000 | 0.800 | 1.000 | 0.333 | 0.000 | **0.253** | low | false |
| G | 1.000 | 1.000 | 0.700 | 0.500 | 0.333 | 0.667 | **0.822** | high | false |
| H | 1.000 | 1.000 | 0.900 | 1.000 | 0.333 | 0.333 | **0.885** | medium (weak cap) | true |
| I | 1.000 | 1.000 | 0.900 | 1.000 | 1.000 | 1.000 | **0.985** | high | false |
| J | 1.000 | 1.000 | 0.200 | 1.000 | 0.667 | 0.667 | **0.830** | medium (weak cap) | true |

Boundary test (`trust_level_boundaries`):

| score | weak | level atteso |
|---:|---|---|
| 0.700 | false | high |
| 0.699 | false | medium |
| 0.450 | false | medium |
| 0.449 | false | low |
| 0.700 | true | medium |
| 0.985 | true | medium |
| 0.450 | true | medium |
| 0.449 | true | low |

Weak context test: observed 0 → true; observed 1/above 1 → true; observed 2/above 1 → true; observed 2/above 2 → false.

Coverage matrix (`coverage_claim_heuristic_matches_table`):

| Answer sintetica | Claim | Coperti | coverage | invalid |
|---|---:|---:|---:|---:|
| `The retry loop is bounded by MAX_RETRIES [S1]. It uses exponential backoff [S2].` | 2 | 2 | 1.000 | 0 |
| `The retry loop is bounded by MAX_RETRIES. It uses exponential backoff [S2].` | 2 | 1 | 0.500 | 0 |
| `- The loop is bounded [S1].\n- The backoff starts at 100ms [S2].` | 2 | 2 | 1.000 | 0 |
| `## Summary\nThe loop is bounded [S1].` | 1 | 1 | 1.000 | 0 |
| Fenced code block con `[S9]` dentro, poi `The value is one [S1].` | 1 | 1 | 1.000 | 0 |
| `Yes [S1]. No.` | 2 | 1 | 0.500 | 0 |
| `The loop is bounded [S99].` | 1 | 0 | 0.000 | 1 |
| `[S1].` | 0 | 0 | 0.000 | 0 |
| `Version 3.14 is stable [S1].` | 1 | 1 | 1.000 | 0 |
| `e.g. this is a claim [S1].` | 1 | 1 | 1.000 | 0 |
| `Steps:\n- The loop is bounded [S1].` | 1 | 1 | 1.000 | 0 |
| `The loop is bounded [S1, S2].` | 1 | 1 | 1.000 | 0 |
| `The loop is bounded [S1] and the backoff [S1].` | 1 | 1 | 1.000 | 0 |
| `The loop is bounded [s1].` | 1 | 1 | 1.000 | 0 |
| `The loop is bounded` con `[S1]` dentro inline code | 1 | 0 | 0.000 | 0 |

Altri test:

- `coverage_ignores_code_fences_and_inline_code`
- `coverage_counts_invalid_citations_once_per_claim`
- `coverage_multi_marker_and_case_insensitive`
- `assessment_exposes_counts_for_repair`
- `trust_zero_sources_is_low_and_weak`
- `trust_commit_only_uses_neutral_current_ratio`
- `trust_cap_uses_catalog_size`
- `trust_rounding_is_base_1000`

#### E15 — payload/serde test (`agentic.rs` o `agentic_trust.rs`)

- `payload_shape_is_additive_and_stable`: serializza un `AgenticResult` completo; asserisce `answer`, `sources`, `reasoning_steps`, `citations`, `trust`, `budget_exhausted`, `completion_checked`, `repair_turns`, `invalid_citations`, `search_unavailable`, `stop_reason`.
- `old_agentic_payload_deserializes`: `{"answer":"a","sources":[],"reasoning_steps":[]}` → default per i campi nuovi; `stop_reason=final_answer`; `trust` default low/weak.
- `citations_have_no_line_field`: serializza una citation; asserisce che non esista `line`.
- `sources_wire_shape_unchanged`: `sources[0]` ha `code_chunk_id`, `file_path`, `relevance_score`; nessun `line`.

#### E15 — capture test

- `capture_observation_renders_evidence_block`: due citation → content contiene `EVIDENCE:` e i due `source_id`/path/commit/score/excerpt.
- `capture_observation_without_citations_is_unchanged`: assente/`[]` → content == text.
- `capture_observation_rejects_invalid_citations_json`: JSON rotto → errore, nessuna riga creata.
- `capture_observation_caps_evidence_at_20`: 25 citation → 20 righe.
- `capture_observation_does_not_create_extra_memory_rows`: una sola riga `memory` per cattura (fake DB o live memory estate).
- `capture_observation_evidence_excerpt_is_bounded`: excerpt 160 rune.

#### E15 — integrazione/contratto

- `sampling_e2e_test::ask_project_reasons_through_mcp_sampling` esteso: il payload contiene `"trust"` e `"citations"`; la risposta sampled senza citazioni → `trust.level == "low"`, `coverage == 0.0`, `weak_context == true`.
- `tool_contract_test::tool_contracts_all_58_names_present` verde (58).
- `discovery_snapshot_test::live_discovery_matches_fixture` verde dopo rigenerazione per `capture_observation.citations_json`.
- `at_ref_payload_unchanged`: il ramo at_ref continua a restituire `Vec<SearchResult>` / `{degraded, results}` senza trust.
- `ibis eval retrieval` baseline invariata (E15 non tocca il ranking).

### Accettazione

1. `cargo test -p ibis-search agentic_trust` verde con la matrice esatta (A–J, boundary, coverage).
2. `cargo test -p ibis-search` verde; nessun test esistente rotto.
3. `cargo test -p ibis-server` verde: 58 tool, fixture discovery, sampling e2e.
4. Un payload `ask_project` reale contiene `citations` e `trust`; un payload vecchio decodifica con i default.
5. `sources` e `reasoning_steps` hanno la stessa forma di prima; nessun `line`.
6. `capture_observation` con `citations_json` salva l'evidenza nel content, senza nuove righe/edge; senza il parametro il content è invariato.
7. Il trust è calcolato senza alcuna chiamata LLM (test con soli dati sintetici).
8. `at_ref` invariato; nessun trust/citation sul ramo strutturato.
9. La fixture discovery è rigenerata per `capture_observation.citations_json` e `memory_list.store`; `ask_project` schema invariato.

---

## E14 — Loop engineering per il ReAct di `ask_project` (consumatore)

### Aggancio

- `ask_project_agentic(&self, query, branch_context) -> Result<AgenticResult>` — agentic.rs:62-66.
- Loop `for i in 0..AGENTIC_MAX_STEPS` — agentic.rs:89-254; `AGENTIC_MAX_STEPS = 5` — agentic.rs:54.
- `GenerationConfig { temperature: 0.2, max_output_tokens: 2000, route_hint: local:L }` — agentic.rs:93-99.
- Cost tracking su `system:search_agentic` — agentic.rs:104-115; `CostTracker::record_usage` — cost.rs:33-63.
- Parsing azione (primo marker per posizione) — agentic.rs:133-148; `FINAL ANSWER` — agentic.rs:150-153; `SEARCH` — agentic.rs:154-199; empty-query nudge — agentic.rs:201-211; search execution — agentic.rs:220-243; no-action nudge — agentic.rs:244-253.
- Fallback answer — agentic.rs:259-263; `format_observation` — agentic.rs:271-331; `SYSTEM_PROMPT` — agentic.rs:36-52.
- Config: `SearchConfig` (`search.fusion`, `search.candidates`, `search.query_expansion`, `search.rerank`) — config.rs:63-97; `Config.search` — config.rs:487-488.
- Wiring: `AppState::rebuild_search` — state.rs:160-185 (knob a 175-180); `main.rs` build — main.rs:734-756 (knob a 748-755); eval — eval_mode.rs:411-417.
- Token accounting: `UsageMetadata` — types.rs:126-150; `estimate_tokens = len/3+1` — types.rs:14-16.
- D12: `ResponseCacheCallSite` — types.rs:45-55; `ResponseCacheSpec` — types.rs:68-82; agentic no-cache — agentic.rs:97-98.
- MCP handler — tools_memory.rs:1234-1302 (agentic a 1298-1301).

### Flusso attuale

1. **Solo max-5-step.** Il loop itera `AGENTIC_MAX_STEPS` volte (agentic.rs:89). Non c'è budget token, no-progress detection, completion check, né repair. Il modello decide da solo quando emettere `FINAL ANSWER` (agentic.rs:150-153); se non lo fa, il loop termina per esaurimento step e restituisce il fallback (agentic.rs:259-263).
2. **Costi tracciati ma non frenati.** Se `usage_metadata` è presente, `record_usage` incrementa `system:search_agentic` (agentic.rs:104-115; cost.rs:33-63). Se è assente, non viene registrato nulla. Il costo non è un freno: non c'è un budget locale.
3. **Nessuna no-progress detection.** Ogni `SEARCH` viene eseguita, anche se identica alla precedente (agentic.rs:220-243). Una query vuota riceve un nudge (agentic.rs:201-211); un'azione assente riceve un nudge (agentic.rs:244-253).
4. **Nessun completion check.** La `FINAL ANSWER` viene accettata così com'è (agentic.rs:150-153). Non ci sono citazioni, coverage, repair, né flag di qualità. E15 aggiunge il produttore; E14 deve consumarlo.
5. **Errori AI propagati.** `generate_content` usa `?` (agentic.rs:90-102): un errore AI mid-loop fa fallire l'intero ask, anche se ci sono già fonti utili.
6. **DB degradato.** `ask_project` può restituire `DatabaseNotConnected` (search.rs:188-190); il loop lo trasforma in osservazione `"OBSERVATION: Search failed: ..."` e continua (agentic.rs:234-242). Ogni `SEARCH` successivo ritenta il DB.
7. **Fonti in HashMap.** `unique_sources` è una `HashMap` keyed su `id` (agentic.rs:86-87); la flatten `into_values()` (agentic.rs:257) non preserva l'ordine di prima osservazione. E15 richiede ordine stabile per gli id.
8. **Query vuota.** Non c'è una validazione upfront: una query vuota consuma una chiamata al modello (agentic.rs:71-83) e riceve il nudge solo se il modello emette un `SEARCH` vuoto.
9. **Test estate mancante.** Il module doc dichiara che i layer di parsing sono pinnati dall'“original test estate” (agentic.rs:1-5: quoted multiline, line fallback, chatter stripping, empty-query nudge, no-action nudge, search-failure continuation), ma il file contiene solo il test D10 `agentic_route_hint_is_local_l` (agentic.rs:364-373). E14 deve ripristinare la suite di regressione del parser.

### Trasformazione

#### E14.1 Config: namespace, default, clamp, retrocompatibilità

**Namespace scelto: `search.agentic.*`**, annidato in `SearchConfig` (config.rs:63-97) sotto `Config.search` (config.rs:487-488).

Motivazione:

- `search.*` è già il namespace del comportamento di retrieval/ask (`fusion`, `candidates`, `query_expansion`, `rerank`); il loop ReAct è comportamento di search, non di provider.
- `ai.*` è il namespace di provider/routing/embedding/consolidation/decide; il loop non configura un modello (D10 fissa `local:L`).
- `optimizer.*` **non esiste** nel config di prodotto (nota C9 nel master queue): non va inventato.
- Un top-level `agentic` sarebbe senza precedenti e fuori dalla gerarchia di `SearchConfig`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgenticConfig {
    #[serde(rename = "max_steps")]
    pub max_steps: u32,
    #[serde(rename = "token_budget")]
    pub token_budget: u32,
    #[serde(rename = "max_repeated_searches")]
    pub max_repeated_searches: u32,
    #[serde(rename = "completion_check")]
    pub completion_check: String,
    #[serde(rename = "completion_min_coverage")]
    pub completion_min_coverage: f64,
}

impl Default for AgenticConfig {
    fn default() -> Self {
        Self {
            max_steps: 5,
            token_budget: 120_000,
            max_repeated_searches: 2,
            completion_check: "warn".to_string(),
            completion_min_coverage: 0.60,
        }
    }
}
```

`SearchConfig` guadagna `#[serde(rename = "agentic")] pub agentic: AgenticConfig`.

| Chiave | Default | Clamp | Comportamento degraded/unknown |
|---|---:|---|---|
| `search.agentic.max_steps` | 5 | `[1, 10]` | 0 → 1; >10 → 10; warn log sul clamp. |
| `search.agentic.token_budget` | 120000 | `0` = off; positivo `[4000, 500000]` | 1..3999 → 4000; >500000 → 500000; warn log. |
| `search.agentic.max_repeated_searches` | 2 | `[1, 5]` | 0 → 1; >5 → 5; warn log. Il freno non è disattivabile. |
| `search.agentic.completion_check` | `warn` | `off` \| `warn` \| `enforce` | unknown/empty → `warn` + warn log. |
| `search.agentic.completion_min_coverage` | 0.60 | `[0.0, 1.0]` | fuori range → clamp; usato solo in `enforce` (e nel log di `warn`). |

Perché 120000: il worst-case osservato di 5 step con 15 hit × 2000 rune è ~110–140k token cumulativi (prompt grows + output). 120k lascia passare la maggior parte dei 5-step e ferma le osservazioni patologiche; chi vuole un cap più aggressivo lo abbassa. `max_steps` resta il freno hard.

`AgenticSettings` in `ibis-search` (plain struct, come `FusionMode`/`RerankMode`):

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgenticSettings {
    pub max_steps: usize,
    pub token_budget: u64,
    pub max_repeated_searches: usize,
    pub completion_check: CompletionCheckMode,
    pub completion_min_coverage: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionCheckMode {
    Off,
    #[default]
    Warn,
    Enforce,
}

impl AgenticSettings {
    pub fn from_config(cfg: &ibis_core::config::AgenticConfig) -> Self { /* clamp + resolve */ }
}
```

- `SearchService` guadagna `pub agentic: AgenticSettings`; `Default` = `AgenticSettings::default()`.
- Wiring in `state.rs:160-185` e `main.rs:734-756`: `service.agentic = AgenticSettings::from_config(&cfg.search.agentic);`. `eval_mode.rs:411-417` non chiama l'agentic, ma il default copre il caso; se in futuro l'eval lo chiama, va wired.
- Retrocompatibilità: `SearchConfig` ha `#[serde(default)]`; un config senza `search.agentic` carica i default; chiavi sconosciute sono ignorate (nessun `deny_unknown_fields`). I valori esistenti di `search.*` non cambiano.
- Degraded mode: la risoluzione non fallisce mai; unknown → default + warn; clamp → warn. `token_budget=0` è l'unico opt-out esplicito.
- Documentazione: aggiungere le chiavi a `docs/ai_and_settings.md` (accanto a `ai.response_cache` e alla mappa di routing — docs/ai_and_settings.md:67, 138-147).

#### E14.2 Budget token per ask

1. Stato locale: `budget_used: u64`, `budget_exhausted: bool`.
2. Prima di ogni `generate_content`:
   - costruisce `prompt_text` concatenando tutti i `Content.parts.text` della history (system/question + turni + osservazioni).
   - `projected = budget_used + estimate_tokens(&prompt_text) as u64 + AGENTIC_MAX_OUTPUT_TOKENS as u64` (reservation 2000).
   - se `token_budget > 0 && projected > token_budget` → stop **prima** della chiamata: `budget_exhausted = true`, `stop_reason = BudgetExhausted`, nessun errore.
   - `AGENTIC_MAX_OUTPUT_TOKENS = 2000` va estratto come costante: oggi è il letterale `max_output_tokens: 2000` in `GenerationConfig` (agentic.rs:95).
3. Dopo ogni chiamata:
   - se `usage_metadata` è presente e `prompt + candidates > 0`: `spent = prompt.max(0) + candidates.max(0)`; altrimenti se `total_token_count > 0`: `spent = total`; altrimenti fallback `estimate_tokens(prompt_text) + estimate_tokens(response_text)`.
   - `budget_used = budget_used.saturating_add(spent)`.
   - registra il costo su `system:search_agentic`:
     - usage presente → `record_usage("system:search_agentic", prompt, candidates, cached_tokens())` (come oggi, agentic.rs:104-115).
     - usage assente → `record_usage("system:search_agentic", estimate(prompt), estimate(response), 0)` (nuovo: il costo non resta invisibile).
   - `budget_used` conta token **grezzi**; il `CostTracker` applica lo sconto D13 (`billed = prompt − 0.9×cached`, cost.rs:42-43) solo al record `tokens_used`. Sono due metriche diverse: budget = compute/contesto, costo = billing.
4. A esaurimento:
   - se non c'è ancora una `FINAL ANSWER` → `answer = AGENTIC_BUDGET_ANSWER` (“I could not complete the investigation within the token budget. The evidence gathered so far is attached.”), `sources`/`citations`/`trust` calcolati, `completion_checked=false`, `repair_turns=0`, `budget_exhausted=true`, `stop_reason=BudgetExhausted`. Nessun errore.
   - se c'è una `FINAL ANSWER` pending (repair bloccato dal budget) → tieni l'answer migliore, `completion_checked=true`, `repair_turns=0`, `budget_exhausted=true`, `stop_reason=BudgetExhausted`.
   - se il budget blocca la prima chiamata (prompt enorme) → fallback, nessuna chiamata AI, flag.
5. Il budget non sopprime i costi già sostenuti: ogni chiamata eseguita registra il suo `record_usage`; il budget controlla solo la chiamata successiva.
6. Il budget **non** include gli embedding/rerank di `ask_project`: quelli sono tracciati su `system:search_vector`/`system:search_rerank` e sono bounded da `max_steps` (al più 2 embedding per search con A4, più l'eventuale rerank A3). Documentare questa separazione.
7. Soft cap: se il provider riporta più token della stima, `budget_used` può superare il budget di una chiamata; il check successivo ferma. `max_steps` resta il cap hard.

#### E14.3 No-progress detection

1. Normalizzazione della query:
   - `trim()`, `to_lowercase()`, `split_whitespace().join(" ")`.
   - rimuove la punteggiatura ai bordi (`.,;:!?()[]{}<>"'`), non quella interna: `src/main.rs`, `foo-bar`, `a_b` restano distinti.
   - se il risultato è vuoto → non è una repeated search; si applica il nudge empty-query esistente (agentic.rs:201-211).
2. `fingerprint = hex(sha256(normalized))[..16]` (sha2 è già dipendenza di `ibis-search`, Cargo.toml:19). Il fingerprint è solo una chiave/log; la query normalizzata può essere usata direttamente nei test.
3. Conteggio:
   - `last_fingerprint: Option<String>`, `consecutive_count: usize`, `no_progress_nudged: bool`.
   - un `SEARCH` con fingerprint uguale all'ultimo incrementa `consecutive_count`; uno diverso resetta a 1.
   - `max_repeated_searches = N` significa **al massimo N esecuzioni consecutive** della stessa query. Con default 2: la 1ª esegue, la 2ª esegue, la 3ª non esegue e riceve il nudge, la 4ª ferma il loop.
4. Al superamento:
   - non esegue la search (risparmia costo/DB).
   - pusha un'osservazione actionable:
     ```text
     OBSERVATION: This exact search was already executed N times and returned the same evidence. Do not repeat it. Refine the query with different terms or emit FINAL ANSWER with the evidence you have.
     ```
   - `no_progress_nudged = true`; il turno consuma uno step e il budget.
   - se il modello ripete la stessa query dopo il nudge → `stop_reason=NoProgress`, break. Cap: un solo nudge.
   - se il modello cambia query → reset `consecutive_count=1`, `no_progress_nudged=false` per la nuova query; il loop continua.
5. Solo le `SEARCH` consecutive identiche contano. A, B, A non è un doom loop (può essere un refinement legittimo) e viene eseguito. Una search fallita conta come tentativo (la query è stata provata); una search non eseguita per DB unavailable non incrementa il conteggio.
6. Interazione con il DB brake (§E14.6): il check no-progress precede l'esecuzione; se `search_unavailable` è true, la search non viene eseguita e non conta come repeated.
7. Se `max_steps` finisce prima del tentativo che avrebbe prodotto l'hard stop, il loop termina con `MaxSteps` (non `NoProgress`): il nudge ha già informato il modello. I test del doom loop usano `max_steps >= 4` per osservare `NoProgress`.

#### E14.4 Completion check citation-based

1. Alla ricezione di una `FINAL ANSWER`, E15 calcola `assessment = assess(&catalog, &answer)` (coverage, counts, invalid, trust). E14 non ricalcola nulla.
2. `completion_checked`:
   - `off` → `false`; nessun repair; l'answer è accettata; il trust è comunque calcolato da E15.
   - `warn` → `true`; log `tracing::warn!` con coverage/covered/total/invalid/threshold; l'answer è accettata; nessun repair.
   - `enforce` → `true`; se `assessment.coverage_bp >= threshold_bp` → accetta; altrimenti repair (se possibile).
3. `threshold_bp = round(clamp(completion_min_coverage, 0, 1) * 1000)`. Confronto in base 1000, non floating point.
4. Repair (max 1):
   - disponibile solo se `steps_used < max_steps` e il budget non blocca la chiamata.
   - pusha un'osservazione actionable:
     ```text
     OBSERVATION: FINAL ANSWER coverage is 0.500 (2/4 claims cited, 1 invalid citation) below the required 0.600. Re-emit FINAL ANSWER and add [S<n>] citations to the uncited claims, using only ids shown in OBSERVATION. If a claim has no supporting evidence, remove it or state that it is unverified. Do not invent ids.
     ```
   - `repair_turns = 1` (tentato); il turno consuma uno step e il budget.
   - il nuovo answer viene riassessato da E15; si tiene quello con `coverage_bp` maggiore; tie → l'originale (meno churn).
   - se il repair produce una `SEARCH`, la search viene eseguita (soggetta a budget/no-progress); l'answer pending resta quello originale. Il primo `FINAL ANSWER` successivo viene riassessato e confrontato con il pending, senza un secondo repair (cap 1); se il loop finisce senza un nuovo `FINAL ANSWER`, si tiene il pending con `stop_reason=MaxSteps`/`BudgetExhausted` e `completion_checked=true`.
   - se il repair produce un'azione assente, si pusha il nudge “Please continue…” e si continua; il pending resta e il prossimo `FINAL ANSWER` viene confrontato senza secondo repair.
5. Esiti:
   - pass (o `off`/`warn`) → `stop_reason=FinalAnswer`.
   - enforce fail, repair eseguito e ancora sotto soglia → `stop_reason=CompletionLowCoverage`.
   - enforce fail, nessuno spazio per il repair (ultimo step) → `stop_reason=CompletionLowCoverage`, `repair_turns=0`.
   - enforce fail, budget blocca il repair → `stop_reason=BudgetExhausted`, `budget_exhausted=true`, `repair_turns=0`.
6. Se il modello non cita nulla: coverage 0, trust basso, **non è un errore**. In `warn` l'answer è accettata; in `enforce` si tenta un repair; se fallisce, `CompletionLowCoverage` con l'answer migliore.
7. Citation id validi vs inventati: solo gli id del catalogo coprono; gli altri sono `invalid_citations`. Un marker in un heading/code fence non è un claim e non conta.
8. Nessun verifier LLM: il check è puro parsing + soglia. Il repair è un turno dello **stesso** modello, non un secondo modello giudice.
9. State machine del loop (sintesi):

| Evento | Condizione | Azione | `stop_reason` |
|---|---|---|---|
| `FINAL ANSWER` | pass / `off` / `warn` | accetta, assess finale | `FinalAnswer` |
| `FINAL ANSWER` | `enforce` fail, repair disponibile | push repair, `repair_turns=1`, continua | — |
| `FINAL ANSWER` | `enforce` fail, nessuno step | accetta il migliore | `CompletionLowCoverage` |
| `FINAL ANSWER` | `enforce` fail, budget blocca repair | accetta il migliore, flag budget | `BudgetExhausted` |
| repair `FINAL ANSWER` | coverage migliore / peggiore | sceglie il migliore; se sotto soglia | `FinalAnswer` / `CompletionLowCoverage` |
| repair `SEARCH` | step disponibili | esegue search, pending answer resta | — |
| repair `SEARCH` | step esauriti | accetta pending | `MaxSteps` |
| nessuna `FINAL ANSWER` | step esauriti | fallback max-step | `MaxSteps` |
| nessuna `FINAL ANSWER` | budget esaurito | fallback budget | `BudgetExhausted` |
| search ripetuta | nudge già usato | break | `NoProgress` |
| AI error | ≥1 step completato | best-effort | `AiError` / `ContextOnly` |
| AI error | 0 step completati | `Err` (comportamento attuale) | — |
| query vuota | subito | short-circuit, nessuna AI call | `EmptyQuery` |

#### E14.5 Prompt: estensione D13 byte-stabile

1. `SYSTEM_PROMPT` (agentic.rs:36-52) resta **byte-identico**: è il prefisso stabile e il contratto esistente. Non si modifica il testo.
2. Nuova costante `pub const CITATION_PROTOCOL: &str` in `agentic.rs`, appesa una sola volta e preceduta da `\n\n` (separatore dal prefisso `SYSTEM_PROMPT`):
   ```text

   CITATION PROTOCOL:
   - Every citable OBSERVATION line starts with its id in square brackets, e.g. [S1].
   - In FINAL ANSWER, support each factual claim with one or more ids in square brackets, e.g. "The retry loop is bounded by MAX_RETRIES [S2]."
   - Use only ids that appear in OBSERVATION. Never invent or renumber ids.
   - A claim without a citation is treated as unsupported by the completion check.
   - If no evidence supports a claim, say so explicitly instead of citing an unrelated source.
   ```
3. Assembly order (D13):
   - `SYSTEM_PROMPT` → `CITATION_PROTOCOL` (blocco stabile) → eventuale `TOPOLOGICAL CONTEXT` (variabile) → `Question: {query}` (ultima).
   - nessun timestamp/request-id nel prefisso; il branch context resta dopo il blocco stabile (agentic.rs:71-83).
4. Snapshot test:
   - `prompt_prefix_is_byte_stable`: due branch context diversi condividono il prefisso `SYSTEM_PROMPT + CITATION_PROTOCOL`; il branch context differisce; la question è l'ultima.
   - `citation_protocol_appended_once`: il prompt contiene una sola occorrenza di `CITATION PROTOCOL`.
   - `system_prompt_hash_snapshot`: `sha256(SYSTEM_PROMPT)` pinnato; un cambio accidentale del prompt rompe il test e richiede un aggiornamento deliberato.
   - `citation_protocol_hash_snapshot`: stesso pattern per il blocco nuovo.
5. Backward compat: una risposta senza citazioni si parsa come oggi; `answer` resta il testo dopo `FINAL ANSWER:`; `trust` è basso; nessun errore. Il parser dei marker non interferisce con `SEARCH`/`FINAL ANSWER`.
6. Formattazione osservazione: `format_observation(&[ObservedSource])` antepone `[S<n>] ` alla riga citabile; il resto (label, score, source_kind, is_current, status, commit, context, issue, commit) resta invariato (agentic.rs:271-331). Le fonti oltre il cap non hanno prefisso.

#### E14.6 Failure mode

| Scenario | Comportamento |
|---|---|
| **AI error mid-loop** | 0 step completati → `Err(SearchError::other("AI generation failed: ..."))` (comportamento attuale). ≥1 step → `Ok` best-effort: `stop_reason=AiError`, answer = pending/fallback, sources/citations/trust calcolati, `completion_checked` coerente. |
| **`ContextOnly`** | 0 step → `Err` (stesso messaggio attuale). ≥1 step → `Ok`, `stop_reason=ContextOnly`, best-effort. |
| **`budget 0`** | Freno disabilitato: il loop è bounded solo da `max_steps`; nessun `BudgetExhausted`. |
| **Budget < primo prompt** | Nessuna chiamata AI; fallback budget; `budget_exhausted=true`, `stop_reason=BudgetExhausted`; trust 0/weak. |
| **Query vuota** | Short-circuit prima del check AI: `Ok`, answer “Please provide a non-empty question.”, `stop_reason=EmptyQuery`, `completion_checked=false`, nessuna AI/DB call. |
| **Query ripetuta** | Nudge actionable, search non eseguita; se ripetuta ancora → `NoProgress`. |
| **No-progress** | Break con `stop_reason=NoProgress`; evidence/sources già raccolti restano; trust calcolato. |
| **Max step** | Break `MaxSteps`; se non c'è `FINAL ANSWER`, fallback max-step; sources/trust presenti. |
| **DB degradato** | Prima search con `DatabaseNotConnected` → `search_unavailable=true`, osservazione “Search is unavailable (database not connected). Answer from the evidence you have or state the limitation.”; le SEARCH successive non vengono eseguite; il modello può emettere FINAL. Nessun errore (salvo 0 step AI). |
| **Search failure non-DB** | Osservazione `"OBSERVATION: Search failed: ..."`, il loop continua; il modello può cambiare query. `search_unavailable` resta false. |
| **AI response vuota** | Push nudge “Please continue…”; il turno conta come step; nessun panic. |
| **Malformed SEARCH** | Parser esistente: quoted multiline, line fallback, chatter stripping, empty-query nudge; nessuna regressione. |
| **FINAL senza citazioni** | Coverage 0, trust basso; warn accetta, enforce ripara una volta; non è un errore. |
| **Citation invalide** | Non coprono; `invalid_citations` conta; repair se enforce; nessun errore. |
| **Repair peggiore** | Si tiene l'originale (tie → originale); `CompletionLowCoverage` se sotto soglia. |
| **Repair error** | Si tiene il pending answer; `stop_reason=AiError`, `completion_checked=true`, `repair_turns=1`. |
| **Budget esaurito durante repair** | Si tiene il pending answer; `stop_reason=BudgetExhausted`, `budget_exhausted=true`, `completion_checked=true`, `repair_turns=0`. |
| **Usage assente** | Stima `estimate_tokens`; costo registrato con la stima; budget coerente. |
| **Usage negativo** | `.max(0)`; nessun panic; eventualmente fallback stima se tutto zero. |
| **`max_steps=1`** | Una sola chiamata: se è SEARCH, fallback `MaxSteps`; se è FINAL, completion check senza spazio di repair. |
| **`max_steps` clamp** | 0→1, >10→10; warn log. |
| **DB down + AI down** | 0 step AI → `Err`; il flag `search_unavailable` non è osservabile perché non c'è risultato. |
| **Cap citation 20** | Le fonti extra restano in `sources` ma non sono citabili; un id inventato oltre il cap è invalido. |

#### E14.7 Interazione con E15

- Ordine: E15 osserva/assegna id durante il loop; alla `FINAL ANSWER` E15 `assess` calcola coverage/trust; E14 legge `assessment.trust.coverage` e i conteggi e decide accept/repair/stop; E15 produce il trust finale dell'answer scelta.
- E14 non calcola coverage e non duplica la formula; E15 non controlla il loop.
- `citations` è la stessa lista che alimenta il payload e `capture_observation`; E14 non la filtra e non la riordina.
- `invalid_citations` è calcolato da E15 e copiato da E14 nel flag; nessun doppio conteggio.
- Il completion check non chiama un verifier LLM; il repair è un turno dello stesso modello con route hint `local:L`.
- Se `completion_check=off`, E15 calcola comunque il trust; E14 non fa repair e `completion_checked=false`.

#### E14.8 Test e accettazione

**Parser regression (ripristino della test estate dichiarata):**

- `parses_quoted_multiline_search_query`
- `parses_line_based_search_query`
- `strips_chatter_after_sentence_boundary`
- `empty_search_query_pushes_nudge_without_search`
- `no_action_pushes_continue_nudge`
- `search_failure_pushes_observation_and_continues`
- `final_answer_extracts_text_after_marker`
- `first_marker_wins_when_both_present`
- `markers_are_case_insensitive`

**Config (`ibis-core`):**

- `agentic_config_defaults` → 5 / 120000 / 2 / warn / 0.60.
- `agentic_config_overrides` → JSON completo.
- `agentic_config_clamps` → `max_steps` 0→1 e 99→10; `token_budget` 1→4000 e 999999→500000; `max_repeated_searches` 0→1 e 99→5; `completion_min_coverage` -1→0.0 e 2→1.0.
- `agentic_config_unknown_completion_check_resolves_warn`.
- `agentic_config_missing_block_uses_defaults`.
- `agentic_settings_from_config_is_clamped`.

**Budget:**

- `budget_exhausted_before_first_call_returns_flag_not_error`
- `budget_stops_after_partial_progress_keeps_evidence`
- `budget_zero_disables_the_brake`
- `usage_metadata_takes_precedence_over_estimate`
- `usage_absent_estimates_and_records_cost`
- `negative_usage_is_clamped`
- `budget_uses_raw_tokens_not_billed_tokens`

**No-progress:**

- `repeated_identical_search_is_not_executed_and_nudges`
- `second_identical_search_after_nudge_stops_no_progress`
- `different_search_resets_repeat_counter`
- `alternating_queries_are_not_consecutive_repeats`
- `normalization_ignores_case_whitespace_and_edge_punctuation`
- `normalization_preserves_internal_path_punctuation`
- `empty_query_is_not_a_repeated_search`

**Completion:**

- `completion_enforce_repairs_once_and_accepts_cited_answer`
- `completion_enforce_keeps_better_answer`
- `completion_enforce_accepts_below_threshold_after_one_repair`
- `completion_warn_accepts_without_repair`
- `completion_off_skips_check_but_computes_trust`
- `completion_fail_at_last_step_has_no_repair`
- `completion_repair_search_keeps_pending_answer`
- `completion_repair_error_keeps_pending_answer`
- `completion_budget_block_keeps_pending_answer`
- `invalid_citations_are_counted`
- `no_citations_is_low_trust_not_error`
- `no_claims_coverage_is_zero`

**Prompt:**

- `prompt_prefix_is_byte_stable`
- `citation_protocol_appended_once`
- `system_prompt_hash_snapshot`
- `citation_protocol_hash_snapshot`
- `question_is_last`
- `response_without_citations_still_parses`

**Failure modes:**

- `ai_error_after_one_step_returns_best_effort`
- `ai_error_before_any_step_propagates`
- `context_only_after_one_step_is_distinct_stop_reason`
- `context_only_before_any_step_propagates`
- `empty_query_short_circuits_without_ai_call`
- `db_unavailable_sets_search_unavailable_and_skips_retries`
- `max_steps_one_returns_fallback`
- `malformed_search_still_nudges`

**Payload/cost/D-rules:**

- `agentic_result_old_json_deserializes_with_defaults`
- `agentic_result_new_json_keeps_answer_sources_steps`
- `agentic_result_serializes_all_new_flags`
- `agentic_calls_never_use_response_cache` (D12)
- `agentic_calls_use_local_l_route_hint` (D10, incluso repair)
- `agentic_cost_recorded_once_per_call`
- `agentic_cost_recorded_with_estimate_when_usage_absent`

**Integrazione:**

- `sampling_e2e_test::ask_project_reasons_through_mcp_sampling` verde (payload con trust/citations).
- `cargo test -p ibis-server` verde (58 tool, discovery, sampling).
- `ibis eval retrieval` baseline invariata (nessuna modifica al ranking).

**Accettazione:**

1. Un ask forzato con budget basso restituisce `budget_exhausted=true`, `stop_reason="budget_exhausted"`, answer non vuota, `sources`/`citations`/`trust` presenti; nessun errore MCP.
2. Un doom loop (stessa query ripetuta) esegue al più `max_repeated_searches` search, poi nudge, poi `stop_reason="no_progress"`; il costo è bounded.
3. `completion_check=enforce` ripara una volta: pass → `final_answer`; fail → `completion_low_coverage`; `completion_check=warn/off` non cambia l'answer.
4. I flag (`budget_exhausted`, `completion_checked`, `repair_turns`, `invalid_citations`, `search_unavailable`, `stop_reason`) sono nel payload e coerenti con la state machine.
5. Nessuna regressione dei parsing test (la suite viene ripristinata e pinnata).
6. Costo per ask prevedibile: chiamate al modello ≤ `max_steps`; token di generazione ≤ `token_budget` + al più una chiamata di overshoot; embedding/rerank ≤ `max_steps` × (2 embedding + 1 rerank).
7. `cargo clippy -D warnings` verde; `cargo test` workspace verde.
8. `ask_project` non usa response cache (D12) e usa `local:L` per ogni chiamata, repair incluso (D10).

---

## Dipendenze residue e rischi

### Dipendenze

- **C8 (Parte I):** E15 definisce il blocco `EVIDENCE:` e il parametro `citations_json`; C8 può consumarli nella pipeline Observations. Nessuna modifica E15 a `memory_consolidate.rs`/`insights.rs`: il confine è `capture.rs` + handler/registry.
- **E16 (durable tasks):** lo stato del loop (budget, no-progress, repair) è in-memory. E16 potrà persisterlo; fuori scope v1.
- **A1 (eval retrieval):** resta il guardrail del ranking; E14/E15 non cambiano `ask_project`. Un eval agentic (qualità risposta/trust) non esiste e non è in v1.
- **D10/D12/D13:** già atterrati; E14 li usa (route hint pinnato, no cache, prompt stabile). Un cambio futuro di D10/D13 deve aggiornare gli snapshot E14.
- **Config docs:** `docs/ai_and_settings.md` va aggiornato con `search.agentic.*`.
- **VSIX badge:** follow-up; consuma `trust`/`citations` senza modifiche al payload.
- **at_ref trust:** follow-up v2; richiede un envelope o un tool dedicato.

### Rischi

1. **Trust come proxy, non verità.** Coverage misura la presenza di citazioni, non l'entailment. Un modello può citare una fonte irrilevante e ottenere trust alto. Mitigazione: presentarlo come segnale, non come garanzia; `weak_context` e `invalid_citations` aiutano; un verifier LLM è escluso per costo (L1).
2. **Comparabilità cross-config.** `top_score` dipende da `search.fusion`/`search.rerank` e dalla scala del canale. Il clamp a 1.0 è grossolano; il trust è comparabile nello stesso config, non tra config diversi. Mitigazione: usarlo per-ask, non come KPI aggregato senza calibrazione.
3. **Prompt più lungo.** Il citation protocol e le osservazioni con `[S<n>]` aumentano i token; su modelli locali piccoli può ridurre la qualità. Mitigazione: default `completion_check=warn`, budget, snapshot; misurare su sampling/e2e.
4. **Repair costoso.** Un repair turn consuma step e budget; se il modello non collabora, si ferma con `CompletionLowCoverage` (non un loop infinito). Cap 1.
5. **`estimate_tokens` grossolano.** `len/3+1` è byte-based: sovrastima testi non-ASCII, sottostima codice compatto. Il budget è una stima; `max_steps` è il cap hard.
6. **Contesto locale.** Un budget di 120k può superare la context window di un modello locale; il provider può troncare o fallire. Mitigazione: `max_steps`, osservazioni bounded, budget configurabile; un budget context-window-aware è futuro.
7. **Ordine `sources` cambiato.** Il passaggio da HashMap a prima-osservazione cambia l'ordine del payload per client esistenti; la forma non cambia. Documentare nel changelog.
8. **Schema `capture_observation` cambiato.** `citations_json` è opzionale ma la fixture discovery va rigenerata; i client che pinnano lo schema devono accettare la nuova property. 58 tool invariati.
9. **Test estate agentic mancante.** Il module doc la dichiara ma il file ha solo il test D10; E14 la ripristina. Senza, ogni modifica al parser è a rischio.
10. **Modello che cita in code fence/heading.** I marker lì sono ignorati; il modello può credere di aver citato. Il repair message lo corregge; il prompt lo istruisce.
11. **`trust.observed_sources` post-cap.** Un ask con >20 fonti ha `observed_sources=20`; il consumer non vede il totale grezzo. `sources.len()` resta disponibile per quel dato.
12. **C8 overlap.** Se C8 vuole una struttura più ricca dell'`EVIDENCE:` testuale, il formato va coordinato; v1 è deliberatamente testuale e bounded per non duplicare memoria.

---

## Prossimo passo

1. Implementare E15 prima (tipi, catalogo, `assess`, payload, capture) con la matrice di test; poi E14 (config, budget, no-progress, completion, prompt, parser regression).
2. Aggiornare `docs/ai_and_settings.md` con `search.agentic.*` e la semantica di `trust`/`coverage`.
3. Rigenerare la fixture discovery per `capture_observation.citations_json` e `memory_list.store` (comando ignorato in `discovery_snapshot_test.rs:198-213`).
4. Eseguire `cargo test` workspace + `cargo clippy -D warnings` + `ibis eval retrieval` (baseline invariata).
5. WQ-5 è già aggiornato al merge; l'implementazione parte dall'ordine complessivo in fondo a questo documento.

---

## Review del merge (2026-11-09)

Correzioni applicate ai draft durante la riconciliazione:

1. **C8 — Q2/Q3 bounded:** le query sugli archi `derived_from` erano filtrate solo per progetto/kind; ora sono limitate a `$memory_ids` (il set di Q1, ≤ 500). Q4/Q5/Q6 restano limitate da `$commits`/`$issues`/`$files`. Il costo del reconcile è lineare nel set bounded, non nello store.
2. **C8 — VSIX `derived`:** il draft affermava che il pre-disable del menu era “gratis”; la UI VSIX oggi tratta come read-only solo `store === "git"` (`memory-tree.js:225-245`). Corretto: i guard server sono la difesa autoritativa; il badge/menu per `derived` è un follow-up UI, non un requisito di correttezza.
3. **C8 — `memory_list` schema:** il nuovo parametro opzionale `store` cambia lo schema di input del tool, quindi la fixture `discovery_tools.json` va rigenerata anche per `memory_list`, non solo per le descrizioni.
4. **C8 — anchor del contract test:** corretto `tool_contract_test.rs:83-89` → `:91-102` (la assert sui 58 tool è lì).
5. **C8 ↔ E15:** chiarito che C8 v1 non dipende dal blocco `EVIDENCE:` di `capture_observation`; il ponte è opzionale e verrà consumato in una fase successiva, senza dipendenza circolare.
6. **E14/E15:** il contratto condiviso era già unico nel draft (sezione 0); nessuna modifica di sostanza. Restano le decisioni: E15 owner di `citations`/`trust`/`coverage`, E14 consumatore per budget/no-progress/completion, nessun verifier LLM, nessun costo LLM in E15.

## Ordine di implementazione complessivo

| # | Stream | Contenuto | Dipende da | Note |
|---|---|---|---|---|
| 1 | **C8 step 1** | read-only foundation (`STORE_DERIVED`, guard update/delete/share/supersede, hard delete trusted, `memory_list(store)`, `memory_search` projection, export exclusion, registry descriptions) | — | Rilasciabile con `ai.observations.enabled=false`, zero LLM. |
| 2 | **E15** | catalogo citazioni, `assess`, trust/coverage, payload additivo, `capture_observation.citations_json` | — | Può procedere in parallelo a C8 step 1–4; è il contract owner. |
| 3 | **C8 step 2–4** | firme/componenti pure, 7 query, diff, persist, esclusioni in `select_observations`/`pending_observation_projects`, discovery a due rami | C8 step 1 | Ancora zero LLM; live test di idempotenza/split/merge. |
| 4 | **E14** | config `search.agentic.*`, budget, no-progress, completion check, prompt citation, parser regression | E15 | Consuma `trust.coverage`/`citations`; nessun ricalcolo. |
| 5 | **C8 step 5–6** | sintesi LLM, hook `refresh_stale_after_ingest`, config `ai.observations`, eval observations + gate A1 | C8 step 4, D10 | Unico step C8 che spende token; cap 10 sintesi/pass. |
| 6 | **Chiusura** | docs (`ai_and_settings.md`), fixture discovery, `cargo test` workspace, `cargo clippy -D warnings`, `ibis eval retrieval`, aggiornamento WQ-5 | 1–5 | Gate di merge. |

**Regola di parallelismo:** C8 step 1–4 e E15 possono viaggiare in parallelo (file disgiunti: `memory/*` vs `agentic*`); E14 parte dopo E15 perché consuma il contratto. C8 step 5–6 chiude la parte LLM quando la fondazione deterministica è verde. Nessuna delle due specifiche introduce un nuovo tool MCP: il contratto resta a 58.
