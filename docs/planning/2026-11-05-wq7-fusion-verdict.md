# WQ-7 — verdetto fusione (A2 chiuso qui)

**Data:** 05/11/2026
**Contesto:** [spec WQ-7](./2026-11-05-surrealdb-4-upgrade.md) assorbe il retune
A2: sulla 4.0 si rimisura `ibis eval retrieval` con i canali DiskANN più il
canale lessicale e si rituna RRF prima di decidere il default
`search.fusion`. Nota di supporto: [validazione A2 su 3.3.5](./2026-11-03-a2-rrf-validation.md).

## Misure (dataset golden, sidecar 4.0.0, crate 4.0.0)

| Embedder | Fusione | MRR | recall@5 | recall@15 |
|---|---|---|---|---|
| offline | additive | 0.917 | 0.958 | 1.000 |
| offline | rrf k=60 | 0.917 | 0.958 | 1.000 |
| offline | rrf k=10 | 0.917 | 0.958 | 1.000 |
| offline | rrf k=1 | 0.917 | 0.958 | 1.000 |
| ollama | additive | **0.917** | **1.000** | **1.000** |
| ollama | rrf k=60 | 0.875 | 0.958 | 1.000 |
| ollama | rrf k=10 | 0.875 | 0.958 | 1.000 |
| ollama | rrf k=1 | 0.875 | 0.958 | 1.000 |

## Re-misura post-remediation (05/11/2026 — dataset 20 case, fusione corretta)

La review WQ-7 ha corretto tre difetti che inficiano questa misura:
**F03** (il dedupe globale pre-RRF impediva la somma cross-channel — ora ogni
canale contribuisce e il dedupe avviene solo dopo la fusione), **F04** (il BM25
grezzo entrava nel blend additive insieme a score cosine/decay calibrati — ora
passa da `s/(s+K)` monotona limitata, K=3, ordinamento intra-canale invariante
alla scala), **F06** (l'indice `file_chunk.content` non alimentava
`ask_project` — ora il canale `file_chunk_text` è in entrambe le fusioni) e
**F05** (il subject lookup cercava solo nel contenuto — ora anche nel titolo).
Il dataset golden cresce da 12 a 20 case con gli 8 casi full-text/esclusione
della sezione 9 del [piano remediation](./2026-11-05-surrealdb-remediation-agent-plan.md);
il report conserva il `channel_id` interno per caso e le violazioni di
esclusione (`absent_leaks`).

| Embedder | Fusione | MRR | recall@5 | recall@15 | case ok |
|---|---|---|---|---|---|
| offline | additive (calibrata) | **0.950** | **0.975** | **1.000** | 20/20 |
| offline | rrf k=1 | 0.842 | 0.950 | 1.000 | 20/20 |
| offline | rrf k=10 | 0.918 | 0.925 | 1.000 | 20/20 |
| offline | rrf k=60 | 0.908 | 0.925 | 1.000 | 20/20 |

## Verdetto

**`search.fusion` resta `additive` — ora su stack corretto e dataset esteso.**
Il retune è stato rieseguito dopo i fix (k ∈ {1, 10, 60}): RRF non batte
additive su nessun k (mrr 0.842/0.918/0.908 contro 0.950). A differenza della
misura pre-remediation, l'obiezione "additive per inerzia, dominato dalla
scala del canale" è chiusa: la calibrazione F04 toglie al BM25 il suo vantaggio
di unità di misura e additive vince comunque. I casi nuovi chiudono tutti a
MRR 1.0 con attribuzione puntuale di canale (`memory_text_title`,
`memory_text_content`, `file_chunk_text`) e zero violazioni di esclusione
(personale/superseded non emergono mai).

Ciò che cambia rispetto ad A2 su 3.3.5: la regressione rrf si sposta di case
(`r05_spawn_threads` e `r08_rrf_decision` al rank 2 invece di `r08`/`r09`), e
la base numerica è la baseline 4.0 pubblicata nel
[piano A1](./2026-11-02-a1-retrieval-eval-plan.md#wq-7-baseline-wq-7-misurata-05112026--surrealdb-sidecar-400-crate-4000-diskann--canale-lessicale).

## Cosa resta aperto per il ranking

- **A3 reranker** consuma la finestra candidati (`search.candidates`, default
  50) già filodata da WQ-7: il suo verdetto potrà rialzare il pavimento CI
  oltre 0.75.
- **C9 entity resolution** usa `ORDER BY vector::distance::cosine` più il
  canale full-text (`search::score(1)`) per gli alias — canali già in opera.
- Il canale lessicale entra nel board come `memory_text_title` /
  `memory_text_content` / `file_chunk_text` (un indice FULLTEXT BM25 per
  campo), **senza** dedupe globale pre-fusione (F03: lo stesso documento in
  più canali somma i contributi; il dedupe del board avviene dopo la fusione).
  Il BM25 entra calibrato (`s/(s+K)`, F04), mai grezzo.