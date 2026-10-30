# Ibis Assistant

**Living project memory for real codebases.** Plan di prodotto e kit di applicazione:
il prodotto nasce in questo repo a partire dal piano in [PLAN.md](PLAN.md).

L'ibis sacro (*Threskiornis aethiopicus*) era l'uccello di Thoth, dio egizio della
scrittura e della memoria. Ibis è il motore; **Ibis Assistant** è il prodotto.

## A che serve questo repo

1. **[PLAN.md](PLAN.md)** — piano di implementazione bloccato (Fasi 0–3 + Track D).
2. **Kit di applicazione** — artefatti già pronti da copiare/applicare nel prodotto:
   - `plugin/` — scaffold completo dell'agent plugin `ibis-assistant`
     (plugin.json Agent Plugins 1.0, mcp.json, 5 skill, hooks, agents, commands, automations).
   - `evaluations/scope/` — config-as-code per il benchmark DevAX con Microsoft Scope
     (criteri, profili, task prompt, seed/run/analysis).
   - `scripts/scope-insights-import.mjs` — D-INSIGHTS: import degli insights di Scope
     come memoria tipizzata Ibis (dedup + ciclo di vita).
   - `scripts/migrate/` — F0.1: migrazione globale `ibis` → `ibis` dal repo sorgente
     (`C:\devsrc\ibis-assistant`, letto in **sola lettura**) verso `runtime/`.
   - `docs/` — mappa di naming e note di ricerca sulla piattaforma VS Code 1.144.

## Workflow per applicare il piano

```text
0. Leggi PLAN.md (decisioni, fasi, review gate R1–R6)
1. F0.1  pwsh scripts/migrate/migrate-rename.ps1 -Source C:\devsrc\ibis-assistant -Destination <repo>\runtime
         → cd runtime && cargo test && cargo xtask dist  → REVIEW R1
2. F0.2–F0.7  applica le sottofasi runtime descritte in PLAN.md §5 → REVIEW R2
3. F1 ∥ F2    VSIX (spec in PLAN.md §6) + copia plugin/ nel tree migrato (§7) → REVIEW R3/R4
4. F3         docs/posizionamento (§8) → REVIEW R5
5. Track D    evaluations/scope (§9): seed → run → D-CLAIMS → R6 gate publish → D-INSIGHTS loop
```

Regola ferrea: il repo sorgente `ibis-assistant` non viene mai modificato.