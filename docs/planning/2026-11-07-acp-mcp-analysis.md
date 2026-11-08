# ACP (Agent Client Protocol) vs MCP per Ibis — analisi critica

**Data:** 2026-11-07
**Status:** Analisi + decisione operativa proposta — "MCP-first, ACP-aware"
**Domanda:** implementare l'Agent Client Protocol in aggiunta o in sostituzione di MCP ha senso per Ibis Assistant?

## Verdict (TL;DR)

| Opzione | Esito | Perché |
|---|---|---|
| **Sostituzione di MCP con ACP** | **No, categorico** | Livelli diversi della pila: MCP è *host ↔ strumenti/contesto*, ACP è *editor ↔ agente autonomo*. ACP non definisce primitive `tools/resources/prompts`; un agente ACP ottiene i suoi strumenti tipicamente **via MCP** (RFD attiva "MCP-over-ACP", target MCP 2026-07-28 — la stessa versione già servita da Ibis). Sostituire = riscrivere il prodotto e perdere ogni client attuale. |
| **Aggiunta di ACP ora** | **No, ROI negativo** | La copertura incrementale è ~zero: gli agenti ACP principali (Claude Code, Gemini CLI, Codex, Copilot in preview) supportano MCP server, e Zed stesso supporta MCP context server → Ibis è già raggiungibile nell'ecosistema ACP senza codice nuovo. Il costo (sessioni stateful, churn v1→v2, matrice test, wizard) non è coperto da domanda attuale. |
| **Posizione consigliata** | **MCP-first, ACP-aware** | MCP resta l'unica superficie protocollo. Decisione registrata + 4 trigger misurabili (§5) che aprirebbero uno spike `ibis-acp` read-only riusando il registro tool esistente. |

## 1. Disambiguazione: quale "ACP"?

Due protocolli condividono il nome:

- **Agent Client Protocol (Zed Industries)** — comunicazione editor/client ↔ agente di coding. **È l'oggetto di questa analisi.**
- **Agent Communication Protocol (IBM/BeeAI)** — comunicazione agente ↔ agente; confluito nel solco A2A (Linux Foundation), non rilevante qui.

Nei materiali di prodotto usare sempre il nome completo "Agent Client Protocol" per evitare la collisione (aggiungere nota a `docs/naming.md` quando si tocca il naming).

## 2. I due protocolli: livelli diversi della pila

| Aspetto | MCP (Model Context Protocol) | ACP (Agent Client Protocol) |
|---|---|---|
| Livello | Host LLM ↔ server di strumenti/contesto | Editor/client ↔ agente autonomo |
| Domanda che risolve | "Come do strumenti e contesto a *qualsiasi* LLM app?" | "Come faccio girare *qualsiasi* agente dentro il mio editor?" |
| Ruoli | Host + client + server | Client (editor) + agent (subprocess) |
| Primitive lato "servitore" | tools, resources (+subscribe), prompts, discovery (`server/discover`) | sessioni (`session/new`/`load`/`prompt`/`set_mode`), update stream, agent plan, slash commands, diff |
| Primitive lato "host" | roots, **sampling**, elicitation | `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/*` |
| Trasporti | stdio, Streamable HTTP (+OAuth 2.1/DCR) | stdio JSON-RPC 2.0; remoto HTTP/WebSocket = RFD attiva, non landed |
| Versione corrente | 2026-07-28 (date-versioning; draft aperta) | v1 stabile; v2 draft (13+ RFD v2) |
| SDK | ufficiali TS/Python/Java/Kotlin/C#/Rust (rmcp 3.7.0 = quello che usiamo) | ufficiali TS/Rust/Python/Java/Kotlin; community Go/.NET/Swift/Dart/… |
| Governance | open specs, SEPs, registry ufficiale | org `agentclientprotocol`, processo RFD, co-steering **Zed + JetBrains** |
| Adozione | de facto standard: Claude, ChatGPT, VS Code, Cursor, Zed, ecc. | 50+ **agenti** (Gemini CLI, Claude adapter, Codex adapter, Copilot preview, Cursor-as-agent, Cline, Goose, OpenCode, Junie…); **client** perlopiù plugin/community (VS Code via estensioni, Neovim, Emacs, Obsidian, Qt Creator…), Zed nativo |
| Legame reciproco | — | riusa le rappresentazioni JSON di MCP "dove possibile"; RFD **MCP-over-ACP** per esporre tool MCP dentro il canale ACP |

Citazione chiave dell'RFD MCP-over-ACP (attiva, autore nikomatsakis): *"ACP manages an agent's conversation while MCP provides tools behind it."* È la definizione ufficiale della divisione dei ruoli: ACP **presuppone** MCP come livello strumenti, non lo rimpiazza.

Dettaglio superficie ACP v1 (serve per la stima di costi in §4):

- Metodi **agente**: `initialize`, `authenticate`, `session/new`, `session/prompt` (+ opzionali `session/load`, `logout`, `session/set_mode`).
- Metodi **client**: `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/*` (capability-gated).
- Notifiche: `session/update`, `session/cancel`.
- UX dedicata: agent plan, session modes/config options (inclusa la selezione del modello), slash commands, elicitation, diff.

## 3. "Sostituzione": un errore categoriale

### 3.1 ACP non ha un livello strumenti

L'architettura attesa da ACP: l'editor (client) avvia un agente ACP; l'agente porta il proprio loop LLM e i propri strumenti, che **tipicamente configura come MCP server**. L'RFD MCP-over-ACP formalizza proprio questo: dichiarare un server MCP `{"type": "acp"}` e fare richieste MCP stateless sul canale ACP già autorizzato — "senza un secondo processo o endpoint di rete" — con target **solo MCP 2026-07-28**.

Un "server ACP" che rimpiazzi un "server MCP" non esiste concettualmente: in ACP Ibis sarebbe *un agente*, non un server. Significherebbe smettere di essere il livello memoria/strumenti e diventare un loop autonomo che modifica codice — esattamente ciò che il README esclude.

### 3.2 Le dipendenze strutturali di Ibis sono su feature *client* di MCP

Inventario del perimetro MCP già costruito (`crates/ibis-server`, `crates/mcp-bridge`):

| Capabilità Ibis | Dove vive | Equivalente in ACP |
|---|---|---|
| 58 tool (`registry.rs`: "the table IS the registry" + `tools::dispatch`) | rmcp 3.7.0 `ToolRouter` | **nessuno**: ACP non elenca tool; l'agente li consuma via MCP |
| `sampling/createMessage` — `ide_sampling` default in runtime plugin (l'IDE è l'host di reasoning; chain `sampling → lm_bridge → ollama → context_only`) | `crates/ibis-ai/src/mcp_sampling.rs` | **nessuno**: in ACP l'agente porta il *suo* accesso modello (la RFD draft "Configurable LLM Providers" riguarda la config dell'agente, non un servizio del client) |
| `roots/list` — binding workspace del plugin | `crates/ibis-server/src/mcp_server.rs` | diverso: ACP passa i workspace roots come *parametro di sessione* (`session/new`), non come servizio richiesto dal server |
| MCP Apps curator (`ui://ibis/memory-curator`, SEP-1865) | `crates/ibis-httpserver/src/curator.rs` | **nessuno** (la UX ACP è plan/diff/permission) |
| Resources + subscribe per revisione di protocollo | `crates/ibis-server/src/resources.rs` | **nessuno** |
| Discovery `server/discover` (2026-07-28) + legacy `initialize` | `crates/ibis-httpserver` | **nessuno** |
| `mcp-bridge` stdio→HTTP (Claude Desktop) | `crates/mcp-bridge` | n/a (è comunque MCP) |
| Registry MCP (`server.json`, GitHub OIDC in `publish-vscode.yml`) | repo root + CI | esiste un "ACP Agent Registry", ma registra *agenti*, non server di strumenti |

Migrare = riscrivere ogni riga di questa tabella su un protocollo che non offre le stesse primitive, oppure perderle.

### 3.3 Contraddizione con l'identità di prodotto

- README: *"Not an agent OS. Ibis does not absorb or rewrite your agents; it feeds them context."*
- DNA non negoziabile: local-first + team server own-infra, AI-client agnostic (MCP + superfici Cursor/VS Code), memoria di progetto viva (git/ticket/graph), non chat memory.
- Un agente ACP è per definizione un loop autonomo con accesso modello proprio: inverte il modello ibrido (l'host cede il reasoning a Ibis via sampling) e sposterebbe la scelta del modello dentro Ibis — contro la filosofia plug-and-play (wizard con default sensati, non ENV) e contro il default plugin `ide_sampling`.

## 4. "Aggiunta": ROI negativo oggi

### 4.1 La copertura incrementale netta è ~zero

Matrice superfici Ibis oggi (`docs/ide-integrations.md`): VS Code extension, Cursor plugin, Copilot CLI/chat plugin, MCP App curator, Claude Desktop (via `mcp-bridge`), CLI. Mancano solo i flussi *agent-first* (Zed, Neovim/Emacs/Obsidian via plugin ACP, eventuali JetBrains).

Ma quel perimetro è già coperto **senza scrivere codice**:

- gli agenti ACP principali supportano MCP server → in Zed, Claude Code / Gemini CLI / Codex caricano Ibis come MCP server e i suoi 58 tool arrivano dentro la sessione agente;
- Zed stesso supporta MCP context server;
- se l'RFD MCP-over-ACP atterra, i client ACP potranno passare i tool MCP (`{"type":"acp"}`) agli agenti direttamente dal canale — sempre Ibis-as-MCP, costo per Ibis = zero.

L'unico incremento vero di un'implementazione ACP-native sarebbe "Ibis come *agente* in chat" — un prodotto diverso da quello che Ibis dichiara di essere.

### 4.2 Costi concreti

- **Sessioni stateful** (`new`/`prompt`/`load`/resume/cancel, con replay proposto in v2) vs tool call stateless: nuova semantica da mappare su `working_state` + `actor`.
- **Terzo listener** (`-mode acp` accanto a `sse|stdio`): la matrice runtime × listener si allarga, e con essa l'estate di test (786+ test oggi, contract test + discovery fixture in lockstep).
- **Churn di protocollo**: la v2 draft è una riscrittura ampia (prompt lifecycle, permission requests, session resume replay, diff file states, background work, tool call updates…). Adottare v1 ora significa pagare la migrazione v1→v2 quando v2 stabilizza.
- **SDK Rust ufficiale giovane**: il crate `agent-client-protocol` è appena stato ricostruito su SACP (RFD "Rust SDK based on SACP" recentemente completata); ritmo di rilascio sostenuto (2.789 commit, schema-v* a ogni release).
- **Reasoning**: senza `sampling/createMessage`, un ibis-agente ACP partirebbe da `context_only`/Ollama — più debole del percorso MCP+sampling nello stesso IDE.

### 4.3 Stima

Spike realistico riusando il dispatcher esistente (crate dedicato che parla ACP stdio e chiama in-process `tools::dispatch`, oppure si attacca via HTTP MCP al daemon — come fa `mcp-bridge`, ma in ACP; runtime `personal` only): **1–2 settimane** + manutenzione a ogni release ACP (schema + eventuali v2). Nessuna domanda utente registrata oggi per giustificarlo, e la coda prioritaria (F4, R7, publish) non lascia capacità libera.


## 5. Steelman: quando ACP avrebbe senso (trigger misurabili)

La conclusione "no" vale **oggi**, non in astratto. Quattro condizioni aprirebbero la valutazione:

| Trigger | Cosa cambia | Azione |
|---|---|---|
| **T1 — Client ACP nativo JetBrains GA** (JetBrains è co-steerer; oggi Junie figura come *agente*, gli IDE JetBrains non risultano tra i client pubblicati — verifica 07/11/2026) | massa critica agent-first fuori dal perimetro VS Code/Cursor | rivalutare lo spike |
| **T2 — RFD MCP-over-ACP landed** | i client ACP passano tool MCP `{"type":"acp"}` agli agenti senza processo separato | testare lato client (Zed); costo Ibis = zero — resta MCP |
| **T3 — Domanda concreta di un team solo-Zed** per un "ibis agent" read-only (ask/remember in chat, slash commands ↔ skill) | superficie nuova con valore reale | aprire lo spike |
| **T4 — ACP v2 stabile + SDK Rust maturo** | il costo di manutenzione scende sotto una figura-giornata/trimestre | aprire lo spike |

**Forma dell'eventuale spike `ibis-acp`** (da NON costruire prima di T3, o T3+T4):

- crate separato (`crates/ibis-acp`) o flag `-mode acp-stdio`; runtime `personal` only, attach HTTP al daemon esistente (come `mcp-bridge`, ma parla ACP);
- facciata **read-only**: `session/prompt` → `ibis ask` / ricerca ibida sul grafo; niente `fs/*`, niente permission request (Ibis non edita codice);
- slash commands ACP ↔ skill esistenti del plugin (`/ibis usage`, remember, status, set-working-state);
- sessione ↔ `working_state` + `actor`; reasoning di default `context_only`/Ollama;
- conformance harness ACP (`/libraries/testing`) nei test.

Nota onesta a favore di ACP che questa analisi accoglie senza buttarla via: l'adozione sugli **agenti** è reale e ampia (Gemini CLI, Codex, Claude via adapter, Copilot in preview, Cline, Goose, OpenCode, Junie…), e la co-steering Zed+JetBrains dà al protocollo una governance più solida di un progetto single-vendor. Il punto non è "ACP è debole": è che **non è un canale di distribuzione per un server di strumenti**, che è ciò che Ibis è.

## 6. Decisione operativa

1. **MCP resta l'unica superficie protocollo.** Nessuna sostituzione; nessuna implementazione ACP adesso.
2. **Registrare la decisione**: questo documento come analisi di riferimento; alla prima revisione owner, aggiungere la riga `WQ-n` in `2026-11-02-master-work-queue.md` ("ACP monitor — T1–T4") secondo le regole della coda (registrazione solo per piani approvati).
3. **Monitor trimestrale** dei trigger T1–T4: una riga di verifica, non un progetto.
4. **Comunicare il posizionamento**: una riga nel README e nel discovery doc (`GET /`): "MCP-first, ACP-aware" — così i client/distributori sanno cosa aspettarsi e perché.
5. **Se T2 atterra**: provare `{"type":"acp"}` lato client Zed — è costo-orologio, non costo-codice Ibis.
6. **Vietato il "per esserci"**: la facciata nasce solo al verificarsi di T3 (idealmente con T4 già chiuso).

## 7. Rischi residui e mitigazioni

| Rischio | Mitigazione |
|---|---|
| ACP v2 riscrive i contratti (churn) | gate T4; nessun codice ACP distribuito prima |
| Naming collision ACP (IBM/BeeAI → A2A) | nome completo "Agent Client Protocol" in ogni documento; nota in `docs/naming.md` |
| Team server senza auth + futuro remote transport ACP | restare stdio/loopback; mai esporre un agente remoto finché non esiste auth (il trust odierno è reachability, non autenticazione — `docs/ide-integrations.md`) |
| Bias "feature freshness" nella roadmap | la differenza competitiva di Ibis è il layer temporale/git-bound (commit-meaning, comparable truth across commits), non il trasporto; ACP non avanza quel perimetro |
| Zed/JetBrains disintermediano MCP | T2 è la guardia: MCP-over-ACP *rafforza* MCP come livello strumenti |

## 8. Correzioni di registro (drift rilevato durante l'analisi)

- `AGENTS.md` conteneva la fact obsoleta "the Cursor plugin is a thin MCP stdio host of the **Go** binary": il workspace è **Rust** (20+ crate, rmcp 3.7.0). Corretta in data odierna.
- Conseguenza pratica: l'SDK rilevante per un eventuale spike ACP è quello **Rust ufficiale** (`agent-client-protocol`), non Go (gli SDK Go restano community).

## 9. Fonti (verificate 07/11/2026)

- Agent Client Protocol — introduzione: https://agentclientprotocol.com/ ; superficie v1: https://agentclientprotocol.com/protocol/v1/overview
- RFD MCP-over-ACP (attiva): https://agentclientprotocol.com/rfds/mcp-over-acp
- Processo RFD e governance (Zed + JetBrains): https://agentclientprotocol.com/rfds/about
- Agenti e client ACP: https://agentclientprotocol.com/get-started/agents , https://agentclientprotocol.com/get-started/clients
- Librerie (ufficiali + community, inclusi gli SDK Go): https://agentclientprotocol.com/libraries/community
- Repo schema/SDK: https://github.com/agentclientprotocol/agent-client-protocol (v1 stabile, v2 draft, ~4.6k★)
- MCP spec 2026-07-28: https://modelcontextprotocol.io/specification/latest
- Repo Ibis: `README.md` (posizionamento "Not an agent OS"), `docs/runtime_modes.md` (rmcp 3.7.0, protocollo 2026-07-28), `docs/ide-integrations.md` (matrice superfici), `docs/ide_host_and_lm_bridge.md` (F0.5 sampling), `crates/ibis-server/src/registry.rs`, `crates/ibis-httpserver/src/curator.rs` (SEP-1865)

