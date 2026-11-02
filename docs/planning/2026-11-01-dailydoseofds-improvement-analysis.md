# Analisi interventi Daily Dose of DS → Ibis Assistant

**Data:** 2026-11-01
**Origine:** analisi degli articoli di [dailydoseofds.com](https://www.dailydoseofds.com/) mappati sullo stato attuale del codebase
**Scopo:** input per il successivo piano di implementazione passo-passo (fase 0–1 come primo milestone)

## Metodologia

- Archivio DDofDS esaminato (304 titoli), 7 articoli letti integralmente e mappati sul codebase:
  1. *[Hands-on] Agent memory is only as good as its schema* (Zep Graphiti: ontologia tipizzata, source/target constraints)
  2. *Why Your Agent Remembers Everything and Understands Nothing* (Zep Observations: pattern recognition deterministico)
  3. *Jev, Clearly Explained* (bounded decisions: choice/score/noul, confidence, astensione, shadow-mode)
  4. *[Hands-on] Make RAG systems 32x memory efficient* (binary quantization, 36M vettori <30ms)
  5. *KV vs Prefix vs Prompt vs Semantic Caching* (4 layer di cache, rischi del semantic cache)
  6. *How to Reduce LLM Costs by 50-60% Using Model Routing* (Plano/Coinbase: alias-based routing, cheapest-first, model affinity)
  7. *Loop Engineering, Clearly Explained* (freni del loop: budget, stuck detection, completion check, verifier separato)
  8. *Building Trustworthy Agentic/RAG Workflows* (trust score, validazione risposta)
- Stato esistente verificato con esplorazione mirata crate per crate (riferimenti `file:line` al momento dell'analisi).
- Formato per intervento: **Fonte → Stato esistente → Proposta → Pro → Contro → Dipendenze/Effort** (S <1g, M 2–4g, L >1 settimana).

---

## A. Qualità retrieval (crates/ibis-search)

### A1 — Eval harness retrieval con golden dataset (fondazionale)

**Fonte:** RAG course (eval), Jev (deployment procedure: misurare prima di automatizzare).

**Stato esistente:** solo self-eval RAFT (`crates/ibis-optimization/src/raft.rs:109-130`, reciprocal rank su chunk atteso, usato per rinforzare `usage_weight`) e benchmark agentico "Scope" in `evaluations/scope/` (pass@k sui task, non qualità retrieval). 786+ test di correttezza, zero di qualità.

**Proposta:** dataset golden YAML `query → expected chunk/file/memory id` costruito dalle fixture esistenti (`evaluations/scope/task-prompts/`) + runner `ibis eval retrieval --dataset X` con recall@k, MRR, nDCG, output JSON e exit code; job CI dedicato.

**Pro:**
- Prerequisito per misurare ogni intervento retrieval (A2–A4).
- Metà dell'infrastruttura esiste già (scoring RAFT).
- Costo di runtime nullo (no LLM).

**Contro:**
- Dataset iniziale da curare a mano.
- Rischio overfitting se troppo piccolo.
- Va versionato: la qualità dipende dall'ingest (cambio chunking = re-baseline).

**Dip:** nessuna. **Effort: M.** → *Eseguire per primo.*

### A2 — Reciprocal Rank Fusion al posto della fusione additiva

**Fonte:** RAG course (hybrid search), best practice standard dei motori ibridi.

**Stato esistente:** fusione additiva a pesi fissi `base 0.75 + keyword 0.15 + entity 0.10` (`retrieval_boost.rs:176-183`) sopra score per canale `cosine*0.7 + time_decay*0.3` (`search.rs:236-240`); i 4 canali (file_chunk/commit_chunk/memory/reasoning) hanno scale di punteggio non comparabili tra loro.

**Proposta:** RRF (k=60) sui rank dei 4 canali, poi i segnali attuali (keyword, entity-link, "is current") come boost additivo *sopra il rank RRF*; config `search.fusion = additive|rrf` per rollback.

**Pro:**
- Robusto alle scale eterogenee dei canali (problema reale oggi: cosine vs pseudo-BM25).
- Implementazione semplice e localissima.

**Contro:**
- RRF usa solo i rank (perde l'informazione di margine).
- Il boost "is current" (+0.05, `search.rs:345-357`) va ricalibrato.
- Guadagno da dimostrare con A1.

**Dip:** A1. **Effort: S–M.**

### A3 — Reranker esplicito post-fusione

**Fonte:** RAG course (reranking), Jev (use-case "retrieval relevance": embeddings trovano testo correlato, non ciò che risponde alla query).

**Stato esistente:** assente: dopo la fusione c'è solo graph enrichment (`search.rs:456-479`) e filtro memoria; nessuno stage di scoring semantico query↔candidato.

**Proposta:** stage opzionale (config) su top-30 fusi: scoring con modello locale tier M (`qwen2.5-coder:7b` via `bulk_local`) o, se disponibile, cross-encoder; output tipizzato (score 0–1) usato per riordinare i top-5/8 finali.

**Pro:**
- Guadagno tipico di precision@5 in RAG.
- Costo zero cloud (tier locale).
- Ben delimitato (una funzione in `ibis-search`).

**Contro:**
- Latenza +0.5–2s per `ask_project` con modelli locali.
- Qualità del rerank dipende dal modello locale (rischio skew verso matching lessicale).
- Va attivato solo se A1 mostra gap.

**Dip:** A1, D10 (routing tier M). **Effort: M.**

### A4 — Query expansion pre-embedding

**Fonte:** RAG course (query transformation), Corrective RAG.

**Stato esistente:** solo tokenizzazione CamelCase/entity-link locale (`retrieval_boost.rs:25, 64-100`); l'unica "seconda chance" è il loop ReAct con query `SEARCH:` successive (`agentic.rs:56-140`).

**Proposta:** espansione deterministica (varianti path, sinonimi dalla `symbol` table, entità collegate) sempre attiva + rewrite LLM opzionale (hint `bulk_local`, cache per query normalizzata) prima del primo vector search.

**Pro:**
- Recupera hit mancati dalla query letterale.
- La parte deterministica costa zero.

**Contro:**
- Rewrite LLM = latenza sul percorso critico.
- Rischio deriva (query troppo ampia diluisce la fusione).
- Interazione con A2 da misurare.

**Dip:** A1. **Effort: S (deterministica) / M (con rewrite).**

---

## B. Embeddings & storage

### B5 — Quantizzazione embedding

**Fonte:** *Make RAG 32x memory efficient* (binary quantization: usata da Perplexity, Azure, HubSpot).

**Stato esistente:** `array<f32>` 768-dim full-precision, HNSW COSINE su 5 tabelle (`crates/ibis-db/src/schema.rs:324-328`), nessuna compressione; vettori f32→f64 in query (`search.rs:498`).

**Proposta:** due opzioni:
- (a) colonna int8 ausiliaria + rescoring full-precision sui top-K in Rust;
- (b) Matryoshka truncation — `nomic-embed-text` supporta dimensioni variabili, quindi indice coarse a 256-dim + rescoring a 768.

Target: solo `file_chunk` (il volume dominante).

**Pro:**
- 4–32x meno RAM/IO per l'indice.
- Rilevante per repo molto grandi; latenza ANN migliore.

**Contro:**
- SurrealDB non espone distanza Hamming → la variante binaria richiede scoring custom in Rust (complesso).
- Il beneficio diventa percepibile solo oltre ~1M vettori.
- Migrazione schema + interazione con `batch_manager` (pending embeddings); HNSW SurrealDB non supporta indici multi-dimensione sulla stessa tabella senza duplicazione.

**Dip:** nessuna, ma posticipabile. **Effort: L.** → *Consigliato solo se il volume dati lo giustifica.*

### B6 — Cache embedding persistente

**Fonte:** stesso tema di efficienza (riutilizzo invece di ricalcolo).

**Stato esistente:** cache embedding solo in-process, cap 1024, svuotata wholesale (`crates/ibis-ai/src/ollama.rs:42,82`); ingest salta per hash (`ingest.rs:462-494`) ma un re-index manuale o un cambio di versione del file ri-embedda tutto il file.

**Proposta:** tabella `embedding_cache` persistita (chiave `hash(testo)+model+dim`), consultata sia da Ollama embed sia da `batch_manager`; config opt-in con cap disco (LRU).

**Pro:**
- Re-ingest/rollback istantanei e gratis.
- Coerente con local-first; semplice (un lookup in più nel path batch).

**Contro:**
- Crescita disco (LRU necessario).
- Le chiavi devono includere model+dim o diventano stale al cambio modello.
- Aggiunge una query DB nel path di embedding.

**Dip:** nessuna. **Effort: S–M.**

---

## C. Memoria (ibis-optimization, memory_*)

### C7 — Ontologia tipizzata per l'estrazione memoria

**Fonte:** *Agent memory is only as good as its schema* (Zep Graphiti: schema = reasoning boundary; source/target constraints sugli edge; descrizioni con esempi guidano l'estrazione).

**Stato esistente:** `kind` enum rigido ma campi liberi (`title`/`text`); consolidation distilla JSON free-form create/merge/noise (`memory_consolidate.rs:365-401`); edge `supersedes/derived_from/part_of_workstream` senza vincoli di tipo (`schema.rs:330-336`).

**Proposta:** schema per-kind con campi strutturati obbligatori (decision→{scope, alternatives, tradeoffs}, fix→{symptom, root_cause, remedy}, constraint→{applies_to, until}…) validati server-side; edge constraint (es. `supersedes` solo tra memory dello stesso kind; `derived_from` solo verso commit/log_file); rubric di estrazione con esempi nel prompt.

**Pro:**
- Retrieval filtrabile per attributo strutturato (oggi impossibile).
- Consolidation più deterministico e verificabile.
- Coerente con la governance già presente (owner-only, visibility).

**Contro:**
- Migrazione dei memory esistenti (accettabile: pre-1.0).
- Schema troppo rigido rischia di scartare dettagli utili.
- Superficie MCP `memory_add` cambia (breaking ammesso in pre-1.0 ma da comunicare).

**Dip:** nessuna. **Effort: M.**

### C8 — Pattern recognition "Observations" deterministico

**Fonte:** *Why Your Agent Remembers Everything and Understands Nothing* (Zep Observations: firme entity-pair+relazione → episode graph → connected components → LLM scrive solo il summary; read-only, rigenerato su evidenza).

**Stato esistente:** consolidation distilla osservazioni *individuali* oldest-first con dedup solo embedding (`memory_consolidate.rs:102-164`); `memory_insights` fa clustering ma con label LLM (`insights.rs:70`). Nessun pattern cross-entità: la domanda "cosa collega questi 3 fix?" è invisibile al vector search.

**Proposta:** pipeline 2 stadi sul grafo esistente:
1. clustering deterministico per firme (coppie entità: file↔symbol, commit↔issue, fix↔root-cause) → connected components;
2. LLM (hint `bulk_local`) produce name+summary del cluster, vincolato all'evidenza.

Persistito come `kind=observation` read-only con link `evidence→` ai ricordi sorgente; rigenerato quando l'evidenza cambia (hook già esistenti: `refresh_stale_memories`, `promote_dirty_anchors`).

**Pro:**
- Colma il gap multi-hop esattamente dove il prodotto è forte (il grafo c'è già).
- Clustering deterministico evita i cluster tematici troppo larghi tipici dell'embedding-clustering.
- Read-only = governance coerente.

**Contro:**
- Definire la "firma" su un grafo di code (non conversazioni) richiede design dedicato.
- Costo LLM per cluster; rischio osservazioni ridondanti → serve dedup e refresh.

**Dip:** C7 (le firme usano entità tipizzate), D10. **Effort: L.**

### C9 — Entity resolution nel consolidation

**Fonte:** Zep Graphiti (entity resolution come step esplicito: "Nexus" e "the Nexus project" diventano un nodo).

**Stato esistente:** duplicate lookup solo via embedding similarity nel consolidation; nessuna normalizzazione deterministica.

**Proposta:** normalizzazione pre-match (path normalizzati, symbol case-insensitive, ID ticket canonici); merge deterministico senza LLM quando la chiave è identica, LLM solo per i casi ambigui.

**Pro:**
- Meno merge sbagliati e meno chiamate LLM.
- Banale da implementare sopra il codice attuale.

**Contro:**
- Copre solo i casi evidenti, non gli alias semantici.

**Dip:** nessuna. **Effort: S.**

---

## D. LLM usage & costi (ibis-ai)

### D10 — Model routing per-task sui tier S/M/L/XL

**Fonte:** *How to Reduce LLM Costs by 50-60% Using Model Routing* (Coinbase/Plano: alias-based routing + selection_policy cheapest + model affinity; cache hit 5%→60%, spend quasi dimezzata).

**Stato esistente:** `ReasoningRouter` ibrido con hint `quality/auto/local_only/bulk/bulk_local` (`crates/ibis-ai/src/router.rs:45-146`); tier S/M/L/XL **dichiarati ma non selezionati per task** (`config.rs:16-19,169-184`); i call-site non dichiarano la propria complessità.

**Proposta:**
- (a) mapping esplicito task→tier nel config: insights labels→S, consolidation/analyzer/rule_synth→M, agentic ask→L/XL (alias-based, come Plano);
- (b) `selection_policy: cheapest` quando due modelli qualificano;
- (c) **model affinity** per sessione ask: pin del modello per la durata del loop ReAct (coerenza intra-task e riutilizzo cache).

**Pro:**
- Taglio costi tipico 50–60% sul cloud.
- Config-driven senza riscrivere i call-site.
- Affinity evita switch di modello a metà task.

**Contro:**
- Rischio qualità se il tier è troppo basso (mitigare con fallback `quality` + validazione via A1/F17).
- Mapping da mantenere quando si aggiungono call-site.
- Affinity richiede un session-id nel path router.

**Dip:** nessuna; sfrutta A1 per validare. **Effort: M.**

### D11 — Bounded decisions (pattern Jev) al posto dell'LLM generativo

**Fonte:** *Jev, Clearly Explained* (state + domande tipizzate choice/score/noul → distribuzione di probabilità + confidence; soglie di astensione in codice; shadow-mode prima di automatizzare).

**Stato esistente:** decisioni chiuse e ripetute oggi fatte con LLM generativo JSON: "questa osservazione è noise?" (consolidation), "questo log è già noto?" (analyzer), più guardrail statici a cooldown (`rule_synth.rs:54-57`).

**Proposta:** primitiva `decide(state, rubric)` nel tier S locale: risposta tipizzata con probabilità per opzione; branch in codice (auto se confidence>θ, escalate altrimenti); rubric versionate nel repo; rollout in shadow-mode (log doppio) prima di attivare.

**Pro:**
- Millisecondi vs secondi, costo ~zero.
- Confidence esplicita → escalation pulita.
- Elimina parsing JSON brittle.
- Gli use-case (noise del consolidation, dedup log, risk classification) sono esattamente i "bounded decisions" del pattern.

**Contro:**
- Non esiste Jev integrabile in Rust → implementazione custom (classificazione via modello S con output vincolato).
- Serve calibrazione su dati reali (accuracy vs confidence).
- Inadatto a decisioni con reasoning multi-step (quelle restano all'LLM).

**Dip:** D10 (tier S), A1/F17 per la fase shadow. **Effort: M.**

### D12 — Cache risposte LLM a gradini

**Fonte:** *KV vs Prefix vs Prompt vs Semantic caching* (exact-match a rischio zero; semantic cache restituisce risposte sbagliate con 200; soglia = proprietà del traffico, non un valore da copiare).

**Stato esistente:** nessuna cache di risposta; solo embedding cache e hot cache git.

**Proposta:** tre livelli:
- (a) exact-match byte-identico (zero rischio) su analyzer log per fingerprint e rule-synthesis per (repo, ext, pattern); misurare prima il repeat-rate;
- (b) semantic cache **solo** su task deterministici (analisi log, sintesi regole), soglia conservativa ≥0.95, invalidazione su cambio indice/rules;
- (c) **mai** su `ask_project`.

**Pro:**
- (a) gratis e banale.
- (b) Taglia i costi sui task ripetitivi dove il costo di un falso hit è basso.

**Contro:**
- Il semantico è il layer più rischioso (trap della negazione: soglie vicine per coppie opposte).
- Invalidazione cross-layer non banale (l'indice cambia → le analisi log cached diventano stantie).

**Dip:** nessuna. **Effort: S (a) / M (b).**

### D13 — Prompt stability per prompt caching cloud

**Fonte:** stesso articolo (contenuto stabile prima, variabile dopo; marker sul confine; log di cache_read per verificare; il read costa 0.1x).

**Stato esistente:** prompt assemblati ad-hoc nei call-site; nessuna convenzione sull'ordine; provider cloud usati senza esploitare `cache_control`/cached tokens.

**Proposta:** convenzione di assembly: system+istruzioni (stabili) → contesto → query (ultima); marcare il blocco stabile per Anthropic/OpenAI; aggiungere `cache_read_tokens` al `CostTracker` per verificare i salvaggi.

**Pro:**
- Read a 0.1x del prezzo input; banale da implementare.
- Si applica subito ai call-site cloud a prompt lungo (rule_synth, consolidation).

**Contro:**
- Beneficio parziale (i contesti variabili dominano alcuni prompt).
- Richiede disciplina su tool schemas e ordine per non invalidare il prefisso.

**Dip:** nessuna. **Effort: S.**

---

## E. Loop agentic & affidabilità

### E14 — Loop engineering per il ReAct di `ask_project`

**Fonte:** *Loop Engineering, Clearly Explained* (max-iteration + budget + no-progress detection + completion check; "ending a turn is not finishing the job"; context rot / doom loop).

**Stato esistente:** loop ReAct con solo `max 5 step` (`crates/ibis-search/src/agentic.rs:56-140`), temp 0.2, max 2000 tok; nessun budget esplicito, nessuno stuck detection, nessuna verifica finale: il modello giudica da solo quando ha finito.

**Proposta:** freni espliciti:
- budget token/costo per ask (esiste già `DbCostStore`);
- no-progress detection (stessa query con stessi argomenti → break);
- completion check objective: coverage evidenza (ogni claim tracciato a ≥1 chunk) prima di emettere la risposta;
- error message actionable per il turno successivo.

**Pro:**
- Il completion check sostituisce il self-assessment del modello con un segnale oggettivo.
- Costi per ask prevedibili; è il pattern usato da Claude Code `/goal`.

**Contro:**
- Un verifier LLM separato raddoppia il costo del path ask — per ask read-only è sufficiente un check deterministico (coverage su chunk citati).
- Rischio di risposte conservative (declino invece di risposta).

**Dip:** E15 per l'implementazione del coverage. **Effort: M.**

### E15 — Trust score / evidenza nel risultato ask

**Fonte:** *Building Trustworthy Agentic/RAG Workflows* (trust score e validazione esposti in UI).

**Stato esistente:** `ask_project` restituisce testo + contesto grafo, senza metadati di qualità o citanze strutturate.

**Proposta:** nel payload ask: lista chunk/commit/memorie usati (già disponibili internamente), coverage score, flag "contesto scarso" (<2 chunk sopra soglia) → l'IDE può mostrare un badge; le stesse citanze alimentano `capture_observation`.

**Pro:**
- Visibilità immediata sull'affidabilità.
- Zero costo se computato dai metadati già in mano.
- Payload MCP retrocompatibile (campo additivo).

**Contro:**
- La coverage su testo libero è fuzzy senza un parser di claim (accettare la versione citanze-based).
- Rischio di sovra-caricare l'output MCP.

**Dip:** E14. **Effort: S–M.**

### E16 — Task agentic durevoli (server mode)

**Fonte:** AI Agents course pt.6 "Durable Agents" (state che sopravvive ai crash, approval con expiry, eval gate).

**Stato esistente:** `batch_manager` rende riprendibili gli embedding (`batch_status='pending'`, sweep 10s); progress reporting esiste (`progress.rs`); nessuna persistenza per task agentic/consolidation lunghi.

**Proposta:** task store su SurrealDB per consolidation/insights lunghi (id, stato, checkpoint), ripresa post-crash, approval con expiry per le mutazioni in server mode.

**Pro:**
- Affidabilità del server condiviso; coerente con la filosofia "durable" del prodotto.

**Contro:**
- Complessità non trascurabile.
- I task lunghi attuali sono già batch riprendibili → beneficio limitato finché non ci sono agent-loop lato server.

**Dip:** C8 (i task observation sono i primi candidati). **Effort: M–L.**

---

## F. Osservabilità & valutazioni

### F17 — OpenTelemetry opt-in

**Fonte:** MLOps/LLMOps course (tracing come base della produzione AI).

**Stato esistente:** `tracing_subscriber` su stderr/file (`crates/ibis-logger/src/lib.rs:100-143`, modalità stdout-quiet per il framing stdio — da preservare); contatori solo su DB (`tokens_used`, `key_usage`); zero OTLP/metrics.

**Proposta:** crate `opentelemetry` feature-gated, export OTLP **default off** (local-first), span per tool call con attributi (project, tool, cost estimate, model tier); compatibile con la modalità stderr-quiet.

**Pro:**
- Correlazione latenza↔costo↔tool reale in produzione.
- Abilita i dati per shadow-mode (D11) e model affinity (D10).

**Contro:**
- Nuove dipendenze; rischio di default troppo rumorosi.
- Il vantaggio si realizza solo se l'utente ha un collector.

**Dip:** nessuna. **Effort: S–M.**

---

## G. Integrazione IDE

### G19 — Proiezione memory → CLAUDE.md/AGENTS.md

**Fonte:** *The anatomy of the claude folder*.

**Stato esistente:** la proiezione markdown è dichiarata opzionale ("Not a markdown sync bus", README §What it is not).

**Proposta:** comando `ibis memory project --target claude-md|agents-md`: genera una sezione con marker di rigenerazione (`<!-- ibis:begin memory -->`) contenente regole/constraints/gotcha `status=current`, per project/branch.

**Pro:**
- Porta la memoria dove gli agent già leggono; basso sforzo, alta visibilità.
- One-way con marker = non viola il claim anti-sync-bus.

**Contro:**
- Ownership del file (l'utente modifica CLAUDE.md a mano → conflitti sui marker).
- Richiede politiche di merge chiare (append-only per sezione).

**Dip:** nessuna. **Effort: S.**

### G20 — Adapter harness standardizzati

**Fonte:** *Run any agent harness under one interface* (UHP/HarnessRouter).

**Stato esistente:** già client-agnostic (Streamable HTTP `/mcp`, mcp-bridge stdio, plugin Cursor, VS Code), ma ogni integrazione ha wiring proprio.

**Proposta:** profili harness dichiarativi nel config (sampling routing, annotations, UI) condivisi tra gli host, invece di logiche replicate.

**Pro:**
- Meno duplicazione nel wiring IDE.

**Contro:**
- Beneficio marginalmente percepibile dall'utente finale; priorità bassa.

**Dip:** —. **Effort: S.**

---

## Sintesi e ordine di attacco consigliato

| Fase | Interventi | Rationale |
|------|-----------|-----------|
| **0 – Fondazione** | A1 (eval) | senza misura, i successi retrieval non sono verificabili |
| **1 – Quick win misurabili** | A2 (RRF), C9 (entity resolution), D13 (prompt stability), B6 (embedding cache) | S–M effort, basso rischio |
| **2 – Qualità** | A4 (expansion), A3 (reranker), C7 (schema memoria) | guidati dai numeri di A1 |
| **3 – Costi** | D10 (routing per-task), D11 (bounded decisions), D12 (cache a gradini) | si basano su tier + shadow-mode |
| **4 – Differenziazione** | C8 (Observations), E14/E15 (loop + trust) | il valore distintivo sul grafo |
| **5 – Opzionali** | B5 (quantizzazione), E16 (durable), F17 (OTel), G19, G20 | solo su necessità dimostrata |

## Prossimo passo

Redigere il piano di implementazione passo-passo partendo da fase 0–1 (A1 eval harness, poi quick win), con per ogni step: test, criteri di accettazione e metrica di confronto pre/post su `ibis eval retrieval`.