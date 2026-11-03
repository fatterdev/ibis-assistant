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

> **Automazione disponibile**: `node scripts/ui-test/run-ui-tests.mjs`
> (guida in [scripts/ui-test/README.md](../../scripts/ui-test/README.md)) —
> genera i task paste-ready per **UI-TARS Desktop** (computer-use, winget
> `ByteDance.UI-TARS` v0.2.4, `%LOCALAPPDATA%\UiTars\UI-TARS.exe`, con VLM
> configurato nelle sue Settings) da `scripts/ui-test/tasks.json`
> (R3.1–R3.8 + P4.1–P4.6), raccoglie le evidenze in
> `artifacts/ui-tests/<run>/` e produce `results.json`/`results.md`
> pronti per questa checklist. Usabile anche senza agente: il prompt di
> ogni task guida l'owner passo-passo.

Pre-flight: binario `ibis` sul PATH (o `ibis.runtime.binaryPath`), Ollama
attivo su `11434` con `nomic-embed-text` (serve a `memory_search` nella tree
view), account VS Code con Copilot agent mode.

### R3.1 — Installazione e attivazione

```powershell
code --install-extension C:\devsrc\ibis-assistant\vscode-extension\ibis-assistant-1.0.1.vsix
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
- Espandi `decision` → voci da MCP `memory_list` (metadata browse per kind,
  no embedding; aggiornato 03/11: l'implementazione F1 usa `memory_list`
  limit 100 — la descrizione originale "memory_search con query semantica" era
  datata); descrizione con status/visibilità/tag.
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

1. In chat (modalità agent) → **Add Context / Add Instructions** → cerca
   `ibis-session-contract` → **Attach** (aggiornato 03/11: le `chatInstructions`
   non vivono nel menu `/`, che lista prompt/skill).
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
| Plugin scaffold + hook contract | **26/26 PASS** (30/10/2026) | P4.1–P4.6 ✅ (03/11/2026, con note — vedi sotto) |
| VSIX packaging + contributions | 4/4 PASS | R3.1–R3.8 ✅ (03/11/2026) |
| Publish pipeline | manifest + badge verificati | ☐ (post ②③⑤) |

**Avanzamento Parte B (30/10/2026, run `artifacts/ui-tests/202610301404-r3r4`):**
- **R3.1 PASS** — attivazione letta dal canale Output: `activated (ide-host:
  vendor, engine: 1.144.0)`; LM bridge :39201 listening; MCP provider
  registered; la riga info vendored-fallback è attesa per design. Evidenze:
  log owner + 2 screenshot.
- **R3.2 PASS** — click sull'item di status bar → toast Ibis Assistant: Status
  con `Backend: spawn ibis -mode stdio -runtime-mode plugin`, workspace
  bindato, LM bridge listening, MCP provider registered, memory default
  ask/team. Evidenze: toast owner + screenshot.
- **R3.3 PASS (chiusura 03/11/2026)** — run owner concluso (frasi B/C su
  build 1.0.1, cronometro; 1.0.1 = numerino sul tree parent + refresh con
  spinner, zero processi orfani alla chiusura VS Code — job object ok).
  Frase B: espansione kind via `memory_list` — raffiche `5× memory_list`
  nel plugin.log (07:10:03–13 UTC, 5 kind espansi) tutte ok; warm
  **~1–10ms** runtime (pattern di arrivo) e **1.5–1.8ms** end-to-end sulle
  build strumentate 1.1.x. Frase C: `memory_expand` ok (dettaglio con
  metadati). Search Memory + 5 click refresh (07:07:23–30): refetch reale
  confermato, difetto era solo di feedback → chiuso in 1.0.1 (spinner +
  numerino). Cold: **FALLITO per boot tax** (spawn→ready 1.4–4.9s con Ollama
  su, ~17s con Ollama giù — SurrealDB 1–4s domina): non è un difetto del
  path di query → **decisione owner** sulle 3 opzioni dell'[analisi tempi
  §5](./2026-11-03-s13-timing-analysis.md). Numeri e decomposizione
  completi: [analisi S1.3](./2026-11-03-s13-timing-analysis.md).
  **Gap evidenze**: la cartella `artifacts/ui-tests/<run>-r33/` NON è stata
  persistita dal run (gap §6.4 dell'analisi) — la chiusura si basa su
  `plugin.log` + i numeri delle build strumentate 1.1.x; per i run
  successivi salvare il canale Output su file.
  Storia del finding (30/10): la prima espansione incassava
  `error: timeout waiting for MCP response id=1` (inizializzazione del
  plugin store > 15s) → **risolto** con timeout configurabile 60s in
  ide-host (`9124c84`); seconda attesa su id=2 ricondotta al **binario
  stale** su PATH (`C:\devbin\cargo\bin\ibis.exe` 0.1.0 pre-fix
  BatchManager, log loop in plugin.log) → ricostruito (S1.2, `ibis 0.2.0`).
- Contesto macchina: engine locale Strix (`C:\devbin\paoai-strix-engine`,
  servizio `StrixUITars` :8082 con UI-TARS-7B-DPO per il computer-use) e runner
  `scripts/ui-test/run-ui-tests.mjs` (`screenshot`, `mark`, `collect`).

**Stato: PARTE A COMPLETA (26/26), PARTE B COMPLETA (03/11/2026) — R3.1–R3.8 e
P4.1–P4.6 PASS; resta il publish gate (azioni ②③⑤, owner).** Registrare qui le
evidenze (screenshot/log) come fatto per R2.

**Aggiornamento run 03/11/2026 (pomeriggio) — Parte B completa, run interattivo
agente+owner con evidenza da log** (`%LOCALAPPDATA%\ibis\ui-tests\output-channel.log`
+ `plugin.log`; VSIX 1.0.11, runtime 0.2.0 ricompilato con log esiti):
- **R3.4 PASS** — `memory_add (selection crates/ibis-core/src/claims.rs:55-68,
  decision, team, source_hash=sha256:9e561ea0…): created`; albero aggiornata
  con refresh manuale, poi auto-refresh atterrato in 1.0.7.
- **R3.5 PASS** — `memory_add (file crates/ibis-core/src/config.rs, decision,
  team, source_hash=sha256:70f4901b…): created`; auto-refresh scattato da solo
  1ms dopo il capture (fix 1.0.7 verificato).
- **R3.6 PASS** (dopo due tentativi) — surface corretta: **Add Instructions**
  in chat, NON il menu `/` (testo R3.6 sopra corretto). Turno agente:
  `MCP Tool Call: load_session_context` **1 sola** nel plugin.log, contesto
  reale restituito (3 memorie seed), zero martellamenti `memory_*`.
  Il primo tentativo era servito da una definition MCP **stale**: un
  `.mcp.json` di radice (artefatto smoke R2, gitignored, `dist\ibis.exe` +
  `IBIS_DATA_DIR=.r2-smoke-data`) rispondeva al posto del provider VSIX →
  contesto "vuoto". Rimosso; unica surface restante = provider "Ibis" VSIX.
- **R3.7 PASS** — walkthrough invisibile in 1.0.9 per step senza `media` (i
  testi erano in una chiave `markdown` inline, schema VS Code richiede
  `media.markdown` = path a file): fix in 1.0.10 con `walkthrough/*.md`;
  step + completion events verificati (screenshot). Nota: lo smoke Parte A
  verifica la struttura del manifest, non l'accettazione di VS Code (gap
  registrato).
- **R3.8 PASS** — server provider in chat: `get_working_state` eseguito con
  **zero conferme** (ok in 116µs), `memory_add` con **conferma** (accettata
  durante il test: memoria "fact di prova" ora nello store, kind fact —
  candidata alla pulizia con `memory_delete`); settings `ibis.runtime.*`
  ricomputano senza prompt di reload.
- **P4.1 PASS** (nota UX) — `/ibis` mostra le 5 skill; `/ibis status`
  risponde ma finisce nel canale Output: la skill è progettata per riassumere
  in chat → follow-up UX (chat-first o toast-first).
- **P4.2 PASS** — `ibis (Agent Plugin)` running (già avviato, restart
  silenzioso — atteso).
- **P4.3 PASS** — hook `sessionStart`: contratto iniettato; l'agente cita le
  note di team e **applica l'igiene** (rifiuta di trattare la memoria di
  prova come fatto di progetto). Il log hooks alla path documentata
  (`exthost\GitHub.copilot-chat\`) non esiste in 1.144 → evidenza chat.
- **P4.4 PASS** — turno senza catture: nessuna chiamata Ibis, hook gestito;
  turno con cattura: `memory_search` (dedup-before-add, igiene) →
  `memory_add` ok → stop normale, zero loop. Bonus: **Memory Curator MCP App
  renderizzato inline in chat** (F0.7 in uso reale).
- **P4.5 PASS** (dopo fix) — superficie corretta: **mode picker**, non menu
  `@`. Prima compilazione: i frontmatter usavano il formato Claude
  (`mcp__ibis__*`), VS Code vuole i riferimenti qualificati `<server>/<tool>`
  → zero tool disponibili (finding: "Unavailable tools are ignored"). Fix
  `fix(plugin): copilot-qualified tool refs in agent frontmatter` (7c9909a);
  retry: Finding/Evidence/Confidence con evidenza reale (`load_session_context`,
  `memory_search`, `why_changed`, `ask_project` — tutti read-only nel log) e
  ricostruzione autonoma della storia del pre-warm (1.0.2 → fix 1.0.3).
  Follow-up: nota brevità narrativa (431253f).
- **P4.6 PASS** — dettaglio plugin: Agents 2, Skills 5, MCP 1, Hooks 2,
  **Automations 2** con nome+descrizione ("Nightly Ibis ingest",
  "Weekly Ibis memory consolidation"). Esecuzione schedulata fuori scope.
- **Finding ambientali (non nostri, registrati)**: il rumore "Warning from
  Post-Tool Use hook" viene dall'estensione **Azure Tools**
  (`ms-azuretools.vscode-azure-github-copilot`): `resources/hooks/local.json`
  registra SessionStart+PostToolUse con path **workspace-relativi**
  (`./.github/hooks/azureSkills/scripts/track-telemetry.ps1`) che non esiste
  nel repo → PowerShell fallisce a ogni tool call (global.json usa path
  assoluti e funziona). Opzioni owner: ignorare / disabilitare
  l'estensione nel workspace / issue upstream.
- **Anomalie performance da registrare**: `ask_project` end-to-end 34.4s e
  18.0s (gamba reasoning + embed); pre-warm boot 12.2–17.8s con Ollama giù
  (auto-start nel job del child, muore con l'idle-close) e **3.3–4.7s con
  Ollama su**; boot più pesante della misura S1.3 (schema init ~2.8s +
  migrations ~2s ricorrenti per boot, contro schema 0.30s misurato in S1.3) —
  decomposizione nel plugin.log del run. Il target cold resta chiuso dal
  pre-warm (prima fetch 12–30ms).

**Aggiornamento S1 (01/11/2026) — chiusura debug "blocchi e lentezze":**
- **S1.1 done** — client stdio persistente in ide-host (sessione per backend
  identity, handshake pagato una volta, idle self-close 120s, retry su child
  morto): le interazioni VSIX non pagano più il boot del child per chiamata.
  Ripacchettata `ibis-assistant-1.0.0.vsix` (58.46 KB), `smoke-plugin.mjs`
  26/26, `session_test.mjs` 3/3 in CI. R3.3 va riverificato con QUESTA build.
- **S1.2 done (igiene ambiente owner)** — `chat.pluginLocations` corretto dal
  percorso pre-revert (morto) a
  `C:\devsrc\ibis-assistant\plugin` (il plugin non si caricava da sorgente →
  P4.x bloccati); binario Go stale `ibis-assistant.exe` (18/09) non più
  risolvibile su PATH (`where.exe` vuoto; la dir `go\bin` è sparita durante
  la sessione per causa esterna — da riverificare a freddo che non torni);
  `debug-353a81.log` archiviato in `artifacts/debug/` (era alla root, evidence
  del vecchio runtime Go lanciato alle 19:56 del 30/10 con CLI `-mode stdio`).
- **S1.3 done (misura 03/11, run owner)** — frasi B/C riverificate su build
  con client stdio persistente + fix 1.0.1; numeri: warm **PASS ampio**
  (`memory_list` ~1–10ms, `memory_search` warm 1.5–1.8ms), cold **FALLITO
  per boot tax** (1.4–4.9s con Ollama su, ~17s giù — SurrealDB 1–4s + check
  ~1.2s + probe). Decomposizione e 3 opzioni di rimozione boot tax:
  [2026-11-03-s13-timing-analysis.md](./2026-11-03-s13-timing-analysis.md).
  **S1.4 done (03/11, post-verdetto, `e9b85b8`)**: timing `memory_search`
  demotati info→debug (tornano con `log_level DEBUG`) + righe nuove a info
  `memory_list: done in …` / `memory_expand: done in …` (parità di evidenza
  frasi B/C, analisi §6.3). **Verdetto cold target (03/11): opzione 1
  pre-warm** — atterrato in VSIX **1.0.2** (`bf824b1`): fire-and-forget in
  `activate()` (sessione persistente + handshake + null call
  `ticket_get_capabilities`); evidenza attesa sul canale Output
  (`pre-warm: runtime session ready in Nms`) e prima espansione warm-like —
  da verificare nel prossimo run owner insieme a R3.4–R3.8.
  Runbook passo-passo owner (failure-first, pre-flight, matrice guasti,
  evidenze): [2026-11-03-r33-closure-runbook.md](./2026-11-03-r33-closure-runbook.md).
  Seed albero (3 memorie, 02/11) via `scripts/seed-memory-r33.mjs` — fatto:
  `memory_search` runtime 22ms cold / 1–3ms warm su Ollama reale; la misura
  mancante è l'espansione via VSIX (S1.1).
**Build debug 1.0.1 (03/11)** — riscontri owner della sessione debug:
  **numerino sul tree parent** (conteggio voci caricate per kind/search,
  `N+` oltre il cap; re-render solo su cambio label — guard convergente
  contro il fire-loop) e **bottone refresh con spinner** (il refetch
  avveniva già, il difetto era solo di feedback). Le build provvisorie
  1.1.0–1.1.2 (strumentazione S1.3 lato VSIX/ide-host: spawn/cold-init/
  reuse/timing per tool call nel canale Output) sono state **mai committate
  e ritirate a fine sessione debug** su decisione owner: la strumentazione
  non è in 1.0.1; i numeri raccolti vivono nell'[analisi tempi
  S1.3](./2026-11-03-s13-timing-analysis.md). Numerazione build debug: patch
  (`1.0.x`), `1.1.0` riservato a F4. `ibis-assistant-1.0.1.vsix` (59.33 KB,
  17 file), smoke 26/26, session_test 3/3. Nota corretta su R3.3:
  l'espansione kind usa **memory_list** (metadata, no embedding), NON
  memory_search — la riga runtime `memory_search:` nel plugin.log compare
  solo con il comando Search Memory.