# Analisi funzionale L3 — punti di aggancio e specifiche per intervento (iterazione 2/2)

**Data:** 2026-11-01
**Predecessori:**
1. [2026-11-01-dailydoseofds-improvement-analysis.md](./2026-11-01-dailydoseofds-improvement-analysis.md) — lista interventi A1–G20 (fonte articoli)
2. [2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md](./2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md) — iterazione 1, flussi per sottosistema

**Perimetro:** interventi delle fasi 0–3 dell'ordine d'attacco (A1, A2, A4, A3, C9, D13, B6, C7, D10, D11, D12). Le fasi 4–5 (C8, E14, E15, E16, B5, F17, G19, G20) restano al livello L2 e verranno specificate L3 in un documento successivo, dopo l'esecuzione delle fasi 0–3 (molti dei loro punti di aggancio dipendono da eval e routing).

**Metodo:** ogni specifica è ancorata a codice letto in prima persona (search.rs, retrieval_boost.rs, config.rs, router.rs, memory_consolidate.rs) o a report di esplorazione verificati con line-references. Formato per intervento: **Aggancio → Flusso attuale → Trasformazione (prima→dopo) → Casi limite → Test → Accettazione**.

---

## A1 — Eval harness retrieval (fase 0)

**Aggancio:** `evaluate_retrieval` (crates/ibis-optimization/src/raft.rs:109-130) e `ask_project` (search.rs:248). L'infrastruttura di scoring esiste: score reciproco `1.0/(rank+1)` per rango del chunk atteso nella lista `SearchResult`, 0 se assente.

**Flusso attuale:** `optimize_loop` (raft.rs:134-220) campiona chunk random → genera QA sintetica (LLM, temp 0.7) → valuta con `evaluate_retrieval` → rinforza `access_count`. Nessun dataset statico, nessun runner esportato, nessuna metrica aggregata, nessuna CI.

**Trasformazione (prima→dopo):**
- Nuovo tipo `EvalCase { id, query, expected_ids: Vec<String> }` in dataset YAML (`evaluations/retrieval/dataset.yaml`), seed iniziale dalle fixture `evaluations/scope/task-prompts/fixture-tasks.md` e dai test `live_memory_estate.rs` (i casi at_ref/HOLDAt sono già scenari verificabili).
- Nuovo runner `RetrievalEvalRunner` in `ibis-optimization` che riusa `evaluate_retrieval` estendendola: per case calcola `mrr`, `recall@5`, `recall@15` (match per `code_chunk_id` OR id memoria), per-channel breakdown (`source_kind` del rank più alto).
- Nuovo subcommand CLI `ibis eval retrieval --dataset <path> [--min-recall-5 X]` (ibis-cli) → report JSON + exit code non-zero sotto soglia.
- Job CI separato (workflow esistenti su windows-latest/ubuntu-24.04) che avvia SurrealDB/Ollama sidecar come già fa `cargo test` e gira il runner sul dataset.

**Casi limite:** degraded mode → case marcato `error`, non `fail` (distinguere ambienti senza AI da fallimenti di ranking); modelli embedding diversi → tolleranza via recall@k, mai rank esatto; expected id di tipo `memory` matchato anche sul canale memory.

**Test:** unit runner su FakeDb/FakeAI (pattern test_support.rs); 1 test di validazione dataset (schema, id unici). `evaluate_retrieval` resta invariato come kernel.

**Accettazione:** dataset con ≥10 case; `ibis eval retrieval` produce report con MRR/recall; CI verde; baseline documentata nel report (numeri pre-A2/A3/A4).

---

## A2 — RRF al posto della fusione additiva (fase 1)

**Aggancio:** i 4 canali separati (search.rs:309-329), il merge con boost (search.rs:359-383), `combine_retrieval_score` (retrieval_boost.rs:176-183, verificata: `base*0.75 + kw*0.15 + ent*0.10`, floor 0.5 su remain).

**Flusso attuale:** ogni canale produce `ChunkResult` con score locale `cosine*0.7 + decay*0.3` su scale non comparabili; merged in un solo vettore; il boost additivo scala il base di canale per 0.75 — quindi il ranking finale è dominato dalla scala del canale sorgente.

**Trasformazione:**
1. Mantenere la raccolta per-canale (già separata) e annotare per ogni chunk `(channel, rank_in_channel)`.
2. Nuova funzione `rrf_fuse(channels: &[Vec<(usize, usize)>], k: f64) -> HashMap<usize, f64>` (k=60) in `retrieval_boost.rs`: per ogni canale, `score += 1/(k + rank)`; chunk presente in più canali **somm**a i contributi (standard RRF).
3. Base finale = RRF score; il boost keyword/entity resta additivo sopra il base ma riscalibrato: `combine_retrieval_score` diventa `rrf_norm*0.75 + kw*0.15 + ent*0.10` con `rrf_norm = rrf / rrf_max_teorico` (max teorico = somma dei canali × 1/(k+1)).
4. Il bonus "is current" (+0.05, search.rs:353) si applica **dopo** la fusione come epsilon costante (non più frazione di score cosine).
5. Config: `search.fusion = "additive" | "rrf"` (default `rrf` solo dopo validazione A1; default iniziale `additive` per rollout sicuro), nuovo campo su `SearchService`/world_state.

**Casi limite:** canale vuoto (skip); chunk senza embedding scartati prima; tie → sort stabile desc + tie-break deterministico per id; dedup per id già garantito da `unique_sources` lato agentic ma non lato `ask_project` (stesso chunk in 1 solo canale per costruzione — file_chunk non può apparire in memory).

**Test:** `combine_keeps_base_share` e `boost_sorts_descending` (retrieval_boost.rs:292, 301) pinnano il path additive → restano per la modalità legacy; nuovi unit per RRF (contributi sommati cross-channel, sensibilità k); **verificare `ask_project_merges_channels_and_enriches_top3` (search.rs:601)**: asserisce valori di score/rank → aggiornare o rendere indipendente dalla modalità di fusione.

**Accettazione:** A1 mostra `recall@5`/MRR ≥ baseline additive su dataset golden; fallback config funzionale; nessuna regressione sui 4 canali unit test.

---

## A4 — Query expansion deterministica (fase 2, parte 1)

**Aggancio:** embedding della query (search.rs:260-263) e tokenizzazione (search.rs:359-361, retrieval_boost.rs:64-100). La `symbol` table è già interrogata in `resolve_linked_symbols` (retrieval_boost.rs:188-226, max 20 bind, best-effort).

**Flusso attuale:** un solo embedding della query letterale; le entità collegate entrano solo nello score boost (entity-link), mai nel matching vettoriale. Se la query usa un nome diverso dal simbolo ("price calculator" vs `PriceCalculatorService`), il canale vector non lo vede.

**Trasformazione:**
1. Nuova funzione `expand_query(query, linked_symbols) -> Vec<String>` in `retrieval_boost.rs`: ritorna `[originale]` o `[originale, variante]` dove variante = query con i token CamelCase riuniti + i nomi dei simboli linked appesi (dedup).
2. Se la variante ≠ originale: secondo `embed_text` (costo +1 embedding per ask, registrato su `system:search_vector`); le query per canale girano due volte con `$vec` diverso; per riga vale `score = max(score_orig, score_var)`.
3. Config: `search.query_expansion = "off" | "symbol_variant"` (default `symbol_variant`, costo zero quando linked_symbols è vuoto).
4. L'espansione LLM/rewrite è **esclusa da questa specifica** (latenza sul percorso critico): eventuale fase successiva con hint `bulk_local` e cache.

**Casi limite:** `linked_symbols` vuoto → nessun secondo embedding (fast path invariato); variante == originale dopo lowercase → skip; fallback `resolve_linked_symbols` fallito → fast path; as_ref/at_ref (`askref.rs:62`) eredita l'espansione solo per il canale semantico, non per exact-path.

**Test:** unit `expand_query` (camelCase, path-like, dedup); test embedding doppio con FakeAI che registra le chiamate; regressione sui canali.

**Accettazione:** A1 recall@5 in crescita sui case con mismatch simbolico; nessun impatto su case senza simboli.

---

## A3 — Reranker post-fusione (fase 2, parte 2)

**Aggancio:** tra il boost (search.rs:359-383) e l'enrichment top-3 (search.rs:386-394). Provider già componibile: il router accetta hint `bulk_local` (router.rs:140-141 → local→cloud).

**Flusso attuale:** dopo il boost, i top-15 determinano output e enrichment; nessuno stage semantico.

**Trasformazione:**
1. Nuovo modulo `crates/ibis-search/src/rerank.rs`: `rerank_hits(&mut Vec<ScoredHit>, query, ai, config) -> ()` — prende i top-N (N=30) dopo il boost, costruisce un singolo prompt di scoring (query + lista numerata di estratti troncati a ~300 rune), output atteso JSON array di `{index, score: 0..1}`; temp 0.0, hint `bulk_local`, max_output_tokens ~512.
2. Riordino: `final_rank = rrf_norm*0.4 + llm_score*0.6` (blend, non sostituzione — evita il collapse verso matching lessicale del modello); hit senza score (parse failure parziale) mantengono il rango pre-rerank.
3. Fail-open: qualunque errore AI (incluso `ContextOnly`) → ordine invariato, warn log. Costo su nuovo record `system:search_rerank`.
4. Config: `search.rerank = "off" | "local"` (default `off`; attivazione guidata dai numeri A1).

**Casi limite:** N < soglia (es. ≤5 hit) → skip; provider non funzionale → skip; JSON troncato → parse per-riga tollerante (righe valide accettate); doppi indici → primo wins; as_ref path (`askref.rs`) **non** reranka nella v1 (cap 15 con floor claim_file — riordinare potrebbe violare il floor; da estendere solo con una variante floor-aware).

**Test:** unit con mock AI (score validi/invalidi/troncati); test fail-open; integration: ordine cambia con mock, invariato su errore.

**Accettazione:** su A1, precision@5/MRR migliora con rerank on; latenza P50 ask +≤2s con modello tier M locale; nessun failure propagato.

---

## C9 — Entity resolution + soglia nel duplicate lookup (fase 1)

**Aggancio:** `find_duplicate_memories` (memory_consolidate.rs:256-332, verificato: top-3 cosine **senza soglia**, governance personal/team via `$dup_owner`, righe con id unsafe scartate).

**Flusso attuale:** top-3 candidati indipendentemente dalla similarità → il prompt LLM decide merge/create/noise → rischio: candidati irrilevanti spingono il modello a merge errati e il costo di un turno LLM è sprecato quando non c'è nulla di simile.

**Trasformazione:**
1. Soglia: `DUP_SIMILARITY_MIN: f64 = 0.80` applicata post-query su `DuplicateMemory.score` (righe con `score < soglia` scartate); config `optimizer.dup_similarity_min`.
2. Pre-match deterministico pre-LLM: normalizza titolo osservazione e candidati (lowercase, trim, separatori path → spazio, collapse spazi); se candidato e osservazione hanno normalizzato titolo identico → azione `merge` diretta verso quel candidato, **senza chiamata LLM** (stesso flusso persist di `persist_consolidation`).
3. Con 0 candidati post-soglia: la chiamata LLM resta ma con `POSSIBLE DUPLICATES: []` (decisione ridotta a create/noise).
4. L'embedding dell'osservazione per il lookup è già ricalcolato dal chiamante (`obs_embedding`, riga 303) — nessun cambio.

**Casi limite:** candidati con `embedding = NONE` → `score = 0.0` → scartati dalla soglia (comportamento corretto); osservazione personal senza owner → già nessun candidato (269-285) — la soglia non cambia la governance; tie multiple identical-title → primo per score.

**Test:** estendere i test esistenti `consolidate_loop_merges_with_current_duplicate` (:821), `consolidate_loop_skips_foreign_personal_duplicates…` (:906); nuovi: `find_duplicate_memories_applies_threshold`, `deterministic_title_match_skips_llm` (asserisce zero chiamate AI).

**Accettazione:** riduzione misurabile di turni LLM per consolidate (tracciata su `system:memory_consolidation`); nessun merge cross-governance introdotto.

---

## D13 — Prompt stability per cloud caching (fase 1)

**Aggancio:** builder prompt dei call-site cloud: `distill_observation` (memory_consolidate.rs:344-348, verificato: istruzioni stabili prime, variabili dopo — già conforme), `analyzer.rs:638/802`, `rule_synth.rs:350`, `ast_rule_gen.rs:234`. Tracking costi: `CostTracker` (cost.rs:28-53) e `UsageMetadata` (solo `prompt_token_count`/`candidates_token_count` oggi).

**Flusso attuale:** nessuna convenzione certificata sull'ordine; nessun contatore di token cachati → il salvataggio non è misurabile.

**Trasformazione:**
1. Convenzione scritta (doc `docs/ai_and_settings.md` + commento nei call-site): **blocco istruzioni byte-stabile per call-site → contesto variabile → query ultima**; nessun timestamp/request-id nel prefisso (verificato per consolidate; audit per gli altri 3).
2. `UsageMetadata` esteso con `cached_content_token_count: Option<u64>` (Gemini lo espone); provider OpenAI-compat mappa `prompt_tokens_details.cached_tokens`; Claude mappa `cache_read_input_tokens`.
3. `CostTracker.record_usage` esteso con param `cached_tokens` → nel record `tokens_used` il costo prompt viene scalato (approssimazione: prompt_billing = prompt − 0.9×cached) e log debug del risparmio. Blended rate resta $0.15/1M (nessun pricing per-modello in questa fase).
4. Nessun `cache_control` Anthropic in v1 (richiede part-level metadata su `Content`): v1 copre Gemini/OpenAI automatic + stabilità di prefisso.

**Casi limite:** prompt con snippet di codice variabili nel prefisso (rule_synth: fingerprint estensione nel prefisso → spostarlo nel blocco variabile); provider che non espone cached tokens → campo assente, costo invariato.

**Test:** snapshot test per i 4 call-site: due invocazioni con contesti diversi → prefisso byte-identico (nuovi unit in ciascun crate); cost test esteso con cached_tokens.

**Accettazione:** su traffico ripetuto (test di integrazione con mock ripetuto), `cached_content_token_count > 0` registrato; prefissi stabili per costruzione.

---

## B6 — Cache embedding persistente (fase 1)

**Aggancio:** `OllamaProvider.cache` (ollama.rs:42 cap 1024, clear wholesale 160-172, verificato); consumatori: 7 call-site `embed_text` + `batch_embed_text` (batch_manager.rs:425). Nota: l'ingest evita già il re-embedding per `(file, hash)` (ingest.rs:545-567) — il gap reale è (a) re-index su DB perso/rebuild, (b) chunk identici cross-file (codice generato, boilerplate), (c) la cache in-process sprecata tra processi.

**Flusso attuale:** cache HashMap in-process keyed sul testo; al cap → clear totale; nessuna persistenza.

**Trasformazione:**
1. Nuova tabella `embedding_cache` (schema.rs): `key` (string, UNIQUE) = `{model}:{dim}:{sha256(text)}`, `model`, `dim`, `embedding array<float>`, `created_at`. Nessun HNSW (nessun bisogno di ANN: lookup per chiave esatta).
2. Seam provider-level: `EmbeddingCacheStore { async fn get(&key) -> Option<Vec<f32>>; async fn put(&key, vec) }` — `OllamaProvider` accetta `Option<Arc<dyn EmbeddingCacheStore>>` (pattern `CostStore`); lookup PRIMA del batch (chiavi calcolate per tutti i miss in coda), write-back dopo la risposta.
3. Config: `ai.embedding.disk_cache = true|false` (default **false** in v1, opt-in; quando true con `DbUsageStore`-like adapter su SurrealDB).
4. Il clear wholesale della cache in-process resta come L1; la cache DB è L2.

**Casi limite:** cambio modello/dim → chiave inclusa, nessuna stale; DB assente/degraded → seam None → comportamento attuale; concorrenza (semaphore 1) → la read-through non accoda; cap disco → prune periodico su `created_at` (pattern `prune_file_at_commit_cache`, hotcache.rs:129-148) con config TTL.

**Test:** estendere i test cache esistenti (`repeated_queries_hit_the_cache`, `cache_clears_never_lose_a_requested_embedding` — ollama.rs:271-482) con store seam; test prune; test chiave modello.

**Accettazione:** re-ingest su DB vuoto con cache calda non invoca Ollama per i testi già noti (verificato con fake store che conta miss); default off non cambia i test esistenti.

---

## C7 — Schema tipizzato per l'estrazione memoria (fase 2, parte 3)

**Aggancio:** schema `memory` (schema.rs:182-219, campi liberi `title/content`), validazioni `add_memory` (add.rs:91-159), prompt+parser consolidamento (memory_consolidate.rs:344-403, 498-536).

**Flusso attuale:** kind enum rigido ma il contenuto è free-text: nessun attributo filtrabile per kind (es. `decision.alternatives`, `fix.root_cause`); il prompt di distill emette solo i 6 campi fissi del JSON.

**Trasformazione:**
1. Schema: `DEFINE FIELD OVERWRITE details FLEXIBLE option<object> ON memory` (FLEXIBLE = nested free-form, il resto della tabella resta SCHEMAFULL). Nessuna migrazione dei row esistenti (campo opzionale).
2. Matrice per-kind (rubric, commento in add.rs + doc): `decision` → `{scope, alternatives[], tradeoffs}`, `fix` → `{symptom, root_cause, remedy}`, `constraint` → `{applies_to[], until}`, `rule` → `{enforcement}`; campi richiesti da una sola chiave minima per kind (es. decision: `scope`), gli altri opzionali — evita lo schema troppo rigido (contro del pattern Graphiti 10/10/10).
3. `add_memory`: nuova validazione `validate_memory_details(kind, details)` con policy `memory.schema_enforcement = "warn" | "strict"` (config; default `warn` in v0.x — pre-1.0 ammette breaking ma il rollout è a due stadi).
4. Prompt consolidamento: aggiunge al JSON atteso la chiave `"details": {...}` con le istruzioni per-kind; `parse_consolidation_decision` valida le chiavi obbligatorie; mancanza → `details` parziale + warn (policy warn) o errore→retry 1 volta (policy strict).
5. Governance edge: in `add_memory` il `supersedes` verso target `store='git'` richiede già stesso dominio (144-152); aggiunto check **stesso kind** per `supersedes` (decision→decision), coerente con C8 (le firme usano il kind).

**Casi limite:** `details` assente nei row legacy → filtro retrieval per attributo degradato (documentato); MCP `memory_add` nuovo param opzionale `details` (retrocompatibile); `memory_update` patch `details` con stessa validazione.

**Test:** matrice validazione per-kind (unit); estendere `parse_decision_matrix` (:1013) e `consolidate_loop_creates_typed_memory_using_bulk_local` (:755) con details; contract test registry (param nuovo, resta 57 tool).

**Accettazione:** `memory_search` con filtro `details.scope` (SQL additivo) funziona sui nuovi record; policy warn non rompe nessun test esistente.

---

## D10 — Model routing per-task (fase 3, parte 1)

**Aggancio:** `ReasoningRouter` (router.rs:123-164, verificato: hint default `auto`; `local_only` mai cloud; `bulk/bulk_local` local→cloud; timeout 120s), `model_overrides` S/M/L/XL (config.rs:16-19, 169-184, verificati), costruzione unica in `state.rs:524-535`, inventario call-site (L2 §3.3, 18 entry).

**Flusso attuale:** il tier S/M/L/XL esiste solo come override del modello locale singolo in `state.rs` (auto-tier all'avvio); per call-site vale solo l'hint binario local/cloud: insights (S-like) e agentic ask (L-like) ricevono lo stesso modello.

**Trasformazione:**
1. Estensione hint: `route_hint = "local:S" | "local:M" | "local:L" | "local:XL"` (parse in router.rs:128-131; mantienenendo gli hint esistenti).
2. Router esteso: `locals: HashMap<String, Box<dyn AIProvider>>` al posto del singolo `local` (costruito in `state.rs` con `OllamaChatProvider` per ciascun tier da `model_overrides`, costruiti lazy per non scaricare 4 modelli); `Step::Local` → `Step::Tier(t)` che seleziona la mappa con fallback al provider default.
3. Mapping per call-site (costante per-crate, commentata): insights → `local:S`; enrich, raft QA → `local:S`; consolidation, analyzer, rule_synth, ast_rule_gen → `local:M`; agentic ask → `local:L`; cloud hints invariati (`quality`, `bulk_local`…).
4. Affinity: naturale in questa forma — ogni call-site ha un provider fisso per la durata del loop ReAct (un solo provider per ask), quindi nessuno switch di modello intra-task (vincolo del pattern Plano soddisfatto senza session-id).
5. Config: `model_overrides` già esistente; nessuna nuova chiave (nuovi hint ignorati dai provider non-router → backward compat).

**Casi limite:** tier non presente nella mappa (Ollama non ha ancora scaricato il modello) → `try_step` → `LocalUnavailable` → fallback cloud secondo hint base (bulk_local) — stesso contratto attuale; CPU-only → `use_cloud_when_no_gpu` si applica per-tier; `local_only` (consolidation personal) su tier M → mai cloud, invariato.

**Test:** estendere i 9 test router esistenti (router.rs:270-449) con casi tier; test di mapping (ogni call-site dichiara il tier atteso — contract per grep); wiring test state.

**Accettazione:** log dei modelli per call-site mostra la mappa attesa; costo cloud per consolidation/insights scende (tracciato su `system:*`) senza variazione di qualità su A1.

---

## D11 — Bounded decisions (fase 3, parte 2)

**Aggancio:** parse/decisioni oggi generate: `parse_consolidation_decision` (memory_consolidate.rs:498-536), parser JSON analyzer (analyzer.rs:638-679), guardrail rule_synth (rule_synth.rs:54-57). Provider: tier S disponibile dopo D10.

**Flusso attuale:** decisioni chiuse ("è noise?", "è già noto?") passano da generazione JSON libera + parse + validazione — parsing brittle, nessuna misura di confidence, retry opachi.

**Trasformazione (v1: primitiva + shadow mode):**
1. Nuovo modulo `crates/ibis-ai/src/decide.rs`:
   - `DecideQuestion { id, kind: Noul | Choice(Vec<(String, String /*descrizione*/)>), instructions }`
   - `decide(state: &str, questions: &[DecideQuestion], ai: &dyn AIProvider) -> DecideResult` — prompt single-turn con schema di risposta vincolato `{"<id>": {"probs": {"<opzione>": p}}}`; hint `local:S`, temp 0.0; confidence = `max_prob − second_prob` (margine, misura di astensione).
   - Policy di astensione **in codice del chiamante** (mai dentro decide): `confidence ≥ θ_auto` → auto; `θ_auto > c ≥ θ_escalate` → path LLM corrente; `c < θ_escalate` → default conservativo (create/noise sicuro).
2. Shadow mode: config `ai.decide.shadow = true|false` (default true). In shadow, il consolidation **calcola** anche `decide()` (stato = osservazione+candidati, question "is_noise") e logga `decide_shadow { decision_llm, probs, confidence, agreement }` accanto alla decisione corrente — nessun cambio di comportamento.
3. Attivazione (fase successiva, guidata dai log shadow): route "is_noise" del consolidation via decide() con soglie derivate dalla calibration; gli altri call-site (analyzer dedup) seguono lo stesso schema in fase 2 di rollout.

**Casi limite:** JSON invalido o probs non normalizzate → abstain (escalate al path corrente); modello S assente → skip shadow (log una volta); calibrazione richiede dataset (`evaluations/decisions/cases.yaml` con esempi labeled, anche 30-50 righe manuali iniziali).

**Test:** parser decide (schema valido/invalido/margin); shadow-mode: decisione corrente invariata + log presente; calibration runner `ibis eval decisions` (stesso pattern A1).

**Accettazione:** in shadow, agreement ≥80% su dataset calibration e log presente per ogni consolidate; nessun cambiamento di comportamento mentre shadow=true.

---

## D12 — Response cache a gradini (fase 3, parte 3)

**Aggancio:** nessuna cache esistente (L2 §3.4). Call-site candidati deterministic: `analyzer.rs:638` (analisi semantica per file log), `rule_synth.rs:350` (già cooldown per fingerprint, rule_synth.rs:54-57), `ast_rule_gen.rs:234` (già once).

**Flusso attuale:** ogni invocazione rifà la chiamata LLM anche per input identici (il cooldown blocca la ripetizione ma non la riutilizza).

**Trasformazione (v1: solo exact-match):**
1. Nuova tabella `llm_response_cache`: `key` UNIQUE = `{call_site}:{model}:{sha256(full_prompt)}`, `response`, `created_at`; TTL via prune (pattern hotcache).
2. Wrapper `CachedGenerate::generate_content(...)` (in ibis-ai, compone con `AIProvider` come il router): lookup → hit ritorna la Candidate ricostruita; miss → chiamata + write-back. Applicato ai 3 call-site deterministici.
3. **Invalidazione**: la chiave include `index_head` (oid HEAD) per analyzer (l'analisi dipende dallo stato indice) e hash della dir `rules/` per rule_synth/ast_rule_gen — cambio contesto = cache miss naturale.
4. **Semantic cache esclusa dalla v1**: per il pattern ddofds (soglia = proprietà del traffico, negation trap, hit sbagliata con 200) richiede calibration e metrica di errore dedicata; eventuale v2 solo su task dimostrati ripetitivi e con soglia ≥0.95 misurata su dati reali.
5. `ask_project`/agentic **mai** cachati (v1 e v2).

**Casi limite:** prompt con byte non-deterministici (timestamp) → audit dei 3 call-site (nessuno ne ha in v1; verificare analyzer include offset/timestamp del file log — se sì, chiave sul prompt intero li incorpora, ok); risposta cache per model diverso → chiave include model; DB assente → wrapper trasparente (no-op).

**Test:** hit/miss con mock AI (contatore chiamate); invalidazione su cambio `rules/`; prune TTL; pass-through senza store.

**Accettazione:** seconda analisi dello stesso log (stesso HEAD) non invoca LLM (contatore = 0); nessun impatto su ask/agentic.

---

## Riepilogo dipendenze fra interventi (grafico funzionale)

```text
A1 (eval) ──valida──► A2 (RRF) ──base──► A3 (reranker)
   │                     ▲                    ▲
   │                     └── A4 (expansion) ──┘ (stesso ranking path)
   ├──valida──► D10 (routing tier) ──provider──► A3, D11
   ├──baseline─► D13 (prompt stability, metriche cache)
   └──shadow────► D11 (decide)
C9 (soglia+resolution) ──indipendente──► C7 (details) ──dati──► C8 (fase 4)
B6 (embedding cache) ──indipendente──► (raggiunge batch_manager e i 7 call-site embed)
D12 (response cache) ──usa──► chiavi con HEAD/rules hash ──dipende da──► nulla (v1)
```

## Prossimo passo

Convertire le fasi 0–1 (A1, poi A2 + C9 + D13 + B6) in un piano di implementazione passo-passo con milestone, ordine dei commit e criteri di accettazione per step (ogni spec di questo documento è già il contratto funzionale del corrispondente milestone). Le fasi 2–3 partono solo con la baseline A1 verde.

**Aggiornamento 09/11:** fasi 0–3 complete (A1, A2, A3, A4, C9, D13, B6, C7, D10, D11, D12). Le specifiche L3 delle fasi 4–5 per C8, E14 ed E15 sono in [2026-11-09-dailydoseofds-improvement-analysis-l3-fasi-4-5.md](./2026-11-09-dailydoseofds-improvement-analysis-l3-fasi-4-5.md).