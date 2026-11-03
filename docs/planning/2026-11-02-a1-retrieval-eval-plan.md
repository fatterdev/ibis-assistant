# Fase 0 (WQ-5) — A1 eval harness retrieval: piano di implementazione

**Data:** 2026-11-02
**Fonte:** contratto funzionale in [analisi L3 §A1](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md)
(registrato in [master work queue](./2026-11-02-master-work-queue.md) come WQ-5).
**Stato:** fase 0 implementata e verificata in locale (02/11/2026, working tree
non ancora committato — commit nel §Ordine).

## Obiettivo

Dataset golden + runner + CLI + CI per misurare la qualità retrieval dell'ibis
interno (canali file_chunk / memory / commit_chunk / reasoning via
`ask_project`), come **baseline** prima degli interventi A2 (RRF), A3
(reranker), A4 (query expansion). Il kernel `evaluate_retrieval`
(`crates/ibis-optimization/src/raft.rs:109-130`) resta invariato.

## Decisioni di design (e perché)

1. **Estate golden deterministica auto-seminata.** I dataset case puntano a id
   espliciti (`file_chunk:g_*`, `memory:g_*`, `commit_chunk:g_*`,
   `reasoning:g_*`) di un estate golden seminato dal runner stesso in un DB
   scratch dedicato (pattern `live_memory_estate.rs`: sidecar SurrealDB su
   `IBIS_SPIKE_SURREAL_URL`, default `ws://127.0.0.1:38000`; DDL
   `schema::init_schema`; wipe + re-seed a ogni run → deterministico e
   ripetibile). I seed derivano da scenario reali del repo (timeout MCP R3.3,
   vincolo thread PDEATHSIG, gotcha `math::max` SurrealDB 3.x, task prompts di
   `evaluations/scope/task-prompts/fixture-tasks.md`).
2. **Embedder esplicito nel report.** Il seed e le query embedding usano la
   STESSA istanza provider (comparabilità). `--embedder auto` (default):
   sonda `embed_text` su Ollama (`nomic-embed-text`), fallback `offline`
   (hash 768-dim, pattern `H03Ai`) con campo `embedder` nel report — i
   baseline sono comparabili solo a parità di embedder. `--embedder offline`
   forza il determinismo (job CI).
3. **CLI in-process, registry MCP invariato (57 tool).** `ibis eval retrieval`
   viene intercettato in `ibis-server/src/main.rs` PRIMA del passthrough CLI
   (che è un client MCP puro): monta direttamente
   `SurrealDbExecutor` + provider + `SearchService` e gira il runner. Nessun
   nuovo tool MCP → nessun tocco a registry/contratti (coerente con la
   decisione di coordinamento §3.1 della coda: le fasi 0–1 non toccano il
   registry; il 58° tool resta riservato a F4-A3 `memory_workstreams`).
4. **Metriche tolleranti al rank.** Per case: `mrr` (1/rank del primo expected
   hit), `recall@5`, `recall@15` (match per `SearchResult.id`; l'id restituito
   è senza parentesi angolari — forma `memory:g_*`, verificata in
   `live_memory_estate.rs`), `top_channel` = `source_kind` del rank più alto.
   Degraded (search error) → case `error`, escluso dalle medie, non `fail`
   (distinguere ambienti senza AI da fallimenti di ranking).

## Superficie

| Artefatto | Contenuto |
|---|---|
| `crates/ibis-optimization/src/eval.rs` | `EvalCase`, `RetrievalDataset` (serde_yaml, validazione: id unici, query/expected non vuoti), `RetrievalEvalRunner` (mrr/recall@5/@15/per-channel), `EvalReport` (serde JSON), estate golden + seed, `OfflineEmbedder` |
| `evaluations/retrieval/dataset.yaml` | ≥10 case golden (id/query/expected_ids/note) |
| `crates/ibis-server/src/main.rs` | dispatch `eval` → `eval_mode` (parse argomenti, connect/seed/run, report JSON su stdout + `--out`, exit code 1 sotto soglia) |
| `.github/workflows/ci.yml` | job `retrieval-eval` (ubuntu-24.04): sidecar SurrealDB (immagine pinnata), build, eval offline con soglia, upload report |

CLI: `ibis eval retrieval --dataset <path> [--min-recall-5 X] [--url U]
[--ns N] [--db D] [--user/--pass] [--embedder auto|offline|ollama]
[--ollama-url U] [--no-seed] [--out <path>]`

## Ordine dei commit (stile repo, 1 commit per punto)

1. `feat(optimization): retrieval eval runner (mrr/recall@k, per-channel) + golden estate seed` —
   `eval.rs` + test unit (fake db/ai pattern `memory_consolidate.rs` tests) +
   `serde_yaml` dep.
2. `feat(evaluations): golden retrieval dataset (12 case)` — `dataset.yaml`.
3. `feat(server): ibis eval retrieval CLI (seed, report JSON, soglia exit)` —
   main.rs dispatch + help.
4. `ci: retrieval-eval job (surreal sidecar pinnato + embedder offline)` +
   baseline documentata.

## Accettazione (da L3 §A1)

- Dataset con ≥10 case; validazione rigida (id unici, expected non vuoti).
- `ibis eval retrieval` produce report JSON con MRR/recall/per-channel;
  exit non-zero sotto `--min-recall-5`.
- CI verde (job separato, sidecar SurrealDB, embedder offline deterministico).
- Baseline numerata (MRR/recall@5/@15 offline + ollama se disponibile)
  documentata in fondo a questo documento → numeri pre-A2/A3/A4.
- `evaluate_retrieval` invariato.

## Rischi / note

- L'embedder offline (hash bytes→dims) è volutamente grossolano: la soglia CI
  si fissa sulla SUA baseline (marginando), non sulla qualità semantica; il
  confronto A2/A3/A4 usa lo stesso embedder + verifica parallela su Ollama.
- `ask_project` tronca il board a 15 (`VECTOR_CHANNEL_LIMIT`): recall@15 è
  "recall sull'intero board restituito" (documentato nel report).
- Il job CI usa un'immagine SurrealDB pinnata (coerente con la pratica
  D1 "pin immagine compose" di Track D; crate `surrealdb` 3.3.4).

## Baseline (misurata 02/11/2026 — SurrealDB sidecar locale 3.3.5, crate 3.3.4)

| Embedder | MRR | recall@5 | recall@15 | case ok |
|---|---|---|---|---|
| `offline-hash-768` | **0.917** | **0.958** | **1.000** | 12/12 |
| `ollama` (`nomic-embed-text`) | **0.917** (misurata 03/11, additive) | **1.000** | **1.000** | 12/12 |

Per-channel (offline): file_chunk 5, memory 5, commit_chunk 1, reasoning 1.
Punti deboli baseline: `r08_rrf_decision` e `r09_rrf_commit` al rank 2 (il
canale rivale vince il rank 1 — atteso, tollerato da recall@5), `r11` recall@5
0.5 (secondo expected hit al rank 9). **Soglia CI: `--min-recall-5 0.75`** —
marginata sotto la baseline offline per beccare regressioni grossolane di
ranking senza falsi rossi su variance di ambiente.

Note ambientali: server SurrealDB 3.3.5 (binario locale) contro crate 3.3.4 —
compatibilità ws verificata in questa sessione (live test del workspace verdi);
immagine CI pinnata `surrealdb/surrealdb:v3.3.5`.

## Esito accettazione (02/11/2026)

- Dataset 12 case (≥10) con validazione rigida — `eval::tests::dataset_validation_matrix`.
- `ibis eval retrieval` end-to-end su SurrealDB reale: report JSON con
  MRR/recall/per-channel, `--out`, exit 1 sotto soglia (verificato: 0.99 → 1,
  0.75 → 0), fallback `auto` → offline con messaggio esplicito.
- Unit test ibis-optimization 16/16 + eval_mode parse 2/2; `cargo clippy
  --workspace --all-targets -- -D warnings` verde; `cargo test --workspace`
  verde (live test inclusi).
- `evaluate_retrieval` invariato (raft.rs non toccato).