# Master work queue — indice dei piani in sospeso

**Data:** 2026-11-02
**Scopo:** un solo punto in cui non perdere il filo tra i piani di lavoro in corso/pianificati. Questo documento è **l'indice e lo stato**; il dettaglio operativo resta nei rispettivi piani (link sotto). A ogni cambio di stato si aggiorna SOLO la riga di tabella e la sezione "prossimo passo" dello stream interessato.

**Regole:**
1. Ogni nuovo piano/spec, appena approvato, si registra qui con ID `WQ-n`.
2. Ogni stream ha **un solo "prossimo passo"** concreto. Se non c'è, lo stream è in attesa e la colonna "Sbloccato da" dice perché.
3. Le decisioni di coordinamento (dove due piani si toccano) vivono in §3 e vengono aggiornate quando cambia qualcosa.

---

## 1. Vista unica

| ID | Stream | Doc di riferimento | Stato (02/11) | Prossimo passo (uno solo) | Sbloccato da | Sblocca |
|---|---|---|---|---|---|---|
| WQ-7 | **SurrealDB 4** — engine greenfield (crate + sidecar 4.0.0, DiskANN, id array, grafo materializzato, full-text, fusione) | [2026-11-05-surrealdb-4-upgrade.md](./2026-11-05-surrealdb-4-upgrade.md) | **DONE (05/11)**: crate `surrealdb` 4.0.0 (workspace verde), schema level 4 greenfield (DiskANN su 5 tabelle embedding con `array<f16>` — B5 assorbito; `DEFINE TYPE` per batch_status/kind/status/visibility; id array `[repo, file_path]` / `[file_id, line]` su source_file/file_chunk con indici file_path/file_version caduti; canale lessicale FULLTEXT BM25 su memory.title/content + file_chunk.content; `REFERENCE … MATERIALIZE INTO` sui lati single-typed degli archi del read path — union non materializzabili, nota in spec), canali `ORDER BY vector::distance::cosine` indicizzati con decay come rescore Rust sulla finestra, finestra candidati `search.candidates` (50), LIMIT+ORDER BY su 16 query, CLI 4.0 (spawn `start` + `surrealkv://`, probe `version --json`, asset Windows `.exe`) — il binario GitHub NON ha `db` subcommand (nota spec). Verdetto fusione (A2 chiuso): default `additive` per misura ([nota](./2026-11-05-wq7-fusion-verdict.md)); baseline 4.0 nel piano A1 (offline/ollama = 0.917/0.958/1.0 additive, 12/12, gate 0.75 passa). Shaped/trace non esposti dal Rust SDK 4.0.0 (rimandati). Live test workspace verdi su binario 4.0.0 (schema init idempotente incluso). **Remediation pass (05/11, `79bde42`..`90a3391`)**: i 13 finding della review ([piano](./2026-11-05-surrealdb-remediation-agent-plan.md)) chiusi con test unitari+live (id lossless F02, chunk fallback F01, invalidazione embedding F08, versioning F10, pruning atomico F12, binding transazione commit F07, id canonici log F09, offset retry-safe F11, migrazioni ordinate F13, fusione cross-channel F03, calibrazione BM25 F04, canale file_chunk_text F06, subject su titolo F05); dataset esteso a 20 case, baseline post-fix additive 0.950/0.975/1.0 (rrf k1/10/60 sotto), verdetto `additive` riconfermato su stack corretto; job CI `live-tests` con `IBIS_LIVE_REQUIRED=1` | — | R7 (WQ-3); C9 (WQ-5); D1 su immagine 4.0 (WQ-4) |
| WQ-1 | **R3/R4 gate** — VSIX 1.0.11 + plugin agent (Parte B) | [review-r3r4-checklist.md](./review-r3r4-checklist.md), [runbook R3.3](./2026-11-03-r33-closure-runbook.md), [analisi tempi S1.3](./2026-11-03-s13-timing-analysis.md) | **CHIUSO (03/11)**: Parte A 26/26 PASS; Parte B completa (R3.1–R3.8 + P4.1–P4.6 PASS) — run interattivo con evidenza da log; cold target chiuso dal pre-warm (prima fetch 12–30ms dopo boot pagato all'attivazione, 3.3–4.7s con Ollama su); note: P4.1 UX (status in chat), P4.5 tool-ref fix + superficie = mode picker, noise hook da estensione Azure Tools (checklist §Esito) | **push differito (decisione owner 03/11)** — a fine sviluppo: riscrittura dell'intera storia in pochi commit precisi (include rimozione claim/fact di test) → push | — | F4 (WQ-3) sbloccata |
| WQ-2 | **Publish gate** (azioni ② ③ ⑤ + `publish-vscode.yml`) | [review-r3r4-checklist.md §Publish gate](./review-r3r4-checklist.md) | **Differito (decisione owner 03/11)**: publish a sviluppo concluso, non a R3/R4 — vedi §3.6 | azioni manuali ②③ (Trusted Publishing OIDC, Open VSX token) al momento del publish | fine sviluppo (decisione owner su timing) | WQ-4 (D-CLAIMS gate pre-publishing) |
| WQ-3 | **F4 — Memory view + team server** (VSIX 1.1.0 / runtime 0.3.0) | [2026-11-01-memory-view-team-server-design.md](../superpowers/2026-11-01-memory-view-team-server-design.md) | **F4-A5 done (05/11, `6d24e72`) — Milestone A completa** — actor identity header + claims mode knob: setting `ibis.identity.actor` (+ env `IBIS_ACTOR`, precedenza setting>env condivisa col guard A4), il backend http VSIX porta `actor` e ogni richiesta ide-host `postJsonRpc` (initialize incluso) manda `X-Ibis-Actor`; bridge http (`ibis-mcp-host`) forwarda `IBIS_ACTOR`; setting `ibis.claims.personalMode` (stub default \| db_only) forwardata come `IBIS_CLAIMS_PERSONAL_MODE` solo se esplicita (spawn + Start Daemon; prereq runtime `feat(server)` `24e799e`); Status riporta actor + claims mode. VSIX **1.0.15**, smoke-plugin 47/47 (4 check nuovi), nuovo `http_actor_test.mjs` nel job CI ide-host, cargo workspace verde (unico fallimento: finding pre-esistente ibis-ai/Ollama). Pretest compose su pin `v3.3.5` (05/11) **non** chiude R7. **R7 CHIUSO (05/11)** — checklist §8 scripted green 0–5 + §8.3 client reali (VSIX attach `fabio1` + plugin http `fabio-plugin`, attribuzione §4 su entrambe le superfici) + estensione LAN multi-host (WSL mirrored, `owner=fabio-lan2` verificato server-side); evidenza `.r7-pretest/r7-gate-checklist.out.txt` (dir locale gitignored), dettaglio nello stream WQ-3 | **F3 — docs/posizionamento** ([PLAN.md §8](./PLAN.md)) — atterrata 07/11 (README "Ibis Assistant vs Copilot Memory" + feature matrix + `docs/ide-integrations.md` + drift 56→58 tool + skill `ibis-sync-copilot-memory`, smoke-plugin 48/48); prossimo: Track D D1 (WQ-4) sull'immagine certificata `v4.0.0` | — (R7 chiuso 05/11) | Track D (WQ-4) |
| WQ-4 | **Track D** — benchmark DevAX/Scope (D1–D5) | [PLAN.md §9](./PLAN.md), [evaluations/scope/README.md](../evaluations/scope/README.md) | Pianificato (scaffold presente; profili con placeholder `TODO-verify-*` by design, pre-D2). D1 pinna l'immagine **4.0.0** uscita da WQ-7; D3 confronta l'agente con la baseline A1 di WQ-7 | D1 — pin immagine compose `v4.0.0` + seed su istanza Scope live | WQ-7, WQ-3 (gate R7), azione D1 | D2–D5, R6 close |
| WQ-5 | **Miglioramenti ddofds** — fasi 0–5 (eval, retrieval, costi, memoria) | [analisi L1](./2026-11-01-dailydoseofds-improvement-analysis.md) → [L2](./2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md) → [L3](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md); [piano A1](./2026-11-02-a1-retrieval-eval-plan.md); [validazione A2](./2026-11-03-a2-rrf-validation.md) | **Fase 0 (A1) done** (commit `9fb53da` 03/11): dataset 12 case + runner + CLI `ibis eval retrieval` + job CI `retrieval-eval`; baseline offline MRR 0.917 / r@5 0.958 / r@15 1.0, baseline ollama (additive) MRR 0.917 / r@5 1.0 / r@15 1.0. **Fase 1 (A2) implementata 03/11**: `rrf_fuse` + `FusionMode` + config `search.fusion` (default **additive**) + eval `--fusion` + campo `fusion` nel report; validazione: offline parità, ollama **regressione** (MRR 0.875, r@5 0.958 — perdita margine r05/r11). **Decisione owner A2 (03/11): opzione 2 — tuning RRF prima di decidere** (default resta additive). **05/11:** retune A2 e B5 assorbiti da WQ-7 — **chiusi lì**: baseline 4.0 = 0.917/0.958/1.0 (offline e ollama additive, 12/12), retune RRF k∈{60,10,1} non batte additive (ollama 0.875/0.958 stabile), default resta `additive` per misura ([verdetto](./2026-11-05-wq7-fusion-verdict.md)); B5 non esiste più come colonna (DiskANN quantizza in engine). **C9 done (07/11)** — soglia `ai.consolidation.dup_similarity_min` (default 0.80) + pre-match deterministico su titolo normalizzato (zero chiamate AI); nota: il knob vive in `ai.consolidation`, non `optimizer.*` (namespace inesistente nel config di prodotto). **D13+B6 done (08/11)** — D13: convenzione prompt-stability per costruzione su 5 call-site (`ANALYZE_INSTRUCTIONS`, `DISCOVER_FORMAT_INSTRUCTIONS`, `AST_RULE_INSTRUCTIONS`, `INGEST_SYNTH_INSTRUCTIONS`, `DISTILL_INSTRUCTIONS` — const byte-stabili + snapshot test per-crate, istruzioni → contesto → query ultima; fingerprint estensione e proposta linguaggio spostati fuori dal prefisso) + cached-token billing (`UsageMetadata.cached_content_token_count` mappata da Gemini `cachedContentTokenCount`, OpenAI-compat `prompt_tokens_details.cached_tokens` [ora anche usage parsing in openai_compat/claude], Claude `cache_read_input_tokens`; `CostTracker.record_usage` con `cached_tokens` → billed = prompt − 0.9×cached saturante, debug log del risparmio, blended $0.15/1M invariato; niente `cache_control` Anthropic in v1); B6: L2 `embedding_cache` su SurrealDB (`schema.rs` tabella + UNIQUE su `key`; chiave `{model}:{dim}:{sha256(text)}` con dim da `ibis_db::schema::EMBEDDING_DIMENSION` const + test di sync con i 5 DDL DiskANN), seam `ibis_ai::EmbeddingCacheStore` (`get`/`put`/`prune`, best-effort), `OllamaProvider::with_disk_cache` (L2 consultata prima del batch HTTP, hit non accodano sul FIFO, write-back dopo fetch, mismatch dim = miss), adapter `ibis-server::assembly::DbEmbeddingCacheStore` (record id = sha256 della chiave, UPSERT idempotente), config opt-in `ai.embedding.disk_cache` (default false) + `disk_cache_ttl_secs` (default 30d, prune al boot), wiring `state.rs` (nessun DB → L2 disabilitata con log); live-probe 4.0: `DELETE … RETURN AFTER` rende `NONE` (righe già assenti post-op) → il prune usa `RETURN BEFORE` (contratto validato su istanza 4.0.0 locale); caveat: il prune `file_at_commit` di hotcache usa ancora `RETURN AFTER` — il conteggio loggato su 4.0 risulterà 0 (prune comunque operativa), da riallineare quando si tocca ibis-search. **A4 done (08/11)** — `search.query_expansion` (`off`|`symbol_variant`, default `symbol_variant`) + `expand_query` (token CamelCase riuniti + simboli linked appesi, dedup, cap 20) + doppio pass vettoriale con merge `score = max(passi)` per riga (canali lessicali invariati, fail-open); eval `--query-expansion` + campo `query_expansion` nel report, seed `symbol` e case `r21`; parità off/on sul golden 21 case (offline 0.952/0.976, ollama 0.952/1.0) e prova live del firing (13 vs 33 token su ask singolo); follow-up: linking fuzzy (containment, coverage>=2 o token>=8, cap 5) + eredità in ask_ref; **A3 atterrata (08/11)**: reranker post-fusione opzionale (`search.rerank` off|local, default off; hint `bulk_local`, blend `base_norm*0.4+llm*0.6`, fail-open, costo `system:search_rerank`, eval `--rerank`+`--rerank-model`+campo report; offline parità 0.952/0.976, ollama giudice granite4.1:3b 0.929/0.976 vs 0.952/1.0 → default off confermato); **C7 atterrata (08/11)**: `details` tipizzato per kind (`option<object> FLEXIBLE`, rubric decision→scope / fix→root_cause / constraint→applies_to / rule→enforcement, policy `memory.schema_enforcement` warn|strict, supersedes stesso kind, prompt distill con details + retry strict una volta, filtro `memory_search` su `details.<key>`, proiezione in list/search/export; live 4.0: roundtrip + filtro, estate 13/13); **D11 atterrata (08/11)**: primitiva `decide()` in `ibis-ai` (`DecideQuestion` Noul|Choice, parser stretto — JSON invalido/probs non normalizzate/opzioni ignote → astensione, confidence `max−second`, hint `local:S`, `decide_with_hint` per `local_only:S`; temp 0.001 perché 0.0 è la sentinella "unset" del provider layer) + shadow mode `ai.decide.shadow` default true nel consolidation (stato osservazione+candidati, domanda `is_noise`, log `decide_shadow` con `decision_llm`/`probs`/`confidence`/`agreement`/rubric, comportamento invariato; shadow `local_only:S` anche per il team (misura il modello S, mai cloud); pre-match C9 resta zero-AI; errore → shadow disabilitato per il pass con un warn) + dataset `evaluations/decisions/cases.yaml` (40 case, 20 noise/20 durable, rubric `is_noise.v1`) + `ibis eval decisions` (ollama|openai_compat, soglie auto/escalate, gate `--min-agreement`, report JSON). Misura 08/11 (qwen3.8-27b via openai_compat, tier-S granite non presente sull'host): 40/40 ok, agreement 0.975, mean confidence 0.865, auto 35/40 a θ 0.75 con accuracy 1.000, 4 escalated, 1 default (unico miss in default bucket, conf 0.20; run precedente 1.000); **D12 done (09/11)** — cache risposte exact-match (`ibis_ai::response_cache`: `ResponseCacheStore` best-effort, `CachedGenerate` decorator, chiave `{call_site}:{model}:{sha256(context, full_prompt)}`; direttiva locale `GenerationConfig::response_cache` `#[serde(skip)]`; `AIProvider::cache_identity` con fingerprint di router/chain/cloud pool; tabella `llm_response_cache` + UNIQUE e adapter `DbResponseCacheStore`; config opt-in `ai.response_cache` default off, TTL 7d con prune al boot; 3 call-site: `log_analyze` con HEAD, `rule_synth`/`ast_rule_gen` con fingerprint `rules/*.yml`; hit a costo zero, errori/risposte vuote mai cachati, ask/agentic/rerank intatti; live 4.0 roundtrip/miss/prune/UNIQUE verde); **L3 fasi 4-5 specificate (09/11)** — [doc](./2026-11-09-dailydoseofds-improvement-analysis-l3-fasi-4-5.md): C8 (Observations deterministiche, `store=derived` read-only, firme/union-find/`evidence_hash`, hook `InvalidateOnly` + pass `Full`, `ai.observations` opt-in) + E14/E15 (contratto `AgenticCitation`/`AgenticTrust`, trust deterministico claim-level, budget/no-progress/completion `search.agentic.*`, `capture_observation.citations_json`); nessun nuovo tool (58), C8 e E15 paralleli, E14 dopo E15 | implementazione fase 4: **C8 step 1-4 + E15 in parallelo**, poi **E14**, poi **C8 step 5-6** (LLM/hook/eval) | - | fase 5 da specificare L3 (E16+F17+G19+G20) |
| WQ-6 | **Track B** — ingest tracce Beacon (B0–B4, B3 se promossa) | [PLAN.md §9-bis](./PLAN.md) | Pianificato (approvato 03/11; solo piano, nessun codice). Quando B3 viene promossa, lo schema nasce sulle convenzioni WQ-7 (id array se la chiave è composta, DiskANN, full-text, `DEFINE TYPE`, archi materializzati) | B0 — installazione endpoint Beacon (azione manuale ⑥) + campionamento `runtime.jsonl` → nota `evaluations/beacon/campionamento.md` | azione manuale ⑥ (owner) | B1–B4, R8, B3 (se promossa) |
| WQ-8 | **StrixEmbed / embeddings esterni** — provider OpenAI-compat configurabile con dimensione vettore variabile (default resta Ollama automatico) | questa riga (probe live 05/11, nessun codice) | Registrato (05/11): servizio Windows NSSM `StrixEmbed` attivo — `llama-server.exe` con `nomic-embed-code.Q6_K.gguf --embeddings --pooling last -c 8192` su `127.0.0.1:8081`, API OpenAI `/v1/embeddings`, probe live `dims=3584 model=nomic-embed-code`. Stato attuale: il client embedding parla solo Ollama `/api/embed` (`state.rs` provider `ollama` default / `gemini`; `openai_compat` e `cursor_lm` reasoning-only) e lo schema DiskANN è pinnato `DIMENSION 768` — un endpoint 3584 come StrixEmbed non aggancia l'indice | aprire spec: **default invariato — Ollama automatico** (`ensure_ollama` auto-start, `nomic-embed-text` 768, baseline A1 intatta, wizard/config come oggi). Si aggiunge la **configurabilità di un endpoint OpenAI-compat esterno**: `EMBEDDING_PROVIDER=openai_compat` + `EMBEDDING_URL`/`EMBEDDING_MODEL` (`POST /v1/embeddings`) in ibis-ai, e la **dimensione vettore variabile** (`ai.embedding.dimensions`, default 768) — lo schema DiskANN (`schema.rs`, 5 indici) deriva la DIMENSION dalla config al primo boot greenfield, la valida e rifiuta con errore onesto un boot successivo con dimensione diversa (la dim è una scelta a vita del datastore: niente migrazioni, coerenza col greenfield WQ-7); validazione dimensione anche in `value.rs` (Q12) e `batch_manager`; wizard `config fast` propone Ollama automatico e la via esterna solo se esplicita. Con la dim variabile StrixEmbed serve `nomic-embed-code` a 3584 direttamente (no GGUF 768 interpone) | — | embeddings senza Ollama sulle macchine con StrixEmbed (servizio sempre-on, no pull/warm-up); qualunque endpoint OpenAI-compat (LAN/cloud) con la sua dimensionalità; wizard/settings per la scelta provider+dim |

Storico chiuso (non più in sospeso): F0.1–F0.7, gate R2, runtime 0.2.0 — vedi [PLAN.md](./PLAN.md).

## 2. Sequencing riconciliato

Fonti: PLAN.md (sequencing originale), F4 spec (sequencing aggiornato, 01/11), checklist R3/R4 (stato reale).

```text
[R3/R4 Parte B]──PASS──►[F4-B1..B4 ∥ F4-A1..A5]──►[WQ-7 SurrealDB 4]──►[R7]──►[F3 docs]──►[Track D D1..D5]──►[R6]──►[Publish gate ②③⑤]
     (WQ-1)                    (WQ-3)                  (crate+sidecar 4.0.0)         (WQ-4)                      (WQ-2, differito)

runtime-side, dopo l'engine:
      [A1 fatto] ──► [WQ-7: DiskANN + full-text + verdetto fusione; B5 assorbito] ──► [C9 + D13 + B6 + A4] ──► [fasi 2–3 done] ──► [L3 fasi 4–5: C8/E14/E15 specificate]   (WQ-5)
```

Nota di riconciliazione: PLAN.md aveva `…R3/R4 → F3 → D1..D3 → R6`; l'F4 spec (più recente) inserisce `F4 → R7` tra publish e F3. **Aggiornamento 03/11 (decisione owner §3.6)**: il publish gate non segue più R3/R4 ma chiude la sequenza a sviluppo concluso (dopo R6/R7, timing a scelta owner); F4 è sbloccata direttamente. **Aggiornamento 05/11 (§3.8)**: WQ-7 sta davanti a R7 e al resto di WQ-5. Il retune A2 e B5 non sono più step propri.

## 3. Decisioni di coordinamento (dove i piani si toccano)

1. **Superficie tool 57 → 58.** F4-A3 introduce `memory_workstreams` e un filtro su `memory_list` (design F4 §A.3). Ogni intervento ddofds che tocca il registry/contratti (nessuno delle fasi 0–1; D10 cambia solo hint, non tool) deve seguire il pattern consolidato: contract test + discovery fixture aggiornati nello stesso commit.
2. **Consolidamento: F4-B3 (daemon-only, opt-in) vs C9/D11 (ddofds).** C9 (soglia+entity resolution) e D11 shadow-mode cambiano gli *interni* di `consolidate_loop`, non il trigger: compatibili con F4-B3 e landabili in qualsiasi ordine. D12 (response cache) non tocca il consolidation. Se F4-B3 atterra prima, C9/D11 lavorano sulla versione daemon-owned; se dopo, nessun conflitto previsto.
3. **A1 eval → Track D D3.** La baseline di retrieval quality (A1) è complementare al D3 "memoria → outcome": A1 misura l'ibis interno, D3 misura l'agente end-to-end. Fare A1 prima che D1..D3 parta in earnest rende le fasi 2–3 ddofds misurabili con Track D in parallelo. **05/11:** i numeri 3.3.5 (MRR 0.917 / r@5 0.958) non sono più il contratto di D3. D3 usa la baseline che WQ-7 pubblica dopo DiskANN e il canale full-text.
4. **Timeout/sidecar.** WQ-1 S1.1 (client stdio persistente) e il rebuild del binario stale sono prerequisiti di misura per S1.3; l'F4-B1 (reconciliation ledger) complica il sidecar lifecycle — fare S1.3/S1.4 PRIMA di F4-B1 evita due volte le stesse misure.
5. **Versioni.** VSIX: le build debug iteracono il numero di **patch** (`1.0.1`, `1.0.2`, … — decisione owner 03/11, a ritroso dalle 1.1.0–1.1.2 provvisorie mai committate) e restano distinguishibili dalla versione a ogni reinstall; `1.1.0` resta riservato a F4. Runtime resta 0.2.0 finché il root `[Unreleased]` non viene taggato (decisione in checklist §Publish gate). F4 porterà runtime 0.3.0. Gli interventi ddofds vanno nel root `[Unreleased]` runtime, nessuna bump propria.
6. **Publish differito (decisione owner 03/11).** Il gate WQ-2 non segue più R3/R4: si pubblica a sviluppo concluso (indicativamente dopo R7/F4, timing a scelta owner). Conseguenze: il primo rilascio pubblico sarà **1.1.0** (F4) invece di 1.0.0 — le patch 1.0.x restano build locali mai pubblicate; le azioni ②③ (OIDC Trusted Publishing, token Open VSX) restano manuali owner e si fanno al momento del publish.
7. **Push differito + riscrittura storia finale (decisione owner 03/11).** Nessun push fino a fine sviluppo; a quel punto riscrittura dell'**intera** storia (47+ commit) in pochi commit precisi, che include la pulizia già registrata (claim `mem_26f1730…`/`mem_973f67a…` e le 2 fact DB-only via `memory_delete` — le fact DB-only non vivono in git, la loro rimozione resta operativa sullo store locale). Le build/prove intermedie non sopravvivono al rewrite: la coda e i doc di planning restano la fonte della verità.
8. **SurrealDB 4 prima di R7 e del resto di WQ-5 (05/11).** Il prodotto non ha una release pubblica: il datastore nasce vuoto su sidecar e crate **4.0.0**, senza upgrade del file 3.x. WQ-7 adotta DiskANN (B5 si chiude qui: niente colonna int8), `array<f16>` se l'indice lo accetta, id array su `source_file` e `file_chunk`, full-text come canale, `MATERIALIZE` su tutti gli archi del read path, e il verdetto di fusione (A2 si chiude qui, `additive` non resta il default per inerzia). R7 certifica l'immagine `v4.0.0`. C7 estende i `DEFINE TYPE` già introdotti e non ridisegna gli id. C8 e C9 leggono liste materializzate e il lookup DiskANN. D10–D13 non toccano l'engine. Dettaglio: [2026-11-05-surrealdb-4-upgrade.md](./2026-11-05-surrealdb-4-upgrade.md).

## 4. Stato per stream (dettaglio minimo)

### WQ-7 — SurrealDB 4
- **DONE (05/11).** Esecuzione completata — dettaglio, numeri e deviazioni
  dalla spec (binario GitHub senza `db` subcommand, union non
  materializzabili, shaped/trace non esposti dall'SDK) in
  [2026-11-05-surrealdb-4-upgrade.md §Note di esecuzione](./2026-11-05-surrealdb-4-upgrade.md#note-di-esecuzione-0511--dove-la-spec-incontra-la-40-reale).
  Crate 4.0.0, schema level 4 greenfield, retrieval indicizzato, verdetto
  fusione, baseline 4.0 pubblicata, live test verdi su binario 4.0.0.
- **Prossimo passo della catena: gate R7** (manuale owner) sull'immagine
  `v4.0.0` — vedi riga WQ-3. C9 e D1 sbloccati.
- **Corretto (05/11, gate R7):** `load_session_context` con `project_name`
  falliva su 4.0.0 — il filtro grafo di progetto insieme a
  `ORDER BY vector::distance::cosine` (introdotto da WQ-7, `ad00490`) dava
  `IR verification failed`. `string::slice` non era la causa. Fix: la finestra
  DiskANN esce senza il predicato grafo e lo scope filtra in Rust
  (`search_memory`, regression test inclusi). Dettaglio nella
  [nota di esecuzione](./2026-11-05-surrealdb-4-upgrade.md#da-correggere).
- A2 e B5 chiusi dentro WQ-7; nessun passo proprio.

### WQ-1 — R3/R4
- Fatto: Parte A 26/26 (30/10); R3.1/R3.2 PASS; S1.1 stdio persistente (smoke 26/26); S1.2 igiene ambiente owner (pluginLocations corretto, binario Go stale rimosso). Blocker rebuild binario risolto (02/11: PATH `ibis 0.2.0`, Go stale gone).
- **Sessione debug owner conclusa (03/11)** — run S1.3 eseguito su build
  provvisorie 1.1.0–1.1.2 (strumentazione S1.3 lato VSIX/ide-host: spawn /
  cold-init / reuse / timing per tool call; mai committate). Riscontri owner
  implementati in **1.0.1** (numerino sul tree parent con guard convergente,
  refresh con spinner — il refetch esisteva già su 1.1.0, il difetto era solo
  di feedback) e **strumentazione rimossa** (fine sessione debug). Seed
  albero + runbook + [analisi tempi](./2026-11-03-s13-timing-analysis.md)
  (warm PASS ampio, cold fallito per boot tax — SurrealDB 1–4s, Ollama giù
  +8.7s). Chiusura VS Code verificata: zero processi orfani (job object ok).
- **R3.3 chiusa (03/11, registrata in checklist)** — frasi B/C verificate su
  1.0.1 dal run owner (raffiche `memory_list`/`memory_expand`/refresh ok in
  plugin.log 07:05–07:10 UTC); pass di misura S1.3 registrato (S1.3 done).
  Gap noto: cartella evidenze `artifacts/ui-tests/<run>-r33/` mai
  persistita (§6.4 analisi).
- **Decisione cold target (03/11, opzione 1: pre-warm all'attivazione)** —
  atterrata in VSIX **1.0.3** (`bf824b1` + fix): fire-and-forget in
  `activate()` — sessione stdio persistente + handshake + null call
  read-only `memory_list` (shape frase B dell'albero; 1.0.2 usava
  `ticket_get_capabilities`, rifiutato dal dynamic tool discovery senza
  ticketing — finding run owner 03/11: "pre-warm failed after 14944ms",
  boot comunque pagato dall'handshake); evidenza su canale Output
  (`pre-warm: runtime session ready in Nms`); la prima interazione reale
  riusa la sessione calda (stessa identità backend).
- **S1.4 atterrato (03/11, `e9b85b8`)** — timing `memory_search` demotati
  info→debug (invisibili al default INFO, tornano con `log_level DEBUG`);
  righe nuove a info `memory_list: done in … entries=N` /
  `memory_expand: done in … chain=N` (parità di evidenza frasi B/C, analisi
  §6.3). cargo test + clippy verdi.
- **Parte B COMPLETA (03/11 pomeriggio, run interattivo)** — R3.4–R3.8 e
  P4.1–P4.6 PASS con evidenza da log (sink `%LOCALAPPDATA%\ibis\ui-tests\`
  + plugin.log); **verifica pre-warm chiusa**: boot pagato all'attivazione
  (3.3–4.7s con Ollama su), prima fetch dell'albero 12–30ms end-to-end
  (target warm rispettato, il "cold" non esiste più sul path tree).
  Interventi atterrati durante il run: VSIX 1.0.4 (sink log) → 1.0.5
  (refresh conteggi collassati) → 1.0.6 (fix multiplexing parallelo
  ide-host) → 1.0.7 (auto-refresh post-capture + expand-all) → 1.0.9
  (expand-all rimosso, decisione owner) → 1.0.10 (fix media walkthrough) →
  1.0.11 (Quick Actions view); runtime: log esiti/rejection per tool call;
  plugin: tool-ref agenti in formato Copilot + nota brevità. Finding e gap
  nel checklist §Esito (incluso il `.mcp.json` smoke R2 rimosso e il rumore
  hook Azure Tools). Restano le azioni owner ②③ per il publish gate.
- Aperto: azioni ②③ (Trusted Publishing OIDC, token Open VSX) → publish
  gate (azioni ⑤ già fatte per i review).
- **Pulizia memorie di test (aggiornata 03/11: push differito, §3.7)**: la
  rimozione non è più una passata separata pre-push — finisce nella
  **riscrittura dell'intera storia** a fine sviluppo (claim in-repo
  `mem_26f1730…` commit `b112bd7` e `mem_973f67a…` commit `1f64f7f`
  eliminati nel rewrite; le 2 fact DB-only "Memoria fact di prova" /
  "Quick Actions fact" `mem_ff975f41…` vanno rimosse dallo store locale via
  `memory_delete`, non vivono in git). **Restano** le 3 memorie seed
  (decision/fix/gotcha, claim `a4cd24f`/`1d00552`: contenuto reale di
  progetto).

### WQ-3 — F4
- **F4-B1 atterrata (03/11, `e28d936`)** — `ibis_core::sidecar_ledger` (ledger
  JSON accanto al file dati SurrealDB: `pid`+start-time+`port`+`pgid`+owner
  identity; `reconcile` prima dello spawn: prune morte / keep con owner vivo
  / terminate orfane con owner morto; PID-reuse escluso via start-time,
  Windows FILETIME + `GetExitCodeProcess`≠STILL_ACTIVE — un terminato con
  handle aperti non è vivo; Linux `/proc` field 22; macOS kill-only,
  residuo documentato) + wiring `ProcessManager` (reconcile pre-spawn con
  ledger dir = parent del file dati, register solo per child confermato
  vivo, unregister post-stop riuscito). Test: 7 unit ibis-core (stale-kill
  e2e con sleeper reali) + live gated `IBIS_SURREAL_BIN` (owner crash
  simulato → restart pulito, 3.9s) + lifecycle riallineato. Workspace
  verde, clippy `-D warnings` verde.
- **Finding follow-up (da B1) — RISOLTO (03/11, commit fix)**: `daemon.rs`
  `stop_process_raw` apre con `TERMINATE|SYNCHRONIZE` senza query right →
  `GetExitCodeProcess` fallisce e il grace-loop D10 può leggere un processo
  vivo come già morto (mascherato col console condivisa, latente per child
  pipe-spawned). Fix: `PROCESS_QUERY_LIMITED_INFORMATION` nella maschera +
  test di regressione che pina il contratto exit-code attraverso la maschera
  esatta dello stop (`stop_handle_query_right_reads_still_active_for_a_live_process`).
  Verificato: ibis-server daemon tests 18/18.
- F4-B1 conclusa anche la parte sidecar-lifecycle-plan §Layer 3 (piano
  aggiornato, [sidecar-lifecycle-plan.md](./sidecar-lifecycle-plan.md)).
- **F4-B2 atterrata (03/11, `68c4c9d`)** — `ibis serve` alias di `run` (anche
  `-serve`/`--serve`) + profilo `--team` (F4 §B.1): nuovi flag server
  `--bind <addr>` e `--data-dir <path>` (normalizzazione single-dash estesa);
  il profilo applica i default SOLO sui knob non impostati — runtime_mode →
  `server` (personal claims `db_only` via resolver esistente), bind →
  `127.0.0.1` (esposizione LAN solo con `--bind` esplicito; config
  `bind_address` rispettato), proiezione `.ibis/memory` on salvo
  `memory.projection_enabled: false` esplicito nel config.json (nuovo
  `ibis_core::config::projection_explicitly_set`); no-op in plugin isolation.
  Banner startup `[TEAM]` a 4 righe: bind address + reminder header
  `X-Ibis-Actor` (CLI: `IBIS_ACTOR`) + boundary no-auth = reachability di
  rete + proiezione attiva. `--data-dir` override raw su `db_data_path`
  (stessa semantica CWD-relative degli altri flag; runbook B4 documenterà
  path assoluti). Test: 5 test profilo + single-dash flags (ibis-server),
  unit helper config (ibis-core); smoke live: banner `[TEAM]` + sidecar
  SurrealDB con data-dir isolata. Workspace verde (cargo test --workspace,
  clippy `-D warnings`).
- **Finding (da B2, non bloccante)** — `ibis_ai::runner::tests::default_api_url_substituted`
  fallisce quando Ollama è attivo sull'host (test `unwrap_err` sull'esito
  ensure-ollama, riceve `Ok(EnsureOllamaOutcome { process: None })`):
  coupling test↔ambiente, pre-esistente (verificato con stash del diff B2).
  Follow-up candidato: rendere il test indipendente da Ollama raggiungibile.
- **F4-B3 atterrata (03/11, `51c89d6`)** — consolidation loop daemon-only
  opt-in (F4 §B.5 / decisione T3): flag `--consolidate` (anche single-dash) +
  config `ai.consolidation.enabled` / `interval_secs` (off di default, floor
  60s, default 1h; flag > config). Nuovo modulo `ibis_server::consolidation`,
  gate: solo daemon sse non-plugin (stdio/plugin client surfaces mai);
  rifiuto del knob loggato (DB assente/degradato) — mai silenzioso. Discovery
  store-driven (`Optimizer::pending_observation_projects`: `SELECT project
  FROM memory WHERE kind='observation' AND status='current' GROUP BY project`,
  decodifica record `repo:*`) → una pass `consolidate_loop` per progetto
  (20 osservazioni, default del tool) + `maybe_project_memory` per progetto
  (parità col tool `memory_consolidate`, che resta il trigger umano: cron
  automazione plugin). Loop: primo tick immediato al boot (drain backlog),
  poi a cadenza; pass strettamente sequenziali (task singolo,
  `MissedTickBehavior::Delay`); stop col token di shutdown. Test: 4 gate
  unit + 2 discovery (fake db: repo distinte/junk scartate, errore propagato)
  + 3 flag (round-trip, override config) + 1 knob config; workspace verde,
  clippy `-D warnings` verde (unico fallimento: `ibis-ai
  runner::default_api_url_substituted` — finding pre-esistente B2, Ollama
  attivo sull'host). Nota residua (non bloccante): overlap in-process tra
  pass del loop e pass da tool concorrenti resta possibile (stessa race
  pre-esistente tra due `memory_consolidate` concorrenti); lease in-process
  candidato follow-up.
- **F4-B4 atterrata (03/11, `9921d6e`)** — packaging ufficiale del team
  server (F4 §B.2/§B.3): `deploy/Dockerfile` (build sorgente `ibis` con
  cache layer manifesti+stub → sorgenti reali; SurrealDB **3.3.5** dal
  tarball release — stessa versione pinnata dal job CI `retrieval-eval` e
  stessa fonte dell'installer runtime, glibc-safe su bookworm; ast-grep
  **0.51.1** dall'asset `app-*-unknown-linux-gnu.zip`, `sg`+`ast-grep`
  accanto al binario; `DB_AUTO_UPDATE=false` ⇒ nessun probe GitHub al
  boot; utente non-root uid 10001, `/data` volume per data file surrealkv +
  ledger sidecar, `/opt/ibis` scrivibile per `surreal.log` + sync regole,
  healthcheck su `GET /` discovery, CMD `serve --team --bind 0.0.0.0
  --data-dir /data/ibis-team.db --consolidate`) + `deploy/docker-compose.yml`
  (`build:` dal root repo, `init: true`, `restart: unless-stopped` (owner
  recovery senza job object), env commentate EMBEDDING_URL/LOG_FILE/
  SURREAL_USER/PASS, mount config.json opzionale, `image:` GHCR commentata
  — publish differito §3.6) + `.dockerignore` (contesto: workspace +
  `rules/`+`sgconfig.yml`) + `.gitattributes` (eol=lf sui tre artefatti
  docker: i RUN shell non sopravvivono al CRLF) + runbook
  `docs/team_server.md`: topologia/guardrail, install A compose e B
  systemd/NSSM (con `--data-dir` assoluti), flag+env, actor identity
  (header `X-Ibis-Actor` fidato **solo da loopback**; client LAN → argomento
  `actor` dei tool; impostazioni VSIX `ibis.identity.actor` + `IBIS_ACTOR`
  CLI arrivano con F4-A5), onboarding client (VSIX attach + `mcpUrl`,
  Cursor http variant, Claude Desktop via `mcp-bridge`, plugin http variant,
  CLI loopback-only today), ownership consolidation §B.5 (consigliato ON,
  automazione settimanale resta il trigger umano), backup/restore/upgrade,
  checklist verifica R7, note sicurezza. Verifica su host senza Docker:
  YAML validato (pyyaml), script stub RUN testato sotto Debian sh (19
  lib + 3 main = 22 target), tarball/zip sidecar ispezionati
  (`surreal`@root tgz, `sg`+`ast-grep`@root zip), asset `include_dir` tutti
  git-tracked; prima build immagine reale al gate R7 (scenario compose su
  LAN, 2+ client).
- **F4-A1 atterrata (03/11, `81fa0d3`)** — attach default al daemon personale
  (F4 §A.1): VSIX **1.0.12** con `runtime.mode` enum `auto`/**spawn**/**attach**
  e default `auto` → `resolveBackendSelection`: attach a `DEFAULT_MCP_URL`
  (override `IBIS_MCP_URL` / `runtime.mcpUrl` esplicito), provider definition
  `{ type: "http", url }` ora attiva di default e tutti i path comando via
  `callMcpTool` http. Status bar in daemon mode sonda l'endpoint
  (`settings.probeTimeoutMs`): reachable → `$(cloud) Ibis attach`, offline →
  `$(error) Ibis daemon offline` (tooltip con hint Start Daemon). Nuovo comando
  **Ibis Assistant: Start Daemon**: one-shot `<binary> start` (verbo PID-file
  del runtime — daemon detached, log su `server.log`, quindi outlives VS Code)
  + re-probe ~15s (boot SurrealDB sidecar); spawn error/timeout o endpoint mai
  raggiungibile → messaggio con istruzione manuale (`ibis start`/`ibis run`);
  **Status** in offline offre il pulsante Start Daemon. `spawn` esplicito resta
  bit-for-bit (autoAttach + probe urls inclusi; `warnDualPluginChildren` solo
  in spawn mode, messaggio aggiornato: spawn + daemon/plugin, fix team =
  plugin `mcp.json` http variant). `probeReachable` hardened (cancel del body
  SSE — il probe ora è hot path). `prewarmRuntime` resta spawn-only (il daemon
  è sempre warm; il tree refresh paga il primo contatto). README + walkthrough
  choose-backend (3 modalità) + CHANGELOG. Verifica: smoke-plugin 30/30 (4
  check nuovi: enum auto/default, Start Daemon contribuito+registrato, provider
  http shape, warning solo in spawn), ide-host test verdi (session 4/4,
  framing, resolve — machineria stdio intoccata), `node --check`, `vsce
  package` 1.0.12. Note: versione 1.0.12 (iterazione patch per build debug,
  `1.1.0` resta riservato alla chiusura F4-A — §3.5); il comportamento
  "outlives VS Code" del daemon (job del host) e il provider http su agent
  mode reale restano da verificare al **gate R7**.
- **F4-A2 atterrata (03/11, prereq `77be046` + `009e3da`)** — memory tree
  status filter (F4 §A.2, decisione A1 `current`): VSIX **1.0.13** con setting
  `ibis.memory.statusFilter` (`current` default | `all`, scope window) passato
  come `status` a `memory_list` (kind browse) e `memory_search` (search:
  empty→current era già default runtime, `all` usa il marker esplicito);
  comando **Ibis Assistant: Toggle Memory Status Filter** (bottone `$(filter)`
  nel view title, scrittura inspect-aware Global/Workspace/Folder così un
  override workspace non maschera il toggle) + listener config-change che
  ricarica conteggi (counts/renderedCounts azzerati — conteggi stale
  dell'altro filtro etichetterebbero male le root) e badge; il view title
  mostra `all` (TreeView.description) mentre il browse non filtrato è attivo;
  entry description conserva il badge status per le righe non-current in
  `all` mode. Log con `status=` per evidenza. **Prereq server atterrato come
  commit separato (`77be046`)**: il "explicit empty/all marker" del design
  non esisteva lato runtime — `memory_search` defaultava empty→current e
  exact-matchava ogni altro valore (un marker `all` avrebbe restituito 0
  righe silenziosamente); `memory_list`/`memory_search` ora accettano
  `status: "all"` (case/whitespace-tolerant, helper `memory_status_is_all` in
  ibis-search, predicato status droppato) + descrizioni registry aggiornate +
  fixture discovery rigenerata (D15 lockstep, generatore in-process) + test
  SQL-shape per entrambi i tool. Split in due commit per rispettare lo scope
  (`feat(server)` mai dentro `feat(vscode)` — nessun precedente). Verifica:
  smoke-plugin 33/33 (3 check nuovi), `node --check`, cargo test workspace
  (unico fallimento: finding pre-esistente ibis-ai/Ollama), clippy `-D
  warnings` verde, `vsce package` 1.0.13. Extra: walkthrough browse-memory
  aggiornato (e corretto il riferimento stale a `memory_search` come sorgente
  delle entries — il tree usa `memory_list` da `eea3f19`).
- **F4-A3 atterrata (04/11, `cc83203`)** — workstream browse server-side (F4
  §A.3): `memory_list` arg `workstream` (nome **o** id) — nome risolto via
  lookup bound case-insensitive (`string::lowercase(name OR '') = $ws_name`),
  id espliciti passati attraverso normalizzati (`format_record_id`) e
  table-checked; **nome sconosciuto → errore** (niente zero-righe silenziose
  per le root di browse). Filtro applicato congiuntamente a kind/status/
  visibility/limit via subquery sull'edge `part_of_workstream` (`in` =
  memory, `out` = workstream — direzione pinnata da unit test). Nuovo tool
  read-only `memory_workstreams` (no args → `{id, name, status,
  current_count}`): `current_count` = membri correnti visibili (stessa
  currentness SQL del browse `current` del tree + visibility filter), ordine
  per most-recently-touched = max member `updated_at` **attraverso l'edge**
  (i membri superseded tengono caldo il parent); righe senza membri in coda,
  tie-break per id. Modulo nuovo `ibis_search::memory::workstream`
  (`list_workstreams` + `resolve_workstream_id` condivisa con `list_memories`).
  **DDL**: la tabella `workstream` era nata SCHEMAFULL senza campi (Axon-like
  "future lanes declare their fields") — F4-A3 è la lane e dichiara i campi
  browse (`name`/`status`/`created_at`/`updated_at`, `IF NOT EXISTS`
  additivo; senza, qualsiasi writer futuro non potrebbe scrivere e il probe
  live fallisce). **Finding SurrealQL (verificato live su 3.3.5)**: il
  `id IN (SELECT in FROM part_of_workstream WHERE out = …)` del design NON
  matcha — la subquery proietta oggetti `{in: …}`, non valori; serve
  `SELECT VALUE in` (l'edge read per-riga `SELECT in, out FROM …` invece
  funziona: la decodifica object-row gestisce `in`/`out`). Superficie 57→58:
  contract test (`tool_contracts_all_58_names_present`, read-only set
  23→24, no-args shape), discovery snapshot + tools/list gates (incluso il
  43-tool core di `today_e2e_test`), fixture rigenerata in-process (D15
  lockstep). smoke-mcp `EXPECTED_READONLY` aggiornato — recupera anche il
  gap stale: `memory_list` read-only da `6447058` non era mai entrato nella
  lista (il check sarebbe fallito al prossimo run). Verifica: cargo test
  workspace (unico fallimento: finding pre-esistente ibis-ai/Ollama), clippy
  `-D warnings`, smoke-mcp 23/23 (43 tool core, 17 read-only) e **probe
  live** su sidecar reale seminato via CLI (10/10): store vuoto → `[]`,
  current_count esclusi i superseded, ordine per touch, nome case-
  insensitive, id esplicito, errore su nome sconosciuto.
- **F4-A4 atterrata (04/11, `822e007`)** — Workstreams view VSIX + inline
  curation menu + refresh on focus (F4 §A.3/§A.4/§A.5): seconda TreeView
  `ibis.workstreamExplorer` nello stesso container activity-bar (due viste,
  un'icona) — roots da `memory_workstreams` (name + `current_count` in
  description `N current`, ordine server-side per touch rispettato),
  children da `memory_list({workstream: <record id>})` con lo stesso status
  filter del kind browse (l'id esplicito salta il lookup nome server-side),
  stessi entry rows (icona kind, badge status/visibility/tags), stesso
  detail editor (`ibis.memory.openDetail`) e stesso degradation pattern
  (`error: …` + log). Il view title mostra `all` mentre il browse non
  filtrato è attivo (parity con il Memory tree, §A.2). **Curation menu
  (A.4)** su entrambe le viste via `view/item/context` con `viewItem =~
  /^ibisMemoryEntry/`: Share → `memory_share {id, visibility: "team"}`
  (nessuna conferma — il runtime promuove solo a team), Archive →
  `memory_update {status: "archived"}` con conferma modale, Supersede… →
  `memory_add {supersedes, kind/visibility prefill, title+content via input
  box}` con conferma modale (route di governance per righe non scrivibili),
  Update Content… → `memory_update {content}` (re-embed) offerto solo su
  righe owner-matching: contextValue `ibisMemoryEntryUpdatable` calcolato a
  render time (`ownedByMe`: owner `unknown` = anonymous local, owner vuoto =
  righe git-claim **mai** updatable, altrimenti match con
  `ibis.identity.actor`/`IBIS_ACTOR` — l'identità ufficiale arriva con A5);
  governance error sull'update → toast con azione "Create a superseding
  memory instead" che apre il flusso Supersede. Ogni azione: toast compact +
  riga full nel canale Output + refresh di entrambe le viste. **Residual
  CHIUSO (04/11, `bfa86ea` + `7437c61`)**: `memory_list` proietta ora il
  marker `store` (additivo, `Memory` lo aveva già, nessun cambio di
  contratto) e l'entryNode mappa `store === "git"` su contextValue
  `ibisMemoryEntryGit` — escluso dai when-clause curation
  (`viewItem =~ /^ibisMemoryEntry(Updatable)?$/`), badge `git` in
  description e tooltip "repo is source of truth — supersede via
  `.ibis/claims/` commit"; smoke-plugin 43/43. **Refresh on focus (A.5)**: `onDidChangeTreeView
  Visibility` su entrambe le viste, solo quando diventano visibili — il
  Memory tree refetcha i kind badge (`refreshCounts`, 8× `memory_list`
  embedding-free) e firea element-scoped i kind roots **identity-stabili**
  (mappa `kindNodes`: il fire per-elemento richiede l'istanza esatta che il
  tree possiede); expanded parents ri-fetchano, collassati solo re-render;
  search roots MAI fireati (il costo embedding si paga solo con una
  rerun esplicita — smoke pin: `"memory_search"` quotato appare una sola
  volta nel file). Workstream view: full refresh (tutto metadata). VSIX
  **1.0.14** (iterazione patch per build debug, `1.1.0` riservato alla
  chiusura F4-A — §3.5); payload curation sul TreeItem
  (`item.ibisMemory`, l'argomento di `view/item/context` è il TreeItem di
  `getTreeItem`). Verifica: smoke-plugin 42/42 (9 check F4-A4 nuovi: vista
  nel container, roots/children shape, 4 comandi curation contribuiti+
  registrati+menù, guard updatable, focus-refresh senza search, refresh
  workstream nel title), `node --check`, `vsce package` 1.0.14. Nessuna
  modifica runtime: la superficie 58 di F4-A3 è già sufficiente.
- **F4-A5 atterrata (05/11, `6d24e72`) — Milestone A completa** — actor
  identity header + claims mode knob (F4 §B.4/§B.7): setting
  `ibis.identity.actor` (string, default vuoto) + env `IBIS_ACTOR` con la
  stessa precedenza setting>env del guard A4 — l'helper `configuredActor`
  (memory-tree, ora esportato) è condiviso fra extension.js e il guard.
  `resolveBackend` attacca `actor` al backend http (attach, daemon e
  auto-attach), quindi ogni `callMcpTool` lo thread fino a `postJsonRpc`,
  che manda `X-Ibis-Actor` su tutte e tre le richieste (initialize incluso);
  senza actor nessun header (semantica anonymous local invariata).
  `resolveBackend(env)` di ide-host mappa `IBIS_ACTOR` sul backend http e
  `runHttpStdioBridge` lo inietta anche sul path bridge (`ibis-mcp-host` —
  il plugin http variant attribuisce); stdio invariato (l'argomento `actor`
  resta la via). Knob claims: setting `ibis.claims.personalMode` (enum
  `stub` default | `db_only`) forwardata in `buildSpawnEnv` come
  `IBIS_CLAIMS_PERSONAL_MODE` **solo quando il setting è esplicito** (mai il
  default — non deve clobberare un `config.json` `in_repo`); in attach mode
  l'env arriva al daemon tramite Start Daemon (`ibis start` eredita l'env
  nel run detached); un daemon avviato fuori dalla VSIX mantiene la sua
  policy di boot. Prereq runtime separato come da pattern F4-A2:
  `feat(server)` `24e799e` (override env su `memory.git_store.personal_claims`,
  db_only/stub/in_repo case-tolerant, junk warnato su stderr e ignorato).
  Evidenza: righe Status `Actor identity:` / `Claims personal mode:` e log
  verbose `backend: … actor=`. ide-host: nuovo `http_actor_test.mjs` (fake
  MCP http: header presente su initialize/initialized/tools-call, assente
  senza actor) dentro il job CI ide-host; resolve_test esteso
  (`IBIS_ACTOR` → `backend.actor`). VSIX **1.0.15** (iterazione patch per
  build debug, `1.1.0` resta riservato al publish — §3.5/§3.6). Verifica:
  smoke-plugin 47/47 (4 check nuovi), `node --check`, `vsce package`
  1.0.15, cargo workspace verde con `--no-fail-fast` (unico fallimento:
  finding pre-esistente ibis-ai/Ollama), clippy `-D warnings` verde. Docs:
  VSIX README (tabella Identity and claims) + CHANGELOG, team_server.md §4
  (client-side automation landed) + §5 + riga env, ide_host_and_lm_bridge.md.
- Milestone A (A1..A5) e Milestone B (B1..B4) atterrati. Il prossimo passo
  **eseguibile** è WQ-7; dopo il pin `v4.0.0` il **gate R7** (manuale owner —
  build immagine reale greenfield + scenario compose su LAN, 2+ client
  attaccati, attribution actor sulle righe condivise, nessun consolidamento
  duplicato, restart daemon pulisce i sidecar stale — checklist §8 del
  runbook team_server, aggiornata 05/11).
- **05/11 (`ec3cf74`):** R7 è sbloccato da WQ-7. L'immagine certificata è
  `v4.0.0`, non la 3.3.5 pinnata oggi da `deploy/Dockerfile`. Un pretest
  compose sulla 3.3.5 non chiude il gate.
- Dipendenza interna da rispettare: B.6 (reconciliation) è precondizione strutturale del Milestone B — **soddisfatta**.
- **R7 CHIUSO (05/11, chiusura formale 07/11)** — gate eseguito sull'immagine
  certificata `v4.0.0` con volume greenfield ricostruito (DB v4 seeded: 13
  righe `rpt-studio`/`ibis-assistant`): checklist §8 del runbook scripted
  **green 0–5** (banner `[TEAM]`, discovery online, attribuzione via bridge
  ide-host reale, un solo loop consolidamento, restart + reconciliazione
  ledger con store intatto, `load_session_context(rpt-studio)` isError=false
  col fix item 9 WQ-7) **+ pass interattivo owner su ENTRAMBI i client reali**
  (VSIX attach `fabio1`: browse, dettaglio, `memory_add` attribuito e
  verificato server-side; plugin http variant `fabio-plugin` via chat con
  argomento tool `actor` — attribuzione §4 dimostrata su entrambe le
  superfici, rows=13) **+ estensione LAN multi-host** (WSL mirrored: bind LAN
  diretto, write dal secondo host via IP LAN con `owner=fabio-lan2` verificato
  server-side, pitfall PowerShell session-header `string[]`→"System.String[]"
  documentato nell'evidenza). Tre fix VSIX usciti dal pass: **1.0.17** (guard
  `onDidChangeTreeViewVisibility` — i build portabili senza l'API abortivano
  l'activate prima del provider `ibis-memory`), **1.0.18** (Remember manda
  `project_name`, richiesto da add.rs), **1.0.19** (Remember/working-state
  mandano anche l'argomento tool `actor` — l'header è ignorato da peer
  non-loopback, team_server §4). Evidenza: `.r7-pretest/r7-gate-checklist.out.txt`
  (dir locale gitignored). Prossimo passo dello stream: **F3 docs** (atterrata
  07/11) → Track D (WQ-4).

### WQ-4 — Track D
- Disallineamento README/analysis.mjs (pass@k annunciato vs pass-rate implementato) e placeholder profili: **stato atteso**, non difetto — si chiudono a D2 (runner+analysis) e D1 (pin/seed su istanza live). Registrato anche come correzione nell'analisi L2 §4.1.
- **05/11:** D1 pinna l'immagine compose `v4.0.0` uscita da WQ-7. D3 confronta l'agente con la baseline A1 di WQ-7, non con MRR 0.917 / r@5 0.958 della 3.3.5.

### WQ-5 — ddofds
- **Fase 0 (A1) completata 02/11/2026, committata 03/11 (`9fb53da`)**:
  `eval.rs` in ibis-optimization (dataset YAML validato, `RetrievalEvalRunner`
  con mrr / recall@5 / recall@15 / per-channel, estate golden auto-seminata
  con id espliciti, `OfflineEmbedder` deterministico), CLI in-process
  `ibis eval retrieval` (niente nuovo tool MCP: registry resta a 57), dataset
  `evaluations/retrieval/dataset.yaml` (12 case), job CI `retrieval-eval`
  (SurrealDB pinnato v3.3.5 + embedder offline, soglia 0.75). Baseline offline:
  MRR 0.917, recall@5 0.958, recall@15 1.0 — 12/12 ok; baseline ollama
  (additive, 03/11): MRR 0.917, recall@5 1.0, recall@15 1.0.
- **Fase 1 (A2 RRF) implementata e validata 03/11/2026**: `rrf_fuse` (k=60,
  somma cross-channel) + `FusionMode` + config `search.fusion` (default
  additive) + "is current" come epsilon post-fusione + tie-break per id +
  eval `--fusion` con campo `fusion` nel report. Validazione
  ([evidenza](./2026-11-03-a2-rrf-validation.md)): parità offline, regressione
  su ollama (MRR 0.875 / r@5 0.958; r05 1→2, r11 secondo hit 5→9 — perdita
  di margine cosine, tradeoff noto). Default NON girato: **decisione owner**
  (opzione raccomandata: default additive, procedere con C9/D13/B6).
- Fasi 0–3 specificate in L3 con criteri di accettazione per intervento;
  **decisione owner su A2 (03/11): opzione 2 — tuning RRF prima di decidere**:
  default resta `additive` (rollback a config), il tuning (k più piccolo,
  ibride rank+cosine normalizzate — fuori scope A2 come specificato, candidati
  a chiudere il gap r05/r11) rivaluta su eval offline+ollama e solo poi si
  decide il default. C9+D13+B6 restano la fase 1 dopo il verdetto RRF.
- **05/11 (WQ-7, §3.8):** il retune A2 e B5 sono assorbiti dall'upgrade
  greenfield. Non si rituna RRF sulla baseline 3.3.5: DiskANN, le distanze
  a ultime cifre diverse e il canale full-text cambiano i ranghi. Il default
  di `search.fusion` esce da WQ-7. Il prossimo passo di questo stream è
  **C9**, sul lookup `ORDER BY vector::distance::cosine` più il canale
  lessicale. A3 consuma la finestra candidati di WQ-7. A4 resta due
  ricerche, ma su DiskANN. C7 estende i `DEFINE TYPE` e non ridisegna gli
  id. C8 legge le liste materializzate, `calls` e `changed` compresi.
  D10–D13 non toccano l'engine. B6, se arriva, memorizza il tipo uscito
  dal probe `f16`/`f32`.
- **C9 atterrata (07/11)** — soglia + pre-match deterministico nel duplicate
  lookup (`find_duplicate_memories` + `consolidate_loop`): knob
  `ai.consolidation.dup_similarity_min` (default 0.80, clamp [0,1] — il
  namespace `optimizer.*` previsto da L3 non esiste nel config di prodotto,
  il knob vive accanto a enabled/interval_secs) scarta post-query i
  candidati sotto soglia (righe senza embedding: score 0.0 → scartate;
  0 candidati post-soglia → distill prosegue con `POSSIBLE DUPLICATES: []`);
  titolo osservazione = `title` se presente, altrimenti prima riga del
  content (convenzione first_line del claim store) normalizzato (lowercase,
  trim, separatori path → spazio, collapse spazi, strip punteggiatura finale
  di frase — necessaria per confrontare titolo vs prima riga); match
  identico con un candidato → merge diretto senza chiamata LLM, stesso flusso
  persist di `persist_consolidation`. Governance intoccata (personal/team
  via `$dup_owner`). Test nuovi: threshold + knob config, deterministic
  match (zero chiamate AI), fallback first-line, normalize unit;
  ibis-core/ibis-optimization/ibis-server verdi, clippy `-D warnings` verde.
- **D13+B6 atterrati (08/11)** — fase 1 completa (A2+C9+D13+B6 chiusi).
  D13 (prompt stability per cloud caching): il blocco istruzioni di ogni
  call-site cloud è un `const` byte-stabile snapshot-testato nel proprio
  crate (`ANALYZE_INSTRUCTIONS` + `build_analyze_prompt` e
  `DISCOVER_FORMAT_INSTRUCTIONS` e `build_ast_rule_prompt` +
  `AST_RULE_INSTRUCTIONS` in ibis-ingest-logs, `INGEST_SYNTH_INSTRUCTIONS`
  in ibis-ingest-code, `DISTILL_INSTRUCTIONS` + `build_distill_prompt` in
  ibis-optimization) — istruzioni → contesto variabile → query ultima,
  niente timestamp/request-id nel prefisso; il fingerprint estensione
  (rule_synth) e la proposta linguaggio (ast_rule_gen) sono fuori dal
  prefisso per costruzione. Billing: `UsageMetadata.cached_content_token_count`
  (Gemini `cachedContentTokenCount`, OpenAI-compat
  `prompt_tokens_details.cached_tokens` — i provider openai_compat/claude
  ora parsano anche usage, prima lo scartavano —, Claude
  `cache_read_input_tokens`), `CostTracker.record_usage(…, cached_tokens)`
  con billed = prompt − 0.9×cached saturante (record `tokens_used`
  accumula billed, debug log del risparmio, blended $0.15/1M invariato).
  B6 (cache embedding persistente): tabella `embedding_cache` SCHEMAFULL
  con UNIQUE su `key` = `{model}:{dim}:{sha256(text)}`, `array<float>`
  (no ANN), seam `ibis_ai::EmbeddingCacheStore` con adapter
  `DbEmbeddingCacheStore` (record id deterministico = sha256(key), UPSERT
  idempotente, best-effort), `OllamaProvider::with_disk_cache` (L2 prima
  del batch HTTP — hit mai sul FIFO, write-back post-fetch, mismatch dim
  = miss, stragglers post-wholesale-clear passano per L2), config
  `ai.embedding.disk_cache` (default **false**) + `disk_cache_ttl_secs`
  (30d, prune al boot); const `ibis_db::schema::EMBEDDING_DIMENSION` (768)
  con test di sync contro i 5 DDL DiskANN. **Live-probe SurrealDB 4.0.0**:
  `DELETE … RETURN AFTER` rende `[[NONE]]` (righe assenti post-op) — il
  prune usa `RETURN BEFORE` che ripristina la lista eliminata (test live
  2/2: roundtrip+miss+mismatch+prune, UNIQUE backstop). Caveat: il prune
  `file_at_commit` (hotcache) conta ancora su `RETURN AFTER` — su 4.0 il
  conteggio loggato sarà 0 (prune comunque operativa), da riallineare al
  primo tocco di ibis-search. Test: ibis-ai 136/136 (4 seam + key),
  schema 2/2, live surreal 11/11 (DDL greenfield idempotente), ibis-core
  51/51, ibis-server lib 61/61, live embedding cache 2/2.

- **A4 atterrata (08/11)** — query expansion deterministica (fase 2, parte 1).
  `expand_query` in `ibis-search/src/retrieval_boost.rs`: `[originale]` o
  `[originale, variante]`, variante = query + token CamelCase riuniti in
  forma identificatore (lowercase) + nomi dei simboli linked appesi (dedup
  case-insensitive, cap `MAX_EXPANSION_SYMBOLS` = 20). Variante assente =
  fast path (nessun secondo embedding) quando `resolve_linked_symbols`
  rende vuoto (anche sul fallimento best-effort). `QueryExpansionMode`
  (`off`|`symbol_variant`) + config `search.query_expansion` (default
  **symbol_variant**; valore ignoto → default + warn), wiring su
  `SearchService` (main/state/eval). `ask_project`: secondo `embed_text`
  registrato su `system:search_vector`; i canali vettoriali girano due
  pass con `$vec` diverso e merge per riga `score = max(passi)` (una entry
  per id; righe solo-variante entrano nel canale), i canali lessicali
  restano sulla query letterale; fallimento dell'embedding variante →
  fail-open sul pass primario (warn). Eval: `--query-expansion` + campo
  `query_expansion` nel report; golden estate semina una riga `symbol`
  (`PriceCalculator`) e il dataset guadagna `r21_symbol_variant` (21 case).
  **Evidenza live (08/11, SurrealDB 4.0.0 locale, offline e ollama)**:
  off vs on parità — offline MRR 0.952 / r@5 0.976, ollama MRR 0.952 /
  r@5 1.0, 21/21 ok; `r21` è rank 1 in entrambe le modalità (il golden set
  non contiene un mismatch che saturi diversamente). Prova che la variante
  spara: run a case singolo su DB fresco, `system:search_vector` = 12
  token (off) vs 30 (on) = primario 12 + variante 18 ("pricecalculator"
  appeso). Test: ibis-search 78/78 (merge max, doppio pass con FakeAI,
  fast path senza simboli, off, fail-open, cap/dedup), ibis-core 52/52
  (config knob), ibis-optimization 23/23 (report + seed symbol), workspace
  lib verde, clippy `-D warnings` verde.
  **Follow-up A4 atterrato (08/11)** — (1) linking simboli fuzzy:
  `resolve_linked_symbols` fa un pass exact (come prima) e poi un pass
  fuzzy best-effort (`string::lowercase(name) CONTAINS $token`, bind
  F-002, cap 20 token); il filtro Rust deterministico accetta un simbolo
  con copertura >= 2 token della query oppure un singolo token di >= 8
  caratteri (un hit singolo corto resta fuori), ordina per copertura,
  lunghezza matchata, nome più corto, nome; coda fuzzy cap 5; lista
  merged cap 20 (vale sia per l'entity-link boost sia per l'espansione).
  Così "price calculator" linka `PriceCalculatorService`. (2)
  `ask_project_at_ref` eredita l'espansione sui canali semantici
  (claims, commit_chunk, reasoning, code): un pass per embedding con
  `score = max(passi)` per riga (il canale code usa
  `as_of_code_semantic_multi`, una sola risoluzione di tree/ref); il
  branch exact-path gira una volta e resta letterale, come da edge case
  L3. Embedding primario + variante passano per l'helper condiviso
  `SearchService::query_embeddings` (stesso costo/fail-open di
  `ask_project`). Evidenza: `r21` ora usa la dicitura mismatch ("price
  calculator tier rounding rules", simbolo seed `PriceCalculatorService`);
  singolo ask su DB fresco = 13 token (off) vs 33 (on) = primario 13 +
  variante 20 con `pricecalculatorservice` appeso; eval offline/ollama
  parità off/on (MRR 0.952, r@5 0.976/1.0, 21/21). Test: ibis-search
  86/86 (fuzzy score/ranking/merge, ask_ref doppio pass + off, fail-open
  invariato), live_memory_estate 12/12, workspace lib verde, clippy
  `-D warnings` verde.
- **A3 atterrata (08/11)** — reranker post-fusione (fase 2, parte 2).
  Nuovo modulo `crates/ibis-search/src/rerank.rs` + knob `search.rerank`
  (`off` default | `local`): dopo il boost, i top
  `min(board, 30, search.candidates)` hit del board fuso passano a UN
  scoring call locale-first (hint `bulk_local`, temp 0.0, max 512 token
  out, estratti ~300 rune con whitespace collassato; istruzioni
  byte-stabili → contesto → query ultima, convenzione D13). Output atteso
  `[{index, score 0..1}]` con parse tollerante: fence markdown, JSON
  troncato salvato per-oggetto, indici fuori range scartati, duplicati
  first-wins, score clampati. Riordino del subset con blend
  `base_norm*0.4 + llm*0.6` (base normalizzata sul max del subset; gli hit
  non scorati mantengono il rango pre-rerank restando sulla stessa scala a
  `base_norm`), tie-break per id; le righe oltre il subset restano dopo.
  Fail-open: subset ≤5 hit, provider non funzionale, errore AI (incluso
  `ContextOnly`) o risposta non parsabile → warn + ordine fuso invariato.
  Costo su `system:search_rerank`; `ask_project_at_ref` NON reranka in v1
  (floor claim_file). Wiring main/state/eval. Eval:
  `--rerank off|local` + `--rerank-model` (default `qwen2.5-coder:7b`) +
  campo `rerank` nel report; con embedder ollama il giudice è il chat
  locale dietro un composito embed+chat nel seam dell'eval. **Misure live
  (08/11, SurrealDB 4.0.0, golden 21 case):** offline off/local parità
  0.952/0.976/1.0 (21/21, rank per-case identici — fail-open verificato);
  ollama con giudice `granite4.1:3b` (qwen2.5-coder:7b non presente
  sull'host) = MRR 0.929 / r@5 0.976 / r@15 1.0 (21/21) contro
  0.952/1.0/1.0 off: un caso (`r19_superseded_excluded`) scende rank 1→2,
  nessun guadagno sul dataset già saturo → default `off` confermato
  (attivazione guidata dai numeri; un giudice tier-M vero e un dataset con
  margine restano il lever). Test: ibis-search 107/107 (18 unit rerank:
  parse/clamp/dedup/fence/troncamento, blend, reorder + unscored, window
  cap, skip ≤5, non-funzionale, errore, non-parsabile, costo; 3
  integrazione ask: reorder, fail-open, off senza call), ibis-core 53/53
  (knob), ibis-server 61/61   (flag eval); clippy `-D warnings` verde.
- **C7 atterrata (08/11)** — ontologia tipizzata `details` per la memoria
  (fase 2, parte 3). Schema: `DEFINE FIELD IF NOT EXISTS details ON TABLE
  memory TYPE option<object> FLEXIBLE` (il resto della tabella resta
  SCHEMAFULL; nessuna migrazione, campo opzionale). Nuovo modulo
  `crates/ibis-search/src/memory/details.rs`: rubric per-kind (chiave
  minima obbligatoria: decision→`scope`, fix→`root_cause`,
  constraint→`applies_to`, rule→`enforcement`; le altre —
  `alternatives[]`/`tradeoffs`, `symptom`/`remedy`, `until` — restano
  opzionali), flatten a path dotted con valori bindati (chiavi
  identifier-vetted `[A-Za-z0-9_]+`, array solo scalari, null = clear in
  update), policy `memory.schema_enforcement` (`warn` default |
  `strict`, valore ignoto → warn) su `SearchService` e `Optimizer`.
  `memory_add`/`memory_update` accettano `details` (JSON object, merge in
  update); `supersedes` ora richiede lo stesso kind (il check store
  git-owned resta precedente per messaggio). Consolidation: il prompt
  distill chiede le chiavi rubric (istruzioni D13 aggiornate + snapshot),
  `persist_consolidation` salva i details, strict → retry UNA volta della
  distillazione poi failure item-level (loop sano). `memory_search` filtra
  per uguaglianza scalare su `details.<key>`; `memory_list`/
  `memory_search`/`memory_export` proiettano `details` come JSON; import
  li riporta. Tool surface invariata (58): parametro `details` su
  `memory_add`/`memory_update`/`memory_search` + fixture discovery
  rigenerata (D15 lockstep). **Probe live (08/11, SurrealDB 4.0.0,
  scratch DB):** roundtrip details attraverso il campo FLEXIBLE (expand)
  e search filtrata che restituisce solo la riga con `scope=billing`;
  live memory estate **13/13**. Test: ibis-search 121/121 (8 unit details,
  4 add, 3 memory lane, live C7), ibis-core 54/54 (knob), ibis-optimization
  25/25 (parse/prompt/retry strict/persist), ibis-server 61/61 + test
  suite completa (fixture discovery inclusa); clippy `-D warnings` verde,
  workspace lib verde.

- **D10 atterrata (08/11)** — model routing per-task sui tier S/M/L/XL
  (fase 3, parte 1). `ReasoningRouter` accetta gli hint `local:S|M|L|XL`
  (case-insensitive) e `local_only:<tier>`: i tier hint mantengono il
  contratto local-first → cloud (su CPU-only con `use_cloud_when_no_gpu`
  il cloud resta preferito), `local_only:<tier>` non esce mai dalla
  macchina. Mappa tier→provider costruita in `state.rs` da
  `ai.reasoning.model_overrides` via `ibis_settings::resolve_tier_models`
  (tier uguale al modello locale di default omesso → fallback al provider
  default; `model` esplicito o `always_smallest_local` pinnano la lane
  locale a un solo modello; provider Ollama costruiti come handle, il
  modello si scarica solo al primo uso del tier). Mapping call-site:
  insights, enrich e RAFT QA → `local:S`; consolidation (personale:
  `local_only:M`), analyzer/discover_format, rule_synth e ast_rule_gen →
  `local:M`; agentic ask → `local:L` (un provider pinnato per l'intero
  loop ReAct, nessuno switch intra-task). Log: mappa assemblata a startup
  + `ibis_ai::router` debug `hint`/`tier`/`local_model` per chiamata. Test:
  ibis-ai 141/141 (5 casi tier: selezione, fallback default/cloud,
  `local_only`, CPU prefer-cloud, suffisso ignoto), ibis-settings 23/23
  (`resolve_tier_models`), contract per-crate sui 9 call-site,
  ibis-optimization 27/27, ibis-search 123/123, ibis-ingest-logs 65/65,
  ibis-ingest-code 45/45, ibis-server 61/61; clippy `-D warnings` verde.

- **D11 atterrata (08/11)** — bounded decisions + shadow mode (fase 3,
  parte 2). Nuovo modulo `crates/ibis-ai/src/decide.rs`: `DecideQuestion`
  (`Noul` | `Choice(Vec<(id, descrizione)>)`) + `decide(state, questions)`
  single-turn con hint `local:S` e output vincolato
  `{"<id>":{"probs":{"<opzione>":p}}}`; il parser è stretto (JSON invalido,
  opzioni mancanti/ignote, probabilità fuori [0,1] o somma ≠1 → astensione,
  nessun best-effort) e la confidence è il margine `max − second`. La policy
  di astensione resta nel chiamante (costanti `DEFAULT_AUTO_THRESHOLD` 0.75
  e `DEFAULT_ESCALATE_THRESHOLD` 0.40 esportate, non applicate dentro
  `decide`). `decide_with_hint` serve i chiamanti privacy-bound
  (`local_only:S`); il prompt segue D13 (istruzioni byte-stabili → stato →
  domande ultime) e usa temp 0.001 perché il provider layer tratta 0.0 come
  "unset" (0.0 sarebbe il default del modello, spesso 0.8). Shadow mode:
  `ai.decide.shadow` default true; il consolidation calcola anche
  `is_noise` (stato = osservazione + finestra duplicati, stesso builder del
  distill) e logga `decide_shadow` con `decision_llm`, `best`, `probs`,
  `confidence`, `agreement`, `rubric`; comportamento invariato, shadow
  `local_only:S` anche per il team (misura il modello S, mai cloud),
  pre-match C9 resta zero-AI, errore/modello S assente →
  shadow disabilitato per il pass con un warn. Dataset
  `evaluations/decisions/cases.yaml` (40 case: 20 noise, 20 durable; rubric
  `is_noise.v1` pinnata) + `ibis eval decisions` (provider
  ollama|openai_compat, `--min-agreement`, `--auto-threshold`,
  `--escalate-threshold`, `--out`; report con agreement, confidence media e
  bucket auto/escalate/default). Misura live 08/11 (qwen3.8-27b via
  openai_compat; tier-S `granite4.1:3b` non presente sull'host): 40/40 ok,
  agreement **0.975**, mean confidence 0.865, auto 35/40 a θ 0.75 con
  accuracy 1.000, 4 escalated, 1 default (unico miss in default bucket,
  conf 0.20; run precedente 1.000). Test: ibis-ai 162/162 (20 decide:
  parser/margine/fence/Noul/hint/temp/prompt-order), ibis-core 55/55
  (`ai.decide.shadow`), ibis-optimization 41/41 (rubric/state/shadow
  invariato+local-only+log+errore, dataset+report), ibis-server 63/63 (parse
  retrieval+decisions); clippy `-D warnings` verde.

### WQ-6 — Track B
- Nessun sub-fase iniziata: piano integrato in [PLAN.md §9-bis](./PLAN.md)
  (approvato 03/11, sessione analisi Agent Beacon vs Microsoft Scope).
  Script-first (B0/B1/B2/B4), promozione a lane nativa `ibis-ingest-traces` (B3)
  solo dopo ≥3 iterazioni utili; gate **REVIEW R8**. Prerequisito owner:
  azione manuale ⑥ (endpoint Beacon MSI, user-mode).
- **05/11:** B0 non aspetta WQ-7 (è un'azione owner). Quando B3 viene
  promossa a lane, lo schema nasce sulle convenzioni WQ-7: id array dove
  la chiave è composta, DiskANN, full-text, `DEFINE TYPE`, archi
  materializzati. Nessuno schema 3.x di transizione.