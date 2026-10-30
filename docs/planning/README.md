# Ibis Assistant — Planning Kit

**Living project memory for real codebases.** Piano di prodotto e kit operativo
integrato nella root del repository **Ibis Assistant** (`fatterdev/ibis-assistant`).

L'ibis sacro (*Threskiornis aethiopicus*) era l'uccello di Thoth, dio egizio della
scrittura e della memoria. `ibis` è il motore e binario CLI; **Ibis Assistant** è il prodotto.

## Contenuto del planning kit

1. **[PLAN.md](PLAN.md)** — piano di implementazione bloccato (Fasi 0–3 + Track D, con revert del naming del 30/10/2026).
2. **[naming-map.md](naming-map.md)** — mappa canonica di revert `threski` → `ibis` / `Ibis Assistant`.
3. **[vscode-platform-notes.md](vscode-platform-notes.md)** — note di ricerca piattaforma VS Code 1.144, Agent Plugins 1.0, MCP Apps e Microsoft Scope.
4. **[review-r2-checklist.md](review-r2-checklist.md)** — gate R2 (Parte A protocollo automatizzato + Parte B smoke manuale in VS Code 1.144).
5. **Artefatti del piano nel repo**:
   - `plugin/` — agent plugin `ibis` (Agent Plugins 1.0: `plugin.json`, `mcp.json`, 5 skill corte per slash `/ibis <nome>`, hooks, agents, automations; nessun `commands/` duplicato).
   - `vscode-extension/` — estensione VSIX `fatterdev.ibis-assistant`.
   - `evaluations/scope/` — config-as-code per il benchmark DevAX con Microsoft Scope (criteri, profili, task prompt, seed/run/analysis).
   - `scripts/scope-insights-import.mjs` — D-INSIGHTS: import degli insights di Scope come memoria tipizzata Ibis (dedup + ciclo di vita).
   - `scripts/migrate/` — storico migrazione F0.1 (`ibis → threski`, 29/10/2026) + script di revert (`rename-back-ibis.ps1`, 30/10/2026).

## Sequenza delle fasi

```text
F0.1 (rename + R1, chiuso) + Revert ibis (30/10)
  → F0.2–F0.7 (runtime MCP completato) → REVIEW R2
  → [F1 (VSIX) ∥ F2 (Agent plugin ibis)] → REVIEW R3 / R4
  → F3 (docs / posizionamento vs Copilot Memory) → REVIEW R5
  → Track D (evaluations/scope: seed → run → D-CLAIMS → R6 gate publish → D-INSIGHTS loop)
```
