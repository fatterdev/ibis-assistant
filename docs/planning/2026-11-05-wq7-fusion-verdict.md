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

## Verdetto

**`search.fusion` resta `additive`.** Non per inerzia: il retune richiesto
dalla spec è stato eseguito (k ∈ {60, 10, 1}) e RRF non batte additive su
nessuna configurazione — parità offline, regressione ollama (MRR 0.875,
perdita r@5) stabile su tutti i k. Il canale lessicale in più non cambia il
rapporto: i case memory già vinti dal canale vettore non mutano ranghi sul
golden set.

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
  `memory_text_content` (due indici FULLTEXT BM25, un indice per campo), con
  dedupe per record id contro il canale vettore memory (prima vittoria).