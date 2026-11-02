# Analisi funzionale L2 — flussi per sottosistema (iterazione 1/2)

**Data:** 2026-11-01
**Predecessore:** [2026-11-01-dailydoseofds-improvement-analysis.md](./2026-11-01-dailydoseofds-improvement-analysis.md) (lista interventi A1–G20)
**Successore:** [2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md) (iterazione 2: specifiche funzionali per intervento)
**Collegamento:** [2026-11-02-master-work-queue.md](./2026-11-02-master-work-queue.md) — schedulazione di questo lavoro rispetto agli altri piani in sospeso (WQ-5)
**Metodo:** tre esplorazioni deep-dive (retrieval / memoria+DB+ingest / AI+costi+eval+planning) + verifica in prima persona delle costanti chiave citate (pesi, soglie, config). Tutti i riferimenti `file:line` sono dello stato corrente del repo.

Scopo: per ogni sottosistema, fotografare il flusso funzionale esistente (input → trasformazioni → output → error path) e mappare su di esso gli interventi A1–G20 del documento predecessore, segnalando dove il flusso attuale "incastra" l'intervento e dove presenta discrepanze non note.

---

## 1. Sottosistema retrieval (`crates/ibis-search`)

### 1.1 `ask_project` — pipeline ibrida HEAD

**Firma:** `ask_project(&self, actor: &ActorContext, query: &str) -> Result<Vec<SearchResult>>` (search.rs:248-252).

Flusso funzionale:

1. **Gate degradato** (search.rs:255-259): `require_db()` → `SearchError::DatabaseNotConnected`; senza AI → `SearchError::Embedding`. L'ordine gate→embed è una scelta di audit (H-A05): nessuna chiamata AI su richieste che devono fallire.
2. **Embedding query** `ai.embed_text(query)` con tracking costo su record `system:search_vector` (search.rs:260-267) via `DbCostStore` (search.rs:166-199, UPSERT `tokens_used += total` solo su `is_safe_record_id`).
3. **4 canali vector** (search.rs:284-329) — SurrealDB non supporta multi-FROM per query vettoriali, quindi una query per tabella, merged dopo. Score per canale = `ask_score_expr()` (search.rs:236-240, verificata):
   `(cosine(embedding, $vec) * 0.7) + (IF created_at IS NONE THEN 0.3 ELSE max([0, 1 - days(now-created_at)/365]) * 0.3 END)` — decadimento lineare 365 giorni. Budget `VECTOR_CHANNEL_LIMIT = 15` per canale (search.rs:32).
   - `file_chunk`: join implicito a `file.path`/`file.hash` come `current_hash`.
   - `commit_chunk`: score su `commit_date` (data autorialità) via `commit_chunk_ask_query()` (commit_meaning.rs:39-44).
   - `memory`: `WHERE embedding != NONE AND currentness AND visibility` (filtro attore via `resolve_trusted_actor`, search.rs:272-278; attore vuoto → solo `visibility='team'`, mai leak personale — temporal.rs:30-41).
   - `reasoning`: concat `question + outcome`.
4. **Risoluzione hash HEAD** per path (search.rs:331-344) via `lookup_current_file_hash_by_path` (lifecycle.rs:15-54, `is_head=true` con fallback `updated_at DESC`); chunk con `hash == head` → **bonus +0.05** (search.rs:345-357, verificato).
5. **Boost multi-signal** (search.rs:359-383): `tokenize_query` → `(keywords, entities)`; `resolve_linked_symbols` (lookup `symbol` tabella, ≥3 char, max 20 bind, best-effort — retrieval_boost.rs:188-226); ogni chunk → `ScoredHit`; `apply_multi_signal_boost` → `combine_retrieval_score(base, kw, ent) = base*0.75 + kw*0.15 + ent*0.10` (retrieval_boost.rs:176-183, verificata; keyword score = `0.6*coverage + 0.4*tf_saturata` con cap per-termine 5, retrieval_boost.rs:122-144). Sort desc, **truncate a 15**.
6. **Graph enrichment** top-3 file distinti (search.rs:386-394, 456-479): `graph::get_file_context` (graph.rs:56-76) — traversa `<-changed<-commit` + `->implements[usage_weight>0.5]->issue`, autori dedup, cache per-path `context_cache` (search.rs:417).
7. **Output:** `SearchResult { code_chunk_id, file_path, relevance_score, is_current, source_kind, commit_hash, status, kind, as_of_ref, content_source, context, content }` (search.rs:36-92). `is_current`: `hash == current_hash`; commit_chunk → sempre false; memory → true.

**Error path:** `DatabaseNotConnected` | `Embedding` | `wrap("embedding failed")` | `other("vector search failed on {t}")` | `other("failed to parse chunks…")`. Degraded è first-class: solo-git restano servibili `ask_project_at_ref` e il canale exact-path, flaggati `degraded: true` (tools_memory.rs:1230-1243).

### 1.2 `ask_project_at_ref` — ask storico (Layer 3)

`askref.rs:27-35`. Precondizione: `git_anchors` + `repo_path`; `ancestor_set_for` con process-local TTL cache `AncestorCache` (5 min, ibis-git/ancestry.rs:70; invalidata post-ingest). Canali: (1) memory git-bound filtrate per-riga `HOLDAt(introduced_in, invalidated_at, ancestors)`; (2) union tree claims `.ibis/claims/*.md` (unranked, score 0, dedup per record id); (3) Δ commit_chunk filtrati per raggiungibilità dall'ancestor set; (4) reasoning HEAD-contract; (5) code: exact-path se la query è un path in `tree(R)` (score 1.0, `content_source: "git_show"`, byte serviti da `cached_file_at_ref` hotcache), altrimenti semantico su embedding HEAD filtrato per `tree_set.contains(path)`. Cap finale `AT_REF_RESULT_CAP = 15` con **floor 3 slot** per claim_file (claim_tree.rs:26,31; cap_at_ref_results claim_tree.rs:331-372).

### 1.3 Loop agentic ReAct (`agentic.rs`)

Entry `ask_project_agentic(&self, query, branch_context) -> AgenticResult { answer, sources, steps }` (agentic.rs:58-62, 16-22). Il SYSTEM_PROMPT (agentic.rs:36-52) **non va nel ruolo system**: è concatenato al primo messaggio utente (righe 74-79). Protocollo ReAct con marker `SEARCH: "<query>"` e `FINAL ANSWER: <testo>`; `AGENTIC_MAX_STEPS = 5` (riga 54); `GenerationConfig { temperature: 0.2, max_output_tokens: 2000 }` (righe 89-93); costi su `system:search_agentic`. Determinazione azione = primo marker per posizione nel testo (righe 130-141). Query vuota → nudge `"OBSERVATION: Search query was empty…"`; nessun azione → nudge `"OBSERVATION: Please continue…"`. Ogni osservazione via `format_observation` (righe 264-324): label per source_kind, `Score: {:.2}`, content troncato a **2000 runes**. Le sorgenti sono dedup per id in `unique_sources`. **Freni esistenti: solo max-5-step.** Nessun budget token, nessuna no-progress detection, nessun completion check: il loop termina sul marker o sull'esaurimento step (terminazione "none" con default answer righe 252-256).

### 1.4 Session / insights / hotcache

- `load_session_context` (session.rs:40-110): working_state → rules (deterministico, LIMIT 20) → `recent_failures` (kinds `[fix,gotcha]`, since 30d, merge con `error_type` dal grafo log, truncate 5) → `top_memories` (search semantico con query fissa) → decisions → workstreams (LIMIT 5) → graph hints. Nota difetto noto: `fetch_limit = min(limit*3, 45)` senza `max(limit)` → clamp senza max panica con limit>45 (mod.rs:651-656, testato `search_memory_limit_above_fetch_cap_does_not_panic`).
- `memory_insights` (insights.rs:70): clustering 2 pass — bucket `kind|primo-tag` + split greedy per cosine embedding ≥ **0.82** (senza embeddings → singleton); label LLM solo con `label_with_ai` (hint `bulk_local`, ≤6 parole/60 runes). **Clustering greedy, non connected-components.**
- `hotcache` (memory/hotcache.rs): tabella schemaless `file_at_commit`, chiave `{oid}:{path}` vettabile, TTL 30d, read-through con invariante byte-exact (non-UTF-8 servito lossy ma mai cachato), kill-switch `hot_ref_cache_disabled`.

### 1.5 Dove incastrano gli interventi retrieval

| Intervento | Punto di aggancio funzionale | Gap nel flusso attuale |
|---|---|---|
| **A1 eval** | `ask_project`/`evaluate_retrieval` (raft.rs:109-130) | scoring reciproco esiste ma non è esposto come runner/CI; nessun dataset |
| **A2 RRF** | step 5 (merge dei 4 canali, search.rs:359-383) | fusione additiva su score eterogenei; pesi fissi in `combine_retrieval_score` |
| **A3 reranker** | tra step 5 e 7 (post-boost, pre-enrichment) | nessuno stage di scoring semantico |
| **A4 expansion** | tra step 2 e 3 (post-embedding, pre-vector) | solo tokenizzazione locale; il ReAct è l'unica seconda chance |
| **E14 loop brakes** | `ask_project_agentic` loop (agentic.rs:85-247) | solo max-5-step; cost tracking esiste ma non è un freno |
| **E15 trust** | struct `AgenticResult`/output MCP `tools_memory.rs:1183-1251` | nessun metadato di qualità/citanze strutturate |

---

## 2. Sottosistema memoria + DB (`ibis-db`, `memory/*`, `ibis-optimization`)

### 2.1 Schema (schema.rs:98-347, SCHEMAFULL SurrealDB 3.x)

- **`memory`** (182-219): kind enum 10 valori, status 4 (`current/superseded/archived/stale`), visibility 2, `confidence` default 0.7, `valid_from/valid_until`, `provenance{agent,session,tool,source_hash,source_path,introduced_in_commit,invalidated_at_commit,dirty,worktree_hash,promote_on_commit,orphan_oid}`, denormalizzati `introduced_in_oid`/`invalidated_at_oid`, `store` (`git|surreal`), `policy_applied`, `embedding` 768.
- **`file_chunk`** (119-129) / **`commit_chunk`** (130-137): `batch_status` ASSERT `pending|completed|failed`, `batch_id`; commit_chunk porta `commit_hash` + `commit_date` verbatim.
- **`symbol`** (260-262): solo `name` + `file` — tabella minima usata dall'entity-link (§1.1 step 5).
- **Ticketing = `issue`/`tracker`** (138-150) provider-agnostic; edge `implements` (commit→issue) con `confidence`/`usage_weight`.
- **HNSW** tutti `DIMENSION 768 DIST COSINE` su 5 tabelle (323-328).
- **Edge**: `supersedes` (memory→memory, 330), `derived_from` polymorfo (memory→source_file|file_chunk|commit|issue, 331-335), `part_of_workstream` (336), `changed` (commit→source_file con `impact`, 275-278), `has_memory` (repo→memory, 297), `calls`/`coupled_with`/`emits_log` (267-309).

### 2.2 Scrittura memoria e supersession (`memory/add.rs`)

`add_memory(&self, actor_ctx, params: &mut AddMemoryParams) -> Result<String>` (add.rs:85-87). Catena validazioni (righe 91-159): content → project → governance supersede (personal owner-only) → kind/status default → visibility (personal richiede trusted actor) → **store governance** `claim_store_decision` (claim_store.rs:53-70: `decision|rule|fix` + team → `git`; git-owned non può essere superseded da non-git) → confidence → git anchor (`resolve_memory_git_anchor` gitbound.rs:113-196: esplicito → ref derived_from → file-anchored con dirty/require_clean/worktree/promote_on_commit). **Claim store one-way**: `commit_claim_for_memory` (claim_store.rs:78-179) committa il file `.ibis/claims/<id>.md` PRIMA della riga DB, nello stesso flusso `add_memory`; `supersede_old_set` (gitbound.rs:227-242) marca il vecchio con `invalidated_at_oid = HEAD`. Transazione finale (add.rs:260-281): UPSERT repo → CREATE memory (id client-side `mem_<uuid>`) → RELATE `has_memory`/`derived_from` → RELATE `supersedes` + UPDATE old.

### 2.3 Lifecycle — trigger post-ingest (non cron)

Tutto ruota su `refresh_stale_after_ingest` (assembly.rs:315-361) nel `ProcessJobFn` della coda di ingestion (assembly.rs:252-312): ordine `invalidate_ancestors → index_claim_store (reconcile + withdrawal git-wins) → refresh_stale_memories_for_repo → refresh_orphaned_git_bounds → promote_dirty_anchors_for_repo → prune_file_at_commit_cache`; errori solo warn, il job non fallisce. Dettagli: `mark_stale_when_source_changed` confronta source_hash vs hash corrente (`status='stale'`); `promote_dirty_anchors` scandisce `file_history` con scan limit 50 (lifecycle.rs:319, 358-361) e matcha `worktree_hash`; `refresh_orphaned_git_bounds` (gitbound.rs:248-316) usa `verify_commits` batch e **flagga, non cancella**. **Solo `supersede_memory_for_repo` è transazionale** (lifecycle.rs:66-124); le refresh pass sono UPDATE row-per-row.

### 2.4 Consolidamento (`memory_consolidate.rs`)

Tool MCP `memory_consolidate` (parametri `project_name`, `max_items` default 20; registry.rs:356-362) → handler `tokio::spawn` **detached** (tools_memory.rs:894-927) → `consolidate_loop` (memory_consolidate.rs:102-192):

1. `select_observations` oldest-first (203-254).
2. `find_duplicate_memories` (256-332): top-3 cosine **senza soglia** (no cut-off — delegato al prompt), con governance personal/team lato SQL (`$dup_owner` bindato).
3. `distill_observation` (334-403): prompt temp 0.1 (testo completo a memory_consolidate.rs:344-348), hint `local_only` per personal / `bulk_local` per team; costi su `system:memory_consolidation`.
4. Validazione `create|merge|noise` (498-536); `merge_with` deve essere tra i candidati (396-401).
5. Persist: `add_memory` con `supersedes = merge_with|observation.id` → archiviazione osservazione in transazione (405-472).
6. Noise decay: `confidence = max(c*0.5, 0.1)`, `valid_until = now+7d` (474-493).
7. Error path: distill fallito → osservazione resta `current` (986-1008 test).

**Nessuno scheduler**: open follow-up esplicito del design 2026-10-09; il consolidate gira solo su invocazione MCP.

### 2.5 RAFT (`raft.rs`) — correzione importante

`generate_synthetic_qa` (51-105, temp 0.7, snippet cap 3000) + `evaluate_retrieval` (109-130, score `1/(rank+1)`, 0 se assente) + `optimize_loop` (134-220: campiona `file_chunk` random, genera QA, valuta, rinforza). **Correzione rispetto al documento predecessore: `usage_weight` NON è scritto dal RAFT loop** — il rinforzo tocca `access_count`/`last_accessed` su `file_chunk`; `usage_weight` è scritto solo da `ibis-ingest-git` (RELATE implements = 1.0) e dal reinforce di `save_reasoning_outcome` (`+0.2` su changed/implements, add.rs:459-468), ed è letto dal graph enrichment (graph.rs:69-71).

### 2.6 Ingest code (`ibis-ingest-code/src/ingest.rs`)

Git tree SoT (`git ls-tree -r HEAD`, ingest.rs:160-165) → worker pool semaphore default 5 (151-154) → per file: read-once + sha256 → record id deterministico `source_file:<repo>_<rel>_<hash>` → hash-skip → refresh marker `is_head` (512-541) → dedup chunk per `(file, hash)` (545-567) → chunking ast-grep (`ast_chunker::parse_ast`) con fallback line-chunking 1000 char (867-885) → persist batch 100 con `batch_status='pending'` e **embedding non nel SET** (sopravvive a partial resume, F-A03) → symbol UPSERT + `contains`/`calls`/`emits_log` edges. Prune in batch da 50 (366-447). Embedding reale delegato al batch manager.

### 2.7 Dove incastrano gli interventi memoria

| Intervento | Aggancio | Gap |
|---|---|---|
| **C7 schema tipizzato** | `add_memory` validazioni + prompt `distill_observation` | campi liberi; nessun vincolo per-kind; edge senza source/target constraints |
| **C8 Observations** | `consolidate_loop` (dopo step 1) + `memory_insights` | clustering greedy per cosine 0.82 (tematico, non a firme); nessun cluster cross-entità con evidenza |
| **C9 entity resolution** | `find_duplicate_memories` (256-332) | nessuna soglia, nessuna normalizzazione deterministica pre-match |
| **B6 embedding cache** | `OllamaProvider.cache` (ollama.rs:42, 160-172) | in-process, cap 1024, clear wholesale (non LRU), non persistita |
| **A1 eval** | `evaluate_retrieval` (raft.rs:109) | scoring reciproco non esposto come runner/dataset |

---

## 3. Sottosistema AI / costi / routing (`ibis-ai`)

### 3.1 Router ibrido (`router.rs`)

`ReasoningRouter { local, cloud, cfg }` (45-49) — albero decisionale `generate_content` (123-164, verificato): hint default `auto`; `prefer_cloud = hint=="quality" || (auto && use_cloud_when_no_gpu && cpu_only)`; `local_only` → **mai** fallback cloud (test :360-378); `bulk`/`bulk_local` → local→cloud; timeout locale `local_timeout_ms` default 120000 (config.rs:165-167); `context_only_fallback` → errore sentinella `AiError::ContextOnly` invece del Joined. Un solo punto di costruzione: `state.rs:524-535`.

### 3.2 Config tier (config.rs, verificato)

`DEFAULT_MODEL_S/M/L/XL = granite4.1:3b / qwen2.5-coder:7b / gemma4:12b / muse-glimmer` (16-19); `model_overrides: HashMap<String,String>` su `ReasoningConfig` (55); normalize riempie i default per S/M/L/XL (169-184). **Il tier non è mai selezionato per call-site**: `model_overrides` è usato per l'auto-tier del modello locale in `state.rs`, non per instradare task per complessità. Gli hint in produzione sono solo `bulk_local` (insights, consolidation), `local_only` (consolidation personal) — tutti gli altri call-site usano `auto` implicito.

### 3.3 Inventario completo call-site LLM (18, verificati)

Embedding (7): search.rs:261, memory/mod.rs:607, add.rs:306, add.rs:394, asof.rs:129, askref.rs:62, rules.rs:43; + analyzer.rs:911 (error_type) + batch_manager.rs:425 (`batch_embed_text`). Generation (10): **agentic.rs:87** (temp 0.2, 2000 tok, auto); **insights.rs:216** (bulk_local); **raft.rs:71** (0.7); **memory_consolidate.rs:364** (0.1, local_only/bulk_local); **analyzer.rs:638** (0.1, JSON array SemanticError); **analyzer.rs:802** (0.0, regex RE2); **enrich.rs:852** (0.1, testo ≤40 parole); **ast_rule_gen.rs:234** (0.1, JSON regola); **rule_synth.rs:350** (0.1, JSON, cooldown fingerprint). Mappa per-tier proposta nel documento L3.

### 3.4 Costi e quote — due contatori distinti

- **Costo** `CostTracker` (cost.rs): `estimate_tokens = len/3+1` (byte), blended rate fisso **$0.15/1M**, record `system:*` o `log_file:*` con UPSERT `tokens_used += total`; store failure loggato, mai fatale. Nessun pricing per-modello.
- **Quota** `key_usage:<hash8>_<yyyymmdd>` (types.rs:119-128): solo Gemini, RPM pacing (`60s/(rpm*0.9)`) + TPM token bucket + RPD hard stop con pausa 10 min; persistito via `DbUsageStore` (state.rs:591-640) con flush 5s.
- **Cache embedding**: `EMBED_CACHE_CAP = 1024`, clear wholesale, semaphore concorrenza 1 (`IBIS_OLLAMA_EMBED_CONCURRENCY`), keep_alive 30m (ollama.rs:12-60). Chat Ollama: `stream: false`, temp/max_predict inseriti solo se >0, timeout 180s (ollama_chat.rs:88-103).

### 3.5 Batch manager (`batch_manager.rs`)

Sweep ogni **10s** (`MissedTickBehavior::Skip`), `BATCH_SIZE 100`; chunk: `SELECT … WHERE batch_status='pending'` su file_chunk poi commit_chunk → embed batch → una sola transazione `UPDATE … SET embedding, batch_status='completed'`; commit: risolve git dir (`ChainedGitDirResolver`), patch `git diff^` con fallback `git show`, chunking line-greedy `max_delta 8000`, transazione unica con upsert chunk + `RELATE has_commit_chunk` + flip status/failed con `batch_error`. Errori → riga lasciata pending o marcata failed, mai crash del loop (16 test).

### 3.6 Dove incastrano gli interventi costi

| Intervento | Aggancio | Gap |
|---|---|---|
| **D10 routing per-task** | `ReasoningRouter` + `model_overrides` | tier non selezionato per call-site; nessun mapping task→tier; nessuna affinity |
| **D11 bounded decisions** | call-site consolidation/analyzer (`parse_consolidation_decision`, parse JSON) | decisioni chiuse fatte con generazione + parsing; nessuna distribuzione/confidence strutturata |
| **D12 cache risposte** | nessuno (assente ovunque) | solo embedding cache; nessun layer di response cache |
| **D13 prompt stability** | prompt builder dei call-site cloud (rule_synth, analyzer, consolidate) | nessuna convenzione di ordine; nessun tracking `cache_read` nel CostTracker |

---

## 4. Valutazioni e pianificazione (stato)

### 4.1 Track D (`evaluations/scope/`) — cosa misura e cosa no

Benchmark agentico "Ibis DevAX con Microsoft Scope": 3 profili (`base-no-ibis`, `ibis-mcp`, `ibis-assistant-full`) × worker (`vscode-copilot`, `claude-code`) × 4 task fixture × `REPEATS=3`. criteria.json: 8 criteri (session contract, memory hygiene, grounded answers, friction, build, tests, task completed). **Nota (corretta il 02/11, non una discrepanza):** il disallineamento README ("pass@k / success@T") vs `analysis.mjs` (pass-rate aggregato + media token/durata) e i placeholder `"TODO-verify-worker-type"` in profiles.json sono **stato atteso**: il Track D non ha ancora raggiunto D2 (runner+analysis) e D1 (pin/seed su istanza Scope live) — vedi [master work queue WQ-4](./2026-11-02-master-work-queue.md). **Non copre** la qualità del retrieval (l'unica eccezione è `evaluate_retrieval` RAFT) né i costi runtime di Ibis.

### 4.2 Stato piano (PLAN.md, specs)

Completati: F0.1–F0.7, gate R2 (23/23 smoke + evidenze VS Code), runtime **0.2.0**. In corso: `[F1 ∥ F2]` (VSIX F1.1–F1.6, agent plugin F2.1–F2.6); gate R3/R4 Parte A 26/26 PASS, Parte B runbook owner da eseguire. Poi: F3 docs → **D1–D5 Track D** → R6 publish. Spec Layer 3 (2026-10-24): fasi 3a–3e, 4a–4d, 5a–5b, 3f implementate; restano aperti branch-qualified refs, ancestry full-DAG, backfill derived_from. Spec hybrid reasoning (2026-09-19): implemented; open follow-ups: `granite4.1:8b` come default M, webview laterale, vLLM.

### 4.3 Discrepanze rilevate in questa iterazione (da registry/consolidate/eval)

1. **Registry 56 vs 57**: header registry.rs:1 e il README dicono "56-tool", `TOOL_SPECS` ne contiene 57 (contract test pinna 57, tool_contract_test.rs:13-101); famiglia Memory 27, non 26.
2. **Nessuna soglia** nel duplicate lookup del consolidamento (top-3 cosine senza cut-off).
3. **`usage_weight` non scritto da RAFT** (solo `access_count`), come corretto in §2.5.
4. **`workstream`** tabella senza campi e senza writer (schema.rs:265) — parte del design 2026-10-09 non portata; `part_of_workstream` sostanzialmente inattiva.
5. **Consolidamento senza scheduler** (solo MCP-triggered detached).
6. **Riferimenti doc rotti**: handoff `2026-10-24-layer3-implementation-state.md` e plan `2026-10-09-memory-absorption.md` citati dagli spec ma assenti dal repo.
7. **Lifecycle non transazionale** (UPDATE row-per-row; solo supersede è transactional) — rilevante per C8 (rigenerazione observazioni).
8. ~~**Track D**: README vs script (pass@k/pass-rate), placeholder TODO.~~ — **ritrattata il 02/11**: stato atteso pre-D1/D2 (piano in corso), non una discrepanza. Vedi [master work queue WQ-4](./2026-11-02-master-work-queue.md).

---

## 5. Mappa d'insieme interventi → flussi (sintesi L2)

| Intervento | Sottosistema | Sezione flusso | Note di aggancio |
|---|---|---|---|
| A1 eval | retrieval | §1.1/§2.5 | riusare `evaluate_retrieval` come kernel; nuovo dataset+runner |
| A2 RRF | retrieval | §1.1 step 5 | sostituire `combine_retrieval_score` con RRF + boost sopra rank |
| A3 reranker | retrieval | §1.1 tra step 5 e 7 | nuovo stage opzionale, hint `bulk_local` |
| A4 expansion | retrieval | §1.1 step 2→3 | espansione deterministica (symbol table già usata) |
| B5 quantizzazione | DB/ingest | §2.1 HNSW, §2.6 | migrazione schema; vincolo SurrealDB (no Hamming) |
| B6 embedding cache | ai | §3.4 | sostituire cache wholesale con store persistito |
| C7 schema memoria | db/memory | §2.1/§2.2/§2.4 | validazioni per-kind in `add_memory` + prompt distill |
| C8 Observations | optimization | §2.4, §1.4 | clustering a firme sostituisce/affianca greedy cosine |
| C9 entity resolution | optimization | §2.4 step 2 | normalizzazione deterministica + soglia |
| D10 routing | ai | §3.1/§3.2 | mapping task→tier + affinity per ask |
| D11 bounded decisions | ai | §3.3 call-site | primitiva decide() con distribution+confidence |
| D12 response cache | ai | §3.4 | layers exact/semantic; mai su ask |
| D13 prompt stability | vari | §3.3 | convenzione ordine + `cache_read` nel CostTracker |
| E14 loop brakes | search | §1.3 | budget + no-progress + completion check nel ReAct |
| E15 trust score | search/server | §1.1 output | citanze strutturate in `AgenticResult`/MCP payload |
| E16 durable tasks | server | §2.4, §3.5 | estendere pattern batch_manager |
| F17 OTel | logger/server | — | feature-gated, default off |
| G19 CLAUDE.md projection | cli/memory | §2.2 read paths | marker di rigenerazione |

## 6. Conclusione iterazione 1

I flussi esistenti coprono già: gate degradato, governance memoria, Layer 3 git-bound, budget per canale, quote cloud. I gap funzionali che gli interventi devono colmare sono concentrici in cinque punti: (1) la fusione/rerank dello step 5 di `ask_project`; (2) l'assenza di freni oltre max-5-step nel ReAct; (3) la distillation memoria senza schema né soglie; (4) il routing per-task mai materializzato sui tier; (5) l'assenza totale di eval retrieval e response caching. L'iterazione 2 (documento successore) scende a specifica funzionale per intervento su queste cinque zone.