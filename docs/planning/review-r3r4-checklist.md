# REVIEW R3/R4 — VSIX "Ibis Assistant" + plugin agent in VS Code 1.144

> Gate R3 di [PLAN.md](PLAN.md §6): *VSIX installata, UI verificata; publish su
> Marketplace + Open VSX + registry*. Gate R4 (§7): *plugin installato da
> sorgente (`chat.pluginLocations`), skill in `/`, hooks fire, MCP server del
> plugin avviato*. Prerequisiti: azioni manuali **②** (Partner Center publisher
> `fatterdev` + Trusted Publishing OIDC), **③** (account Open VSX) e **⑤**
> (installazioni locali per i review).
>
> Naming corrente (revert 30/10/2026): VSIX `fatterdev.ibis-assistant`
> (settings `ibis.*`, commands "Ibis Assistant: *"), plugin `ibis`
> (slash `/ibis <nome>`), marketplace `fatterdev/ibis-marketplace`.

## Parte A — automatizzata (26/10-check, eseguita 30/10/2026)

`node scripts/smoke-plugin.mjs` — nessuna UI richiesta. **26/26 PASS**:

| Gruppo | Check | Esito |
|---|---|---|
| Plugin scaffold | plugin.json (name `ibis`, schema Agent Plugins 1.0), mcp.json spawn `ibis -mode stdio -runtime-mode plugin` | 2/2 PASS |
| Skills | 5 SKILL.md con frontmatter name/description (`usage`, `readiness`, `status`, `remember`, `set-working-state`) | 5/5 PASS |
| Hooks (contratto verificato su docs.github.com/copilot/reference/hooks-reference) | hooks.json `${PLUGIN_ROOT}` + `timeoutSec`; `session-start.mjs` → solo `additionalContext` (397 caratteri); `agent-stop.mjs`: cattura già fatta → `{}`, `stop_hook_active` → `{}` (una sola continuazione), edit senza cattura → `decision: block` + reason (368 caratteri), nessun segnale → `{}`, transcript assente → `{}` (fail-open) | 7/7 PASS |
| Agents/automations | `ibis-forensics` (tool tutti read-only), `ibis-curator`, 2 automazioni con cron | 4/4 PASS |
| VSIX | engines `^1.144.0` + vsce 4.x + id `fatterdev.ibis-assistant`; ogni comando di menu contribuito (6 ref); `chatInstructions` con file esistente (2576 caratteri); walkthrough 4 step | 4/4 PASS |
| Distribuzione | `server.json` (`io.github.fatterdev/ibis`, streamable-http), `marketplace/marketplace.json` (source github `plugin/`), badge `vscode:mcp/install`, deep link `vscode://chat-plugin/install` | 4/4 PASS |

Integrazione continua del packaging: `npm run package` in `vscode-extension/` →
`ibis-assistant-1.0.0.vsix` (17 file, ~55.9 KB) verde a ogni subfase F1.x;
`node --check` su `extension.js`, `memory-tree.js`, `remember-selection.js`.

## Parte B — R3: VSIX installata in VS Code 1.144 (runbook owner)

Pre-flight: binario `ibis` sul PATH (o `ibis.runtime.binaryPath`), Ollama
attivo su `11434` con `nomic-embed-text` (serve a `memory_search` nella tree
view), account VS Code con Copilot agent mode.

### R3.1 — Installazione e attivazione

```powershell
code --install-extension C:\devsrc\ibis-assistant\vscode-extension\ibis-assistant-1.0.0.vsix
```

- **PASS**: attivazione senza errori; Output **Ibis Assistant** mostra
  `activated (ide-host: package|vendor, engine: 1.144.x)`; nessun warning
  `ide-host package import failed` (in pacchetto è atteso `vendor`).
- Verifica engines: l'estensione non installa su VS Code < 1.144 (`engines.vscode`).

### R3.2 — Status bar item

- **PASS**: item destro con testo `$(cloud|terminal) Ibis attach|spawn`;
  con bridge LM attivo compare `$(zap)`, con MCP provider registrato `$(check)`.
- Click sull'item → esegue **Ibis Assistant: Status** (toast/output secondo
  `ibis.status.outputMode`).
- Cambio di una setting `ibis.*` (es. `ibis.runtime.mode`) → item si aggiorna
  senza reload (messaggio di reload richiesto solo per bridge/provider).

### R3.3 — Memory tree view

- **PASS**: container activity bar **Ibis Assistant** (icona ibis) → view
  **Memory** con 8 kind (`decision, fix, rule, constraint, gotcha, design,
  fact, ask`).
- Espandi `decision` → voci da MCP `memory_search` (query semantica con il
  kind come seed + filtro kind, limit 50); descrizione con status/visibilità/tag.
- Click su una voce → documento read-only `ibis-memory:` con metadati
  (kind/status/visibility/tags/confidence/updated) e contenuto completo
  (`memory_expand`; fallback: summary di ricerca se l'expand fallisce).
- **Ibis Assistant: Search Memory** → query libera → root `search: "…"` con i
  match; icona refresh nella view title ricarica i nodi espansi.
- Degradazione attesa (non blocca): senza AI provider configurato, l'espansione
  mostra il nodo `error: …` (log in output channel) — comportamento runtime,
  non difetto VSIX.

### R3.4 — Remember Selection con provenance

1. Seleziona codice in un file del workspace → context menu editor →
   **Ibis Assistant: Remember Selection** (o Command Palette).
2. Quick pick kind → titolo → **PASS**: toast `Ibis Remembered (…)` con
   `path:lineaInizio-lineaFine`.
3. Verifica la memoria nella tree view / dettaglio: contenuto termina con
   `Source: <percorso-relativo>:<start>-<end>` e la chiamata `memory_add` ha
   passato `source_path` + `source_hash: sha256:<hex>` (evidenza: risposta
   tool in chat dopo ingest, o `settings_get`/log — lo schema è coperto da
   Parte A solo a livello contrattuale; il bind staleness è runtime).

### R3.5 — Remember File (editor + explorer)

- Editor senza selezione → **Ibis Assistant: Remember File** → prompt
  "What should be remembered about …" → **PASS**: memoria con
  `Source: <rel>:1-<N> (whole file)`.
- Explorer: click destro su file → **Ibis Assistant: Remember File** → stesso
  flusso con il file selezionato (senza editor attivo).

### R3.6 — Chat instruction (contratto di sessione)

1. In chat (modalità agent) `/` → cerca `ibis-session-contract` → **Attach**.
2. **PASS**: l'istruzione è iniettata; su un nuovo task l'agente chiama
   `load_session_context` una sola volta e non martella `memory_*`
   (stesso contratto della skill `usage` del plugin — superficie VSIX).
3. Verifica cross-surface: il contratto NON compare anche come skill
   (duplicati vietati — decisione §1 del piano).

### R3.7 — Walkthrough onboarding

- **Help / Welcome → Get Started with Ibis Assistant** (o primo avvio):
  4 step (`install-binary`, `choose-backend`, `browse-memory`,
  `remember-with-provenance`), link comandi funzionanti
  (`ibis.openExtensionSettings`, `workbench.view.extension.ibis`, `ibis.remember*`),
  completion events che spuntano gli step (onView/onSettingChanged/onCommand).

### R3.8 — MCP provider dalla VSIX (spawn isolato)

- **MCP: List Servers** → `ibis` (contributo `mcpServerDefinitionProviders`)
  → Start → in agent mode ripetere il mini-smoke R2 B1: `load_session_context`
  e `get_working_state` **zero conferme**; `memory_add` chiede conferma.
- Impostazioni runtime (spawn/attach/auto-attach/data dir) ricomputano la
  definition senza reload (solo `lmBridge.*`/`registerProvider` chiedono reload).

## Parte B — R4: plugin `ibis` da sorgente

Pre-flight: cartella `plugin/` del repo.

1. **Install from source**: Command Palette → **Chat: Install Plugin from
   Source** → seleziona la cartella `plugin/` (oppure `chat.pluginLocations`
   con `{ "<repo>\\plugin": true }` per il test locale senza installazione).
2. **P4.1 Skill in `/`**: digita `/ibis` → compaiono le 5 skill come
   `/ibis usage`, `/ibis readiness`, `/ibis status`, `/ibis remember`,
   `/ibis set-working-state`; `/ibis status` risponde con stato backend/AI.
3. **P4.2 MCP server del plugin**: **MCP: List Servers** → `ibis` avviato dal
   `plugin/mcp.json` (spawn `ibis -mode stdio -runtime-mode plugin`);
   da attach, sostituire con `{ "type": "http", "url": "http://127.0.0.1:3030/mcp" }`
   (equivalente `IBIS_MCP_URL`, documentato nel README plugin).
4. **P4.3 Hook `sessionStart`**: nuova sessione agent → il contratto Ibis è
   iniettato come `additionalContext` (evidenza: log **GitHub Copilot Chat
   Hooks** in `%APPDATA%\Code\logs\…\exthost\GitHub.copilot-chat\` o risposta
   dell'agente che dichiara di aver caricato il contesto). Contratto verificato
   in Parte A (solo `additionalContext` consumato).
5. **P4.4 Hook `agentStop`**: turno con editing senza catture → **una**
   continuazione forzata con il promemoria `capture_observation`/`memory_add`;
   turno con cattura già fatta → stop normale; secondo stop consecutivo senza
   `stop_hook_active` violato → nessun loop (guard verificata in Parte A).
   Payload e decision control conformi alla reference (F2.3 chiuso).
6. **P4.5 Agenti**: `@ibis-forensics` risponde con **Finding / Evidence /
   Confidence** usando solo i tool read-only del frontmatter;
   `@ibis-curator` disponibile per curation.
7. **P4.6 Automations**: dichiarazioni `nightly-ingest` (cron `0 3 * * *`) e
   `weekly-consolidate` (cron ven 10:30) visibili; l'esecuzione schedulata è
   fuori scope del gate (osservazione successiva su uso reale).

## Publish gate (dopo R3/R4 PASS)

1. Azioni manuali ② ③ completate (Trusted Publishing, token Open VSX → secret
   `OPEN_VSX_TOKEN`).
2. `git push origin main` + **dispatch** di `.github/workflows/publish-vscode.yml`
   (toggle Marketplace/OpenVSX/registry): packaging CI, `vsce publish --oidc`,
   `ovsx publish`, `mcp-publisher login github-oidc && publish` dal root
   `server.json`. Al primo run: fissare la versione del CLI publisher
   (oggi install script da `main`) e verificare i listing su Marketplace,
   Open VSX e registry MCP.
3. Creare/pushare `marketplace/` come repo **`fatterdev/ibis-marketplace`**
   (azione manuale, come da nota nel README dello scaffold) → verificare il
   deep link `vscode://chat-plugin/install?source=fatterdev/ibis-marketplace&plugin=ibis`
   e lo snippet team `.github/copilot/settings.json`.
4. Badges VSIX README (install MCP server) e plugin (install plugin) cliccabili.
5. Chiusura gate: aggiornare PLAN.md (header + §10) e rilasciare il root
   `[Unreleased]` accumulato come `0.3.0`? **No** — la VSIX resta `1.0.0`
   (già pubblicata così) e il runtime resta `0.2.0`: il root `[Unreleased]`
   si rilascia al prossimo tag runtime secondo `PLAN.md §10`.

## Esito R3/R4

| Item | Parte A (automatizzata) | Parte B (VS Code 1.144, owner) |
|---|---|---|
| Plugin scaffold + hook contract | **26/26 PASS** (30/10/2026) | P4.1–P4.6: ☐ |
| VSIX packaging + contributions | 4/4 PASS | R3.1–R3.8: ☐ |
| Publish pipeline | manifest + badge verificati | ☐ (post ②③⑤) |

**Stato: PARTIE A COMPLETA (26/26), PARTIE B in attesa di esecuzione owner**
(azioni ② ③ ⑤). Registrare qui le evidenze (screenshot/log) come fatto per R2.