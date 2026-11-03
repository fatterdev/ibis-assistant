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
| WQ-1 | **R3/R4 gate** — VSIX 1.0.0 + plugin agent (Parte B) | [review-r3r4-checklist.md](./review-r3r4-checklist.md) | Parte A 26/26 PASS; Parte B: R3.1 ✅ R3.2 ✅ **R3.3 in debug** (S1.1–S1.2 done, S1.3 pending) | **S1.3**: riverificare R3.3 frasi B/C con build S1.1 (target: espansione freddo <2s, caldo <500ms) | rebuild binario stale (sidecar-lifecycle-plan §esecuzione, punti 1–3) | R3.4–R3.8, P4.1–P4.6, publish gate |
| WQ-2 | **Publish gate** (azioni ② ③ ⑤ + `publish-vscode.yml`) | [review-r3r4-checklist.md §Publish gate](./review-r3r4-checklist.md) | ☐ non iniziato | azioni manuali ②③ (Trusted Publishing OIDC, Open VSX token) | WQ-1 (R3/R4 PASS) | WQ-3, WQ-4 |
| WQ-3 | **F4 — Memory view + team server** (VSIX 1.1.0 / runtime 0.3.0) | [2026-11-01-memory-view-team-server-design.md](../superpowers/2026-11-01-memory-view-team-server-design.md) | Approved; 9 sub-fasi (F4-B1..B4 ∥ F4-A1..A5) + gate R7 da fare | **F4-B1** — reconciliation Layer 3 (ledger + liveness + start-time) | WQ-2 | gate R7 → F3 docs → Track D |
| WQ-4 | **Track D** — benchmark DevAX/Scope (D1–D5) | [PLAN.md §9](./PLAN.md), [evaluations/scope/README.md](../evaluations/scope/README.md) | Pianificato (scaffold presente; profili con placeholder `TODO-verify-*` by design, pre-D2) | D1 — pin immagine compose + seed su istanza Scope live | WQ-3 (gate R7), azione D1 | D2–D5, R6 close |
| WQ-5 | **Miglioramenti ddofds** — fasi 0–5 (eval, retrieval, costi, memoria) | [analisi L1](./2026-11-01-dailydoseofds-improvement-analysis.md) → [L2](./2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md) → [L3](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md); [piano A1](./2026-11-02-a1-retrieval-eval-plan.md) | **Fase 0 (A1) done 02/11**: dataset 12 case + runner (mrr/recall@5/@15 per-channel) + CLI `ibis eval retrieval` (report JSON, soglia exit) + job CI `retrieval-eval`; baseline offline MRR 0.917 / r@5 0.958 / r@15 1.0 (working tree non committato) | **Fase 1** — A2 RRF al posto della fusione additiva, validato dalla baseline A1 (poi C9+D13+B6) | — (baseline A1 verde) | fasi 1–3; baseline per Track D D3 |

Storico chiuso (non più in sospeso): F0.1–F0.7, gate R2, runtime 0.2.0 — vedi [PLAN.md](./PLAN.md).

## 2. Sequencing riconciliato

Fonti: PLAN.md (sequencing originale), F4 spec (sequencing aggiornato, 01/11), checklist R3/R4 (stato reale).

```text
[R3/R4 Parte B]──PASS──►[Publish gate ②③⑤]──►[F4-B1..B4 ∥ F4-A1..A5]──►[R7]──►[F3 docs]──►[Track D D1..D5]──►[R6]
     (WQ-1)                    (WQ-2)                (WQ-3)                                (WQ-4)

parallelo, runtime-side, ortogonale alla VSIX:
     [ddofds fase 0: A1 eval] ──► [fase 1: A2+C9+D13+B6] ──► [fasi 2–3: A4+A3+C7, D10+D11+D12]   (WQ-5)
```

Nota di riconciliazione: PLAN.md aveva `…R3/R4 → F3 → D1..D3 → R6`; l'F4 spec (più recente) inserisce `F4 → R7` tra publish e F3. La sequenza attesa è quella sopra (F4 prima di F3/Track D).

## 3. Decisioni di coordinamento (dove i piani si toccano)

1. **Superficie tool 57 → 58.** F4-A3 introduce `memory_workstreams` e un filtro su `memory_list` (design F4 §A.3). Ogni intervento ddofds che tocca il registry/contratti (nessuno delle fasi 0–1; D10 cambia solo hint, non tool) deve seguire il pattern consolidato: contract test + discovery fixture aggiornati nello stesso commit.
2. **Consolidamento: F4-B3 (daemon-only, opt-in) vs C9/D11 (ddofds).** C9 (soglia+entity resolution) e D11 shadow-mode cambiano gli *interni* di `consolidate_loop`, non il trigger: compatibili con F4-B3 e landabili in qualsiasi ordine. D12 (response cache) non tocca il consolidation. Se F4-B3 atterra prima, C9/D11 lavorano sulla versione daemon-owned; se dopo, nessun conflitto previsto.
3. **A1 eval → Track D D3.** La baseline di retrieval quality (A1) è complementare al D3 "memoria → outcome": A1 misura l'ibis interno, D3 misura l'agente end-to-end. Fare A1 prima che D1..D3 parta in earnest rende le fasi 2–3 ddofds misurabili con Track D in parallelo.
4. **Timeout/sidecar.** WQ-1 S1.1 (client stdio persistente) e il rebuild del binario stale sono prerequisiti di misura per S1.3; l'F4-B1 (reconciliation ledger) complica il sidecar lifecycle — fare S1.3/S1.4 PRIMA di F4-B1 evita due volte le stesse misure.
5. **Versioni.** VSIX resta 1.0.0 per il publish gate; runtime resta 0.2.0 finché il root `[Unreleased]` non viene taggato (decisione in checklist §Publish gate). F4 porta VSIX 1.1.0 / runtime 0.3.0. Gli interventi ddofds vanno nel root `[Unreleased]` runtime, nessuna bump propria.

## 4. Stato per stream (dettaglio minimo)

### WQ-1 — R3/R4
- Fatto: Parte A 26/26 (30/10); R3.1/R3.2 PASS; S1.1 stdio persistente (VSIX ripacchettata 58.46 KB, smoke 26/26); S1.2 igiene ambiente owner (pluginLocations corretto, binario Go stale rimosso).
- Aperto: S1.3 misura timing R3.3 → S1.4 demote log info→debug → R3.3 chiusura → R3.4–R3.8 → P4.1–P4.6 → publish gate.

### WQ-3 — F4
- Nessun sub-fase iniziata. Primo commit previsto: `feat(runtime): sidecar reconciliation at startup (layer 3)` (F4-B1).
- Dipendenza interna da rispettare: B.6 (reconciliation) è precondizione strutturale del Milestone B.

### WQ-4 — Track D
- Disallineamento README/analysis.mjs (pass@k annunciato vs pass-rate implementato) e placeholder profili: **stato atteso**, non difetto — si chiudono a D2 (runner+analysis) e D1 (pin/seed su istanza live). Registrato anche come correzione nell'analisi L2 §4.1.

### WQ-5 — ddofds
- **Fase 0 (A1) completata 02/11/2026** (working tree, commit da fare nel §Ordine
  del [piano A1](./2026-11-02-a1-retrieval-eval-plan.md)): `eval.rs` in
  ibis-optimization (dataset YAML validato, `RetrievalEvalRunner` con mrr /
  recall@5 / recall@15 / per-channel, estate golden auto-seminata con id
  espliciti, `OfflineEmbedder` deterministico), CLI in-process
  `ibis eval retrieval` (niente nuovo tool MCP: registry resta a 57), dataset
  `evaluations/retrieval/dataset.yaml` (12 case), job CI `retrieval-eval`
  (SurrealDB pinnato v3.3.5 + embedder offline, soglia 0.75). Baseline offline:
  MRR 0.917, recall@5 0.958, recall@15 1.0 — 12/12 ok.
- Restano: misurare la baseline con Ollama reale quando torna su :11434;
  committare secondo il §Ordine del piano.
- Fasi 0–3 specificate in L3 con criteri di accettazione per intervento; il
  prossimo è A2 (RRF) validato dalla baseline A1.