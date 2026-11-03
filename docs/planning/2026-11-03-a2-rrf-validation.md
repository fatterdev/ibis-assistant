# Fase 1 (WQ-5) — A2 RRF: implementazione e validazione

**Data:** 2026-11-03
**Fonte:** contratto funzionale in [analisi L3 §A2](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md);
baseline pre-intervento in [piano A1](./2026-11-02-a1-retrieval-eval-plan.md).
**Stato:** implementato e validato 03/11/2026 — **default resta `additive`**
(l'accettazione L3 "≥ baseline additive" passa sull'embedder offline, NON su
Ollama reale → decisione owner richiesta prima di girare il default).

## Cosa è stato implementato

| Artefatto | Contenuto |
|---|---|
| `crates/ibis-search/src/retrieval_boost.rs` | `FusionMode` (additive \| rrf, `parse`/`resolve` con warn su valore ignoto), `rrf_fuse(channels, k)` (k=60, contributi sommati cross-channel, rank 0 scartati), `rrf_max_score` (normalizzazione teorica `n_canali/(k+1)`, canali con ≥1 hit), `combine_rrf_score` (`rrf_norm*0.75 + kw*0.15 + ent*0.10`, base clampata 0..1), `apply_rrf_boost` (sort desc + **tie-break deterministico per id**); `ScoredHit` guadagna `id` |
| `crates/ibis-search/src/search.rs` | `ask_project` annota i rank per canale durante la raccolta (già per-canale); in RRF il bonus "is current" NON scala più il cosine: diventa epsilon costante `CURRENT_EPSILON=0.05` applicato alla base normalizzata **dopo** la fusione (stesso predicato `at_head` del bump legacy, che in additive resta identico); sort con tie-break per id |
| `crates/ibis-core/src/config.rs` | `search.fusion = "additive" \| "rrf"` (default `additive`, rollout sicuro; valore ignoto → additive + warn) |
| wiring | `ibis-server/src/main.rs` (`build_search_service`) e `state.rs` (`rebuild_search`) impostano `service.fusion_mode` dalla config |
| `crates/ibis-server/src/eval_mode.rs` | flag `--fusion additive\|rrf` su `ibis eval retrieval` (default additive) |
| `crates/ibis-optimization/src/eval.rs` | `EvalReport.fusion` nel report JSON (comparabilità A2/A3/A4 a parità di fusion) |

Registry MCP invariato (57 tool — nessun tocco a registry/contratti, coerente
con §3.1 della coda). `combine_retrieval_score` additive e i suoi test di pin
(`combine_keeps_base_share`, `boost_sorts_descending`) restano intatti per la
modalità legacy.

## Test

- Nuovi unit RRF (`retrieval_boost.rs`): contributi sommati cross-channel,
  skip canali vuoti/rank 0, sensibilità k (gap rank si appiattisce con k
  grande), `rrf_max_score` teorico, pesi e clamp di `combine_rrf_score`,
  tie-break per id, promozione keyword.
- Nuovo test `ask_project` in modalità RRF (`search.rs`):
  `ask_project_rrf_fusion_ranks_by_channel_rank` — il ranking segue i rank di
  canale e non la scala del cosine (rank-1 memory pareggia rank-1 file via
  tie-break id e supera un rank-2 file con cosine molto più alto).
- Test config `search_fusion_config` (default additive, override rrf).
- `cargo clippy --workspace --all-targets -- -D warnings` verde;
  `cargo test --workspace` verde a esclusione di 2 test preesistenti di
  `ibis-ai` **ambientali** (falliscono quando Ollama risponde su :11434:
  `default_api_url_substituted` assume la porta giù; verificati fallire anche
  su HEAD pulito — non imputabili ad A2).

## Validazione (03/11/2026 — SurrealDB 3.3.5 locale su :38000, dataset golden A1 12 case)

### Embedder offline (`offline-hash-768`, deterministico, = baseline A1)

| Fusion | MRR | recall@5 | recall@15 | case ok |
|---|---|---|---|---|
| `additive` (baseline, riprodotta) | 0.917 | 0.958 | 1.000 | 12/12 |
| `rrf` | 0.917 | 0.958 | 1.000 | 12/12 |

Per-case (delta): r09_rrf_commit **2→1** (migliora), r05_spawn_threads
**1→2** (regredisce), r08/r11 invariati. Aggregati identici → parità.

### Embedder Ollama reale (`nomic-embed-text` — la misura mancante della baseline A1)

| Fusion | MRR | recall@5 | recall@15 | case ok |
|---|---|---|---|---|
| `additive` (nuova baseline reale) | **0.917** | **1.000** | 1.000 | 12/12 |
| `rrf` | 0.875 | 0.958 | 1.000 | 12/12 |

Delta per-case: r05_spawn_threads primo expected hit **1→2**; r11_multi_eval_rrf
secondo expected hit **5→9** (questo costa il recall@5). Gli altri 10 case
invariati.

## Esito accettazione (L3 §A2)

- ✅ Fallback config funzionale (additive \| rrf, default additive, warn su
  valore ignoto); nessuna regressione sui 4 canali / unit test; A1 harness
  riusato senza modifiche al kernel.
- ⚠️ "MRR/recall@5 ≥ baseline additive": **passa su offline (parità), fallisce
  su Ollama reale** (MRR −0.042, recall@5 −0.042). Causa: il tradeoff
  documentato in L1 — "RRF usa solo i rank (perde l'informazione di margine)".
  Con embedder semantico il margine cosine del rank-1 viene ignorato e chunk
  di canali rivali al rank 1 di canale sopravanzano gli expected hit (r05:
  un memory rule superato da un file_chunk rank-1 di canale; r11: il secondo
  file_chunk atteso scivola fuori dalla top-5).

## Decisione richiesta all'owner

1. **Tenere default `additive` e procedere** con C9+D13+B6 (fase 1), A2 resta
   dietro config per tuning successivo (k, ibride rank+cosine, interazione con
   A3 reranker / A4 query expansion che potrebbero chiudere il gap r05/r11).
2. Tuning RRF prima di decidere (es. k più piccolo, o fusione ibrida
   rank+cosine normalizzata — fuori dallo scope A2 come specificato).
3. Accettare comunque la parità offline e girare il default a `rrf`
   (sconsigliato: perde su embedder reale).

Raccomandazione: opzione 1 — A2 è atterrato con rollback a config; la perdita
di margine è esattamente ciò che A3 (reranker) e A4 (query expansion) addressano.