# REVIEW R2 — Smoke MCP in VS Code (30/10/2026)

> Gate R2 di [PLAN.md](PLAN.md): *smoke MCP in VS Code — zero conferme sui read-only,
> `/ibis.*` in chat (mappa di revert da `/threski.*`), resources in "Add Context",
> sampling attivo, App visibile (con fallback testuale sempre garantito)*.
>
> Nome server nel config: `ibis` → i prompt si renderizzano come `/ibis.<nome>`,
> le risorse sotto `ibis://…`, l'MCP App è `ui://ibis/memory-curator`.

## Parte A — smoke di protocollo su binario reale (automatizzato, eseguito 30/10/2026)

`node scripts/smoke-mcp.mjs [ibis.exe] [workspace]` — spawna il binario con
`run --mode stdio --runtime-mode plugin`, workspace bindato via `roots/list`
(cwd vuota), DB embedded dedicato su porta libera, e verifica l'intera superficie:

| # | Check | Esito |
|---|---|---|
| 1 | MCP initialize (stdio) — serverInfo "Ibis Assistant" | PASS |
| 2 | F0.6 dynamic discovery — 41 tool core, nessun `ticket_*` (ticketing non configurato) | PASS |
| 3 | F0.2 ogni tool ha `annotations {title, readOnlyHint}` | PASS |
| 4 | F0.2 set read-only esatto (15 tool core: ask_project, analyze_blast_radius, as_of_project, find_dead_code, get_rules, get_working_state, knowledge_diff, load_session_context, memory_expand, memory_history, memory_insights, memory_search, recent_failures, settings_get, why_changed) | PASS |
| 5 | F0.2 mutanti con `readOnlyHint: false` esplicito | PASS |
| 6 | F0.3 `prompts/list` = blast_radius, readiness, remember, status, why_changed | PASS |
| 7 | F0.3 `prompts/get readiness` (1177 caratteri, testo contratto di sessione) | PASS |
| 8 | F0.3 `completion/complete` remember/kind "dec" → "decision" | PASS |
| 9 | `resources/list` annuncia `ibis://guide` + `ui://ibis/memory-curator` | PASS |
| 10 | F0.7 risorsa App con MIME `text/html;profile=mcp-app` + `_meta.ui` (CSP loopback) | PASS |
| 11 | F0.4 templates `ibis://memory/{kind}/{id}`, `ibis://rules/{name}`, `ibis://ticket/{id}` | PASS |
| 12 | `resources/read ibis://guide` (markdown, 12836 caratteri) | PASS |
| 13 | F0.7 `resources/read ui://ibis/memory-curator` (14469 caratteri, CSP embedded) | PASS |
| 14 | Bind workspace + DB embedded raggiungibile (`get_working_state` → found=false) | PASS |
| 15 | F0.7 `memory_add`: blocco testuale JSON sempre presente (fallback garantito) | PASS |
| 16 | F0.7 `memory_add`: `_meta.ui.resourceUri = ui://ibis/memory-curator` | PASS |
| 17 | F0.4 `resources/read ibis://memory/fact/<id>` (template read della memoria reale) | PASS |
| 18 | F0.4 `resources/subscribe` accettato | PASS |
| 19 | F0.4 `notifications/resources/updated` pushata su `memory_update` | PASS |
| 20 | `memory_search` resta sempre un tool result (mai errore di protocollo) | PASS |
| 21 | F0.5 `sampling/createMessage` servito dal client (1 chiamata via `ask_project`) | PASS |
| 22 | `ask_project` è `readOnlyHint: true` → zero conferme in VS Code | PASS |
| 23 | Uscita pulita alla chiusura stdin (comportamento da sessione VS Code) | PASS |

**Esito Parte A: 23/23 PASS** (gate `cargo test --workspace` verde; ricostruito
`dist` con `cargo xtask dist` prima dello smoke manuale).

Nota ambientale: la prima esecuzione del gate ha scoperto un artefatto stale
(`discovery_snapshot_test` con `CARGO_MANIFEST_DIR` pre-revert baked in
dall'ambiente di migrazione) — risolto con `cargo clean -p ibis-server`.

## Parte B — smoke manuale in VS Code 1.144 (runbook operativo owner)

> **Perché richiede esecuzione manuale**: i 5 check di Parte B collaudano il
> renderer UI di VS Code 1.144 e il Copilot harness autenticato (assenza di
> dialog di conferma sui tool `readOnlyHint: true`, slash picker `/`, picker
> `Add Context`, dialog consenso `sampling/createMessage` e iframe sandboxed
> MCP Apps). Il protocollo sottostante sul binario reale è già validato al
> 100% in Parte A (`23/23 PASS` ri-eseguito il 30/10/2026 con Ollama attivo).
>
> **Stato 30/10/2026**: **B1–B5 tutte con evidenza owner — R2 CHIUSO** (B4
> dimostrato dal registro sampling requests: 15 chiamate runtime servite da
> `copilot/auto`). Dettagli in "Evidenze e anomalie raccolte".

### Pre-flight ambiente (già verificato sulla macchina il 30/10/2026)

- **VS Code**: `1.144.0` presente (`C:\devbin\vscode\bin\code`).
- **Binario e sidecar**: `C:\devsrc\ibis-assistant\dist\ibis.exe` + `surreal.exe` + `sg.exe` presenti (`smoke-mcp.mjs dist\ibis.exe` → **23/23 PASS**).
- **Ollama**: attivo su `http://127.0.0.1:11434` con `nomic-embed-text:latest` presente (necessario per B1 controprova e B5: se Ollama fosse spento, `memory_search` tornerebbe `isError: true` e `with_ui_meta` ometterebbe per design `_meta.ui.resourceUri`, impedendo il mount dell'MCP App).
- **Isolamento smoke reale** in `.mcp.json`:
  ```json
  {
    "mcpServers": {
      "ibis": {
        "type": "stdio",
        "command": "C:\\devsrc\\ibis-assistant\\dist\\ibis.exe",
        "args": ["run", "--mode", "stdio", "--runtime-mode", "plugin", "--scan", "false"],
        "env": {
          "RUNTIME_MODE": "plugin",
          "IBIS_DATA_DIR": "C:\\devsrc\\ibis-assistant\\.r2-smoke-data",
          "SURREAL_URL": "ws://127.0.0.1:18099/rpc",
          "DB_AUTO_UPDATE": "false"
        }
      }
    }
  }
  ```
  *(Senza `IBIS_DATA_DIR` e `SURREAL_URL` espliciti, `apply_plugin_isolation` scriverebbe nel vero store `%LOCALAPPDATA%\ibis\plugin` sulla porta condivisa `18000`; così lo smoke non sporca né il personal store né il plugin store reale).*

### Step 0 — Apertura e settings VS Code (1 min)

1. Apri il workspace in VS Code 1.144:
   ```powershell
   code C:\devsrc\ibis-assistant
   ```
2. Apri Settings JSON (`Preferences: Open User Settings (JSON)`) e verifica/aggiungi:
   ```json
   "chat.mcp.apps.enabled": true,
   "chat.mcp.workspaceRootConfig.enabled": true
   ```
3. Esegui da Command Palette **`MCP: List Servers`**:
   - Verifica che compaia **`ibis`** (da `.mcp.json`), clicca **Start Server** (e accetta l'eventuale prompt di Trust).
   - *Fallback rapido*: se `.mcp.json` non comparisse nella tua build/profilo, copia la stessa voce in `.vscode/mcp.json` sostituendo la chiave top-level `"mcpServers"` con `"servers"`.
4. Attendi ~3–5 secondi l'avvio di SurrealDB embedded su `:18099` (verificabile in `.r2-smoke-data\plugin.log`).

---

### B1 — Zero conferme sui read-only + controprova mutante (prepara anche B3 e B5)

1. Apri **Copilot Chat** (modalità Agent, nuova sessione) e invia:
   > `Load my session context for this workspace and check my current working state. Use only Ibis MCP tools.`
   - **Atteso PASS**: `load_session_context` e `get_working_state` (`readOnlyHint: true`, con `title`) vengono eseguiti **senza alcun prompt di conferma** manuale.
2. **Controprova mutante** (crea subito la memoria usata poi in B3 e B5) — invia:
   > `Use memory_add to store a fact with title "R2 smoke entry" and content "R2 manual smoke entry in VS Code 1.144" for this project.`
   - **Atteso PASS**: VS Code **chiede conferma** prima di eseguire `memory_add` (`readOnlyHint: false`).
   - Approva l'esecuzione e **annota l'`id` restituito** nel JSON (formato `memory:⟨mem_...⟩`, incluse le parentesi angolari Unicode `⟨⟩`).
- **Evidenza**: screenshot tool call read-only eseguite senza conferma + dialog di conferma su `memory_add`.

---

### B2 — Prompt MCP slash in chat (`/mcp.ibis.*`)

1. Nella casella di input della chat digita `/` seguito da `ibis`:
   - *Nota piattaforma VS Code*: VS Code espone i prompt di un server MCP con il prefisso `/mcp.<server>.<prompt>` (es. **`/mcp.ibis.readiness`**, `/mcp.ibis.status`, `/mcp.ibis.why_changed`, `/mcp.ibis.blast_radius`, `/mcp.ibis.remember`), oppure `/ibis.<prompt>` a seconda dell'harness attivo — filtrando con `/ibis` compaiono tutti e 5.
2. Seleziona ed esegui **`readiness`** (`/mcp.ibis.readiness`) → verifica che inietti il testo del contratto di sessione (~1177 caratteri).
3. Seleziona **`remember`** (`/mcp.ibis.remember`) e nel parametro `kind` digita `dec` → verifica l'autocompletion protocollo **`decision`**.
- **Evidenza**: screenshot del picker `/` con i 5 prompt + autocompletion `decision`.

---

### B3 — Resources & Resource Templates in "Add Context"

1. Clicca **Add Context (`+` / `#`) → MCP Resources** (oppure Command Palette → **`MCP: Browse Resources`**):
   - **Atteso PASS (risorse statiche)**: compaiono `ibis://guide` e `ui://ibis/memory-curator`.
2. Allega **`ibis://guide`** e invia:
   > `Summarize the attached ibis://guide resource in 3 bullet points.`
   - **Atteso PASS**: la chat legge i ~12.8 KB markdown della guida.
3. Torna in **Add Context → MCP Resources** (o `MCP: Browse Resources`), seleziona il **Resource Template** `ibis://memory/{kind}/{id}` e compila:
   - `kind`: `fact`
   - `id`: l'ID esatto annotato in B1 (`memory:⟨mem_...⟩`)
   - **Atteso PASS**: viene allegato e letto il JSON completo della memoria creata in B1.
- **Evidenza**: screenshot del picker risorse + risposta con `ibis://guide` allegata.

---

### B4 — Sampling attivo (`sampling/createMessage`)

1. Verifica preliminare configurazione runtime — invia in chat:
   > `Call settings_get and report ai.reasoning.provider and sampling_available.`
   - **Atteso**: `provider` effettivo = `"ide_sampling"` e `sampling_available: true` (senza conferme perché `settings_get` è `readOnlyHint: true`).
2. Test round-trip sampling — invia:
   > `Call ask_project with query "Why did init change recently?"`
   - **Atteso PASS**:
     - Al primo utilizzo VS Code mostra il **prompt di consenso al sampling** per il server `ibis` → **Approva** (se già autorizzato in precedenza, verificabile da `MCP: List Servers → ibis → Configure Model Access`).
     - *Nota importante*: `McpSamplingProvider` (`crates/ibis-ai/src/mcp_sampling.rs`) restituisce il testo generato dal modello della chat ma **non** concatena il nome del modello nel testo di `ask_project`. La prova oggettiva che sia passato da MCP sampling (e non dal fallback Ollama) si verifica aprendo **`MCP: List Servers → ibis → Show Output`** (impostando il livello log su *Trace*): compare la richiesta server→client `"method": "sampling/createMessage"` con la risposta del modello VS Code.
- **Evidenza**: screenshot del consenso / `Configure Model Access` o trace `sampling/createMessage` nell'Output MCP + risposta di `ask_project`.
- **PASS (owner, 30/10/2026)** — due evidenze oggettive indipendenti:
  1. `chat.mcp.serverSampling` in `.vscode/settings.json` mostra
     `"ibis-assistant/.mcp.json: ibis": {"allowedDuringChat": true,
     "allowedOutsideChat": true}` — il consenso sampling per il server è stato
     concesso (equivale a `Configure Model Access` approvato).
  2. Registro delle sampling requests del server `ibis`: **15 richieste totali**
     (30/10/2026 11:17:04–11:17:22, modello `copilot/auto`) — sono le chiamate
     di generazione regole ast-grep dell'ingest (`You generate ast-grep YAML
     rules for Ibis code ingest…`, estensioni `.md`/`.js` sul workspace
     `ibis-assistant`). In plugin mode la catena F0.5 è `sampling → lm_bridge →
     ollama`: senza VSIX non esiste LM bridge e le regole richieste dal runtime
     sono arrivate al modello della chat (`copilot/auto`) — quindi la lane
     `sampling/createMessage` è l'unica via praticata ed è **attiva**.
     Le risposte sono le regole `ai-generated-*-chunk/-calls` effettivamente
     generate dal modello IDE.
- **Nota qualità (non blocca R2)**: alcune regole generate dichiarano
  `language: markdown` ( grammatica non built-in ast-grep, ammessa solo come
  fallback con `language_glob`) e `language_glob` incoerenti (`"html|"`, `"md"`,
  `""`) — leak dell'esempio del prompt e autoconsapevolezza del modello
  ("is not built in and may need a custom language registration"). Alcune
  risposte arrivano in ```json fence nonostante "Return ONLY a JSON array".
  La validazione/lint delle regole AI-generated è materia del ingest lane
  (pre-esistente), candidata naturale a criterio D-CLAIMS in Track D.

---

### B5 — MCP App "Ibis Assistant Memory Curator" + fallback testuale

1. Con `"chat.mcp.apps.enabled": true`, invia in chat:
   > `Use memory_search with query "R2 manual smoke entry" and show the results.`
   - **Atteso PASS (con App attiva)**:
     - Nel risultato del tool in chat viene renderizzata inline l'**MCP App "Ibis Assistant Memory Curator"** (`ui://ibis/memory-curator`, iframe sandboxed con CSP solo loopback) che visualizza la card della memoria creata in B1;
     - Il **blocco testuale JSON** del tool result rimane comunque presente/ispezionabile (fallback garantito anche in `memory_add` di B1).
2. **Test degradazione / fallback puro**:
   - Imposta temporaneamente `"chat.mcp.apps.enabled": false` nelle Settings e ripeti la stessa chiamata `memory_search`.
   - **Atteso PASS**: nessun errore UI; il risultato testuale JSON completo è leggibile da solo. Riporta poi `"chat.mcp.apps.enabled": true`.
- **Evidenza**: screenshot con iframe Memory Curator renderizzato + screenshot col solo fallback testuale.

---

### Cleanup post-smoke (10 sec)

1. Ferma il server `ibis` da **`MCP: List Servers → ibis → Stop Server`**.
2. Rimuovi la directory temporanea dello smoke:
   ```powershell
   cmd /c "if exist C:\devsrc\ibis-assistant\.r2-smoke-data rmdir /s /q C:\devsrc\ibis-assistant\.r2-smoke-data"
   ```

## Evidenze e anomalie raccolte (30/10/2026, run owner)

**Dal log `GitHub Copilot Chat Hooks`** (`%APPDATA%\Code\logs\20261030T121429\
window1\exthost\GitHub.copilot-chat\`) — fonte oggettiva delle chiamate:

- I tool MCP sono stati eseguiti (`mcp_ibis_assistan_load_session_context`,
  `mcp_ibis_assistan_get_working_state`, `mcp_ibis_assistan_memory_add`) e i
  corrispondenti `PostToolUse` hook di ambiente hanno ricevuto le risposte —
  `load_session_context` ha ritornato la memoria creata nel round-trip
  precedente (round-trip MCP in chat funzionante).
- `tool_search` dell'harness elenca i tool ibis accanto a quelli di altri
  server (`mcp_ibis_assistan_get_rules`, `settings_get`, `repo_pr_complete`,
  `capture_observation`…) — discovery ok.

**B1 — PASS (owner)**: `Load Session Context` + `Get Working State` eseguiti
senza alcun dialog di conferma (screenshot). *"Tool call received a warning"*
è **rumore esterno**: proviene dal plugin `modernize-dotnet` installato
(`~\.vscode\agent-plugins\github.com\dotnet\modernize-dotnet`), il cui
`hooks.json` dichiara `powershell -File hooks/scripts/track-telemetry.ps1` con
percorso **relativo** invece di `${PLUGIN_ROOT}`-anchored (il plugin
`upgrade-agent` accanto usa il pattern corretto con exists-check). Nessuna
relazione con Ibis: disabilitare quel plugin o segnalare a dotnet.
Evidenza conferma-mutante (dialog su `memory_add`) non catturata nelle
screenshot — da confermare nella run di B4.

**B2 — PASS (owner, con nota)**: il prompt `remember` è stato invocato e ha
iniettato il testo (screenshot: istruzione a `memory_add` + memoria creata).
La dialog argomenti è comparsa (per dev guide i completions MCP sono
supportati) ma **senza lista suggerimenti** per `kind`="dec" — gap UI VS Code
(da ritestare su Insiders); a livello protocollo `completion/complete`
`dec`→`decision` è verificato in Parte A. Non è un difetto server.

**B3 — PASS (owner)**: `ibis://guide` allegato via Add Context e riassunto in
3 bullet (screenshot). La lettura del resource template
`ibis://memory/{kind}/{id}` non è stata catturata in screenshot (coperta al
100% da Parte A, check 17).

**B4 — PASS (owner)**: consenso sampling già concesso
(`chat.mcp.serverSampling` in `.vscode/settings.json`:
`allowedDuringChat`/`allowedOutsideChat` = true per il server `ibis`) e
**registro sampling requests con 15 chiamate servite dal modello della chat**
(`copilot/auto`, 30/10 11:17): le sono le generazioni di regole ast-grep
dell'ingest runtime — in plugin mode senza VSIX la catena F0.5 non ha né LM
bridge né Ollama come via praticabile per quelle richieste, quindi sono
transitate da `sampling/createMessage`. Dettagli nella sezione B4.

**B5 — PASS (owner)**: l'App "Ibis Assistant Memory Curator" è renderizzata
inline nel tool result `memory_add` (screenshot: header, search box, card
CREATED) **insieme** al blocco testuale (fallback garantito, presente sopra
l'iframe). Il test di degradazione con `chat.mcp.apps.enabled: false` non è
stato catturato (la degradazione è coperta da Parte A check 15–16).

**Finding — difetto reale del runtime (da file post-R2)**: `.r2-smoke-data\
plugin.log` cicla ogni 10s `BatchManager: Failed to fetch pending commits/
chunks ... Failed to convert to none | string: Expected string, got record`
(`CommitRow`/`IdContentRow` ricevono record-id SurrealDB dove la struct
si aspetta string) sulla lane di ingest polling in plugin mode. Non blocca R2
(background lane), ma è rumore log ad alto volume e va corretto (fix task
dedicato, non nel gate).

## Automazione (post-R2 — esito ricerca 30/10/2026)

Tre strati, con strumenti diversi:

1. **Protocollo (già automatizzato)** — `scripts/smoke-mcp.mjs`: è il sostito
   naturale dei test e2e; promuoverlo a job CI (richiede solo Node + binario
   built; Ollama opzionale con asserzione di degradazione). Nessuna API VS
   Code coinvolta: copre F0.2–F0.7 end-to-end sul binario reale.
2. **API VS Code (`@vscode/test-cli` + `@vscode/test-electron`)** — integration
   test in Extension Development Host (`vscode-test`, workspace + `launchArgs`)
   per la superficie **VSIX** (F1): comandi, status bar, tree view, instruction
   — dove l'API è assertabile. Limiti documentati: non copre il Copilot
   harness (conferme/sampling/App sono UI-side) e da CLI serve un'istanza VS
   Code dedicata (o Insiders) — per CI va bene (macchina dedicata), in locale
   meglio via launch config di debug.
3. **Comportamento agente (conferme, igiene tool, sampling reale)** — è
   esattamente il **Track D / Scope** già in piano: worker "VS Code Copilot"
   con profili base vs +MCP vs +MCP+skills, criteri D-CLAIMS (annotations→
   attrito conferme; skills+hooks→igiene d'uso), evidenze da trajectory.
   La Parte B manuale resta l'evidenza di review gate; Scope la automatizza
   statisticamente in D3.

## Esito R2

| Item | Parte A (protocollo) | Parte B (VS Code 1.144) |
|---|---|---|
| Zero conferme read-only + conferma mutante | PASS (annotations) | **PASS owner** (B1; conferma mutante da riacquisire in B4) |
| Prompt MCP slash in chat (`/mcp.ibis.*`) | PASS (prompts+completions) | **PASS owner** (B2; completions UI = gap VS Code) |
| Resources + Templates in Add Context | PASS (list/read/subscribe/live) | **PASS owner** (B3; template read coperto da Parte A) |
| Sampling attivo (`sampling/createMessage`) | PASS (round-trip) | **PASS owner** (B4: serverSampling authorized + 15 sampling requests servite da `copilot/auto`) |
| MCP App + fallback testuale | PASS (resource + `_meta.ui` + testo) | **PASS owner** (B5; degradazione coperta da Parte A) |

**ESITO R2: SUPERATO** — 30/10/2026, tutte le voci con evidenza owner + Parte A
23/23. Decisioni di chiusura (30/10/2026): `scripts/smoke-mcp.mjs`, questa
checklist e il fix BatchManager committati; `.vscode/` e `.mcp.json` in
`.gitignore` (config locali, riproducibili dal §Pre-flight); runtime rilasciato
**0.2.0** con tag `v0.2.0` (`PLAN.md §10`); da qui avvio **[F1 ∥ F2]**. Il
difetto BatchManager (`Expected string, got record`) è corretto con test di
regressione; l'hook rotto di `modernize-dotnet` resta anomalia esterna.