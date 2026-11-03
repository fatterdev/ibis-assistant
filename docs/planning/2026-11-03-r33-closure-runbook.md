# Runbook owner — chiusura R3.3 (frasi B/C + misura S1.3)

**Data:** 2026-11-03 · **Per:** azioni manuali WQ-1 (S1.3)
**Principio:** procedura scritta per il fallimento — ogni passo dichiara l'esito
atteso, dove guardare se non arriva, e l'evidenza da salvare. Non proseguire a
occhio: registra.

**Stato macchina verificato 02–03/11 (agente):** PATH
`C:\devbin\cargo\bin\ibis.exe` → `ibis 0.2.0`; binario Go stale non
risolvibile; **VSIX finale `ibis-assistant-1.0.1.vsix` (59.33 KB)** — build
con numerino sul tree parent + refresh con spinner; le build debug
provvisorie 1.1.0–1.1.2 (strumentate per S1.3) sono state ritirate a fine
sessione e la strumentazione è rimossa su decisione owner; Ollama 0.40.11 su
:11434 con `nomic-embed-text`; SurrealDB locale 3.3.5. Seed eseguito: 3
memorie (decision/fix/gotcha) nel data dir plugin, `memory_search` runtime
**22ms cold / 1–3ms warm**. I numeri raccolti con le build strumentate vivono
nell'[analisi tempi S1.3](./2026-11-03-s13-timing-analysis.md).

**Correzione importante (03/11):** l'espansione di un kind (frase B) usa MCP
**`memory_list`** (metadata, no embedding) — NON `memory_search`. Le righe
runtime `memory_search:` nel plugin.log compaiono solo col comando **Search
Memory**. In 1.0.1 (senza strumentazione VSIX) il timing della frase B si
misura con cronometro + screenshot; nelle build 1.1.x debug si leggeva dal
canale Output (tabelle storiche sotto, §Log da cercare).

## Artefatti

| Percorso | Che cosa è |
|---|---|
| `%LOCALAPPDATA%\ibis\plugin\plugin.log` | log runtime del plugin (STDOUT è riservato al MCP JSON-RPC — i log vanno SOLO qui) |
| Canale Output **Ibis Assistant** in VS Code | attivazione + versione build (`ibis: 1.0.1`) + errori VSIX (le righe di timing esistevano solo nelle build debug 1.1.x, ritirate) |
| `scripts/seed-memory-r33.mjs` | seed 3 memorie + misura `memory_search` E `memory_list` (la chiamata della frase B) sullo stesso spawn contract del VSIX |
| `vscode-extension\ibis-assistant-1.0.1.vsix` | build corrente: numerino sul tree parent + refresh con spinner (client stdio persistente S1.1) |
| `artifacts/ui-tests/<run>-r33/` | DOVE salvare le evidenze (crea la cartella col timestamp, es. `20261103-r33`) |

Strumentazione timing nel **plugin.log** (livello `info`, **da NON demuotere
prima del verdetto S1.4**): `memory_search: embedded in …` (embedding query),
`memory_search: db query returned in …` (query SurrealDB),
`memory_search: boosting (elapsed=…)` (merge/boost) — SOLO per il comando
Search Memory.

### Log da cercare nel canale Output (STORICO — valido SOLO per le build debug 1.1.0–1.1.2, ritirate)

Queste righe esistevano nella strumentazione di debug (rimossa in 1.0.1);
restano come riferimento dell'analisi tempi in
[2026-11-03-s13-timing-analysis.md](./2026-11-03-s13-timing-analysis.md).

| Riga | Significato | Cosa dimostrava |
|---|---|---|
| `activated (ide-host: vendor, engine: 1.144.0, ibis: <ver>)` | attivazione + **versione build** | anti-stale (la versione resta visibile anche in 1.0.1) |
| `backend: spawn ibis -mode stdio -runtime-mode plugin (resolved in Xms…)` | backend risolto | spawn mode attivo, costo resolve |
| `child spawn: ibis -mode stdio -runtime-mode plugin pid=N` | child avviato | il boot parte QUI (una volta) |
| `session cold start/cold init in Xms (handshake paid once…)` | prima chiamata della sessione | boot plugin/SurrealDB pagato una volta |
| `session reuse (persistent child, no respawn)` | chiamata successiva | **S1.1 funziona: nessun respawn** |
| `tools/call <tool> in Xms (N chars)` | durata per chiamata MCP | il numero S1.3 per frase B/C |
| `session drop/idle-close after Xms` | morte/idle della sessione | crash o 120s idle attesi |
| `memory tree: kind=<k> via memory_list in Xms, N entries` | fetch frase B (lato view) | target cold <2000ms, warm <500ms |
| `memory tree: search "<q>" via memory_search in Xms, N entries` | Search Memory (lato view) | confronto col plugin.log |
| `memory tree: refresh requested/…/refresh cycle complete` | bottone refresh | il refresh re-fetcha davvero i kind espansi |
| `memory detail: memory_expand <id> in Xms (chain=N)` | frase C | latenza expand |
| `memory_add … in Xms: <json>` | Remember (R3.4+) | latenza cattura |
| `tools/call <tool> FAILED in Xms: …` | errore con timing | diagnosticare subito il fallo |

---

## 0. Pre-flight a freddo (5 comandi, ~2 min)

Esegui e salva l'output in `artifacts/ui-tests/<run>-r33/preflight.txt`:

```powershell
where.exe ibis; ibis --version
where.exe ibis-assistant          # atteso: NON trovato
Get-Item .\vscode-extension\ibis-assistant-1.0.1.vsix | Select Length, LastWriteTime
Test-NetConnection 127.0.0.1 -Port 11434 -InformationLevel Quiet   # atteso: True
ollama list                        # atteso: nomic-embed-text presente
```

| Esito atteso | Se fallisce |
|---|---|
| `ibis 0.2.0` | binario stale su PATH → rebuild (`cargo install --path crates/ibis-server`) e ricontrolla; registra la versione trovata |
| `ibis-assistant` non trovato | se torna, la dir `go\bin` è risalita sul PATH (S1.2 regredita) → rimuovila e registra |
| file `ibis-assistant-1.0.1.vsix` presente (≥ 59 KB, mtime 03/11 08:02) | se manca o il sorgente `vscode-extension/` è più nuovo del .vsix → `npm run package` in `vscode-extension/` |
| porta 11434 True | **failure atteso #1**: avvia Ollama (`ollama serve` o l'app) e ripeti; `ollama pull nomic-embed-text` se il modello manca |
| modello presente | `ollama pull nomic-embed-text` |

Chiudi **VS Code** prima dei passi 1–2 (il runtime del plugin non deve tenere
la SurrealDB su `:18000` del data dir condiviso).

## 1. Seed memorie (l'albero è vuoto finché non esistono)

```powershell
node scripts\seed-memory-r33.mjs | Tee-Object artifacts\ui-tests\<run>-r33\seed.txt
```

- **Atteso:** 13/13 PASS (3 `memory_add` con id `memory:⟨mem_…⟩`, 6
  `memory_search` ok, 2 `memory_list` ok, righe `TIMING | …` — la coppia
  `memory_list cold/warm` è il dato della frase B).
- Già eseguito il 02/11 (le 3 memorie ci sono): NON ripetere — crea
  duplicati. Se serve solo il timing `memory_list`, eseguirlo comunque va
  bene ma registra i duplicati creati.
- **Se fallisce su `memory_add`** con `embedding failed … 11434` → Ollama
  giù (ritorna al passo 0). Con `database not connected` → il DB non è
  salito: guarda `plugin.log` (riga `Starting embedded database …
  bind=127.0.0.1:18000`) — se c'è un processo `ibis`/`surreal` orfano
  (`Get-Process ibis,surreal`), chiudilo e riprova.
- **Se fallisce lo spawn** (`cannot spawn ibis`) → binario non su PATH (passo 0).

## 2. VSIX (re)installata + canale Output

```powershell
code --uninstall-extension fatterdev.ibis-assistant
code --install-extension .\vscode-extension\ibis-assistant-1.0.1.vsix
```

Apri VS Code sul workspace `C:\devsrc\ibis-assistant`, canale Output →
**Ibis Assistant**.

- **Atteso:** `activated (ide-host: vendor, engine: 1.144.0, ibis: 1.0.1)`;
  nessun `ide-host package import failed`. **La riga `ibis: 1.0.1` è la prova
  anti-stale** — se vedi `ibis: 1.0.0` la VSIX vecchia è ancora installata:
  ripeti uninstall/install e ricontrolla la versione nel pannello Estensioni.
- **Evidenza:** screenshot del canale (primi ~10 righe) in
  `<run>-r33/01-output-channel.png`.
- **Se l'attivazione fallisce** → registra il testo esatto del canale +
  `%APPDATA%\Code\logs\<data>\exthost\` (cartella Ibis Assistant) e fermati
  lì: è un difetto VSIX nuovo, non di R3.3.

## 3. Frase B — espansione kind, CON misura (cold/warm)

1. `Ctrl+Shift+P` → **Developer: Reload Window** (reset esplicito: così
   "cold" è davvero cold).
2. Apri un cronometro. Activity bar → **Ibis Assistant** → view **Memory**
   → espandi `decision`. Ferma il cronometro alla comparsa delle voci.
3. Collassa e ri-espandi `decision` (warm). Seconda misura.
4. Ripeti B su `fix` e `gotcha` (una volta sola ciascuno, per conferma).

**Atteso (target S1.3): cold < 2s, warm < 500ms.** Le voci mostrano titolo +
descrizione con status/visibilità/tag. **Il parent del kind mostra il numerino
con quante voci sono caricate** (da 1.0.1: `decision  3`, con `N+` quando il
fetch tocca il cap — richiesta owner 03/11).

**Evidenze (tutte):**
- 2+ screenshot con l'orologio di sistema visibile (o cronometro) in
  `<run>-r33/02-expand-cold.png`, `03-expand-warm.png`;
- solo per il comando **Search Memory** (non per la frase B), il timing
  runtime da plugin.log:

```powershell
Select-String -Path "$env:LOCALAPPDATA\ibis\plugin\plugin.log" -Pattern "memory_search:" | Select-Object -Last 12 | Tee-Object artifacts\ui-tests\<run>-r33\timing-log.txt
```

- lettura (1.0.1, senza strumentazione VSIX): il cronometro è la misura
  end-to-end; il plugin.log scompone SOLO il percorso Search Memory
  (embed/db/boost). Per la frase B la scomposizione richiederebbe le build
  debug strumentate 1.1.x — i numeri già raccolti sono nell'[analisi tempi
  S1.3](./2026-11-03-s13-timing-analysis.md) (warm 1.5–10ms, cold = boot tax
  1.4–4.9s).

**Se il tempo sfora** → non "riprovare finché passa": registra cronometro +
plugin.log per capire DOVE (embed vs DB vs boost vs transport) e passa alla
matrice guasti (§Guasti R3.3-specifici sotto).

## 4. Frase C — dettaglio memoria

Click su una voce espansa → si apre documento read-only `ibis-memory:` con
metadati (kind/status/visibility/tags/confidence/updated) e contenuto completo
(`memory_expand`; in fallback il summary di ricerca).

- **Evidenza:** `04-memory-detail.png`.
- **Se il documento è vuoto o senza metadati** → registra screenshot +
  `plugin.log` tail; il fallback summary compare? (il fallback è atteso solo
  se `memory_expand` fallisce — se non c'è nemmeno quello è un difetto).

## 5. Search Memory

Command Palette → **Ibis Assistant: Search Memory** → query libera (es.
`timeout plugin`) → root `search: "…"` con i match.

- **Evidenza:** `05-search.png`.

## 6. Refresh

Con almeno un kind espanso: icona refresh nel title della view.

**Atteso (da 1.0.1 — il bottone dà feedback visibile):**
- spinner **"Ibis: refreshing memory…"** nel title della view per ~0.4s;
- i parent ricalcano il numerino aggiornato (se una memoria è cambiata, il
  conteggio cambia — il re-render del parent avviene solo quando il label
  cambia, guard convergente);
- nodi ricaricati senza errori né duplicati visibili.

**Se lo spinner non compare** → registra la versione (`ibis: x.y.z` nella
riga activated): su build < 1.0.1 il refresh è corretto ma silenzioso
(refetch con dati identici = nessun effetto visibile — confermato dai dati
nell'[analisi tempi](./2026-11-03-s13-timing-analysis.md) §4).

- **Evidenza:** `06-refresh.png`.

## 7. Registrazione e chiusura

1. Aggiorna [review-r3r4-checklist.md](./review-r3r4-checklist.md) §Esito
   R3/R4: riga R3.3 → PASS (o PARZIALE con elenco difetti), link alla cartella
   evidenze, i numeri cold/warm.
2. **Non toccare i log `info`**: la demote `info`→`debug` (S1.4) si fa SOLO
   dopo la misura registrata — è azione dell'agente, avvisala con i numeri.
3. La coda WQ-1 si aggiorna con l'esito (una riga).

---

## Guasti R3.3-specifici (sintomo → causa → verifica → azione → evidenza)

| Sintomo | Causa probabile | Verifica | Azione | Evidenza da salvare |
|---|---|---|---|---|
| `error: timeout waiting for MCP response id=N` alla prima espansione | init store > timeout (mitigato a 60s in `9124c84`) o binario stale tornato | `where.exe ibis` + `ibis --version`; `plugin.log`: quando è partito SurrealDB vs quando è arrivata la chiamata | se binario stale: toglierlo dal PATH + Reload Window; se timeout anche a 60s: registrarla come regressione del 9124c84 | `plugin.log` init-to-call timeline + screenshot nodo errore |
| Nodo `error: …` sull'espansione (albero visibile ma voci = errore) | AI non configurato per il runtime plugin: Ollama giù (era giù il 02/11) o modello embedding assente | `Test-NetConnection :11434`; `ollama list`; `plugin.log` righe embedding | avvia Ollama / pull modello → Reload Window → riesegui frase B | output ollama + righe log embedding + screenshot |
| Espansione vuota (nessuna voce, NESSUN errore) | memorie assenti nel data dir che il runtime sta usando (seed non eseguito lì) | `seed-memory-r33.mjs` esce PASS? Nel toast di Status (click status bar): quale backend/data dir? | riesegui il seed con VS Code CHIUSO; verifica di non avere un `ibis.runtime.dataDir` override nelle settings VSIX | output seed + screenshot toast Status + screenshot settings `ibis.*` |
| Voci mostrate senza filtro kind (decision mostra fix) | regressione contratto filtro kind | `node scripts\smoke-mcp.mjs` (verifica kind filter sul canale stdio) | apri finding nel checklist — difetto runtime, non UI | output smoke-mcp |
| Cold > 2s | boot plugin pagato alla prima espansione (boot tax: SurrealDB 1–4s + check ~1.2s + probe; fino a +8.7s con Ollama giù — vedi analisi §2) | `plugin.log`: quando è partito SurrealDB vs quando è arrivata la prima chiamata; la seconda espansione è veloce? | se cold è boot tax ma warm è in target: è una-tantum — registra i numeri e prosegui; la rimozione della boot tax è la decisione owner sull'analisi §5 (pre-warm) | `plugin.log` boot timeline + cronometro |
| Warm > 500ms | costi per chiamata lato runtime (metadata DB) o view render | cronometro + `plugin.log` (senza righe `memory_search:` per la frase B — non embedda; il percorso Search Memory sì) | registra — è l'informazione che S1.3 deve produrre, non un ostacolo | cronometro + righe `plugin.log` rilevanti |
| `memory_add` (seed) crea righe ma l'albero resta vuoto | righe senza embedding (degraded) — non surfacciano nella ricerca semantica | seed eseguito con `EMBEDDING_PROVIDER=none`? Ollama era giù durante il seed? | riesegui seed con Ollama su (cancella prima i duplicati senza embedding) | output seed + `ollama list` |

## Ripristino ambiente (se serve)

- Duplicati/monnezza nel store: `memory_delete` via MCP (o lasciare — sono
  team-visibility e fanno parte dell'estate di test).
- SurrealDB orfani: `Get-Process ibis,surreal` → nessuno atteso a riposo; il
  job object killa l'albero alla chiusura (verificare che `plugin.log` finisca
  con `stdio closed; shutting down...`).