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
| WQ-1 | **R3/R4 gate** — VSIX 1.0.11 + plugin agent (Parte B) | [review-r3r4-checklist.md](./review-r3r4-checklist.md), [runbook R3.3](./2026-11-03-r33-closure-runbook.md), [analisi tempi S1.3](./2026-11-03-s13-timing-analysis.md) | **CHIUSO (03/11)**: Parte A 26/26 PASS; Parte B completa (R3.1–R3.8 + P4.1–P4.6 PASS) — run interattivo con evidenza da log; cold target chiuso dal pre-warm (prima fetch 12–30ms dopo boot pagato all'attivazione, 3.3–4.7s con Ollama su); note: P4.1 UX (status in chat), P4.5 tool-ref fix + superficie = mode picker, noise hook da estensione Azure Tools (checklist §Esito) | **push differito (decisione owner 03/11)** — a fine sviluppo: riscrittura dell'intera storia in pochi commit precisi (include rimozione claim/fact di test) → push | — | F4 (WQ-3) sbloccata |
| WQ-2 | **Publish gate** (azioni ② ③ ⑤ + `publish-vscode.yml`) | [review-r3r4-checklist.md §Publish gate](./review-r3r4-checklist.md) | **Differito (decisione owner 03/11)**: publish a sviluppo concluso, non a R3/R4 — vedi §3.6 | azioni manuali ②③ (Trusted Publishing OIDC, Open VSX token) al momento del publish | fine sviluppo (decisione owner su timing) | WQ-4 (D-CLAIMS gate pre-publishing) |
| WQ-3 | **F4 — Memory view + team server** (VSIX 1.1.0 / runtime 0.3.0) | [2026-11-01-memory-view-team-server-design.md](../superpowers/2026-11-01-memory-view-team-server-design.md) | **F4-A1 done (03/11, `81fa0d3`)** — VSIX 1.0.12: `runtime.mode` enum `auto` (nuovo default = attach al daemon personale) + **Start Daemon** (`ibis start` detached + re-probe) + provider http definition; Milestone B completa, resta il gate R7; 5 sub-fasi A residue | **F4-A2** — statusFilter current/all (`feat(vscode): memory tree status filter (current default)`) | — (publish differito, §3.6) | gate R7 → F3 docs → Track D |
| WQ-4 | **Track D** — benchmark DevAX/Scope (D1–D5) | [PLAN.md §9](./PLAN.md), [evaluations/scope/README.md](../evaluations/scope/README.md) | Pianificato (scaffold presente; profili con placeholder `TODO-verify-*` by design, pre-D2) | D1 — pin immagine compose + seed su istanza Scope live | WQ-3 (gate R7), azione D1 | D2–D5, R6 close |
| WQ-5 | **Miglioramenti ddofds** — fasi 0–5 (eval, retrieval, costi, memoria) | [analisi L1](./2026-11-01-dailydoseofds-improvement-analysis.md) → [L2](./2026-11-01-dailydoseofds-improvement-analysis-l2-flussi-funzionali.md) → [L3](./2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md); [piano A1](./2026-11-02-a1-retrieval-eval-plan.md); [validazione A2](./2026-11-03-a2-rrf-validation.md) | **Fase 0 (A1) done** (commit `9fb53da` 03/11): dataset 12 case + runner + CLI `ibis eval retrieval` + job CI `retrieval-eval`; baseline offline MRR 0.917 / r@5 0.958 / r@15 1.0, baseline ollama (additive) MRR 0.917 / r@5 1.0 / r@15 1.0. **Fase 1 (A2) implementata 03/11**: `rrf_fuse` + `FusionMode` + config `search.fusion` (default **additive**) + eval `--fusion` + campo `fusion` nel report; validazione: offline parità, ollama **regressione** (MRR 0.875, r@5 0.958 — perdita margine r05/r11). **Decisione owner A2 (03/11): opzione 2 — tuning RRF prima di decidere** (default resta additive) | **tuning RRF** (k più piccolo / ibride rank+cosine normalizzate, fuori scope A2) + re-run eval offline+ollama, poi decisione default | — | tuning A2; fasi 1–3 (C9/D13/B6); baseline per Track D D3 |
| WQ-6 | **Track B** — ingest tracce Beacon (B0–B4, B3 se promossa) | [PLAN.md §9-bis](./PLAN.md) | Pianificato (approvato 03/11; solo piano, nessun codice) | B0 — installazione endpoint Beacon (azione manuale ⑥) + campionamento `runtime.jsonl` → nota `evaluations/beacon/campionamento.md` | azione manuale ⑥ (owner) | B1–B4, R8, B3 (se promossa) |

Storico chiuso (non più in sospeso): F0.1–F0.7, gate R2, runtime 0.2.0 — vedi [PLAN.md](./PLAN.md).

## 2. Sequencing riconciliato

Fonti: PLAN.md (sequencing originale), F4 spec (sequencing aggiornato, 01/11), checklist R3/R4 (stato reale).

```text
[R3/R4 Parte B]──PASS──►[F4-B1..B4 ∥ F4-A1..A5]──►[R7]──►[F3 docs]──►[Track D D1..D5]──►[R6]──►[Publish gate ②③⑤]
     (WQ-1)                    (WQ-3)                                (WQ-4)                      (WQ-2, differito)

parallelo, runtime-side, ortogonale alla VSIX:
      [ddofds fase 0: A1 eval] ──► [fase 1: A2 tuning + C9+D13+B6] ──► [fasi 2–3: A4+A3+C7, D10+D11+D12]   (WQ-5)
```

Nota di riconciliazione: PLAN.md aveva `…R3/R4 → F3 → D1..D3 → R6`; l'F4 spec (più recente) inserisce `F4 → R7` tra publish e F3. **Aggiornamento 03/11 (decisione owner §3.6)**: il publish gate non segue più R3/R4 ma chiude la sequenza a sviluppo concluso (dopo R6/R7, timing a scelta owner); F4 è sbloccata direttamente.

## 3. Decisioni di coordinamento (dove i piani si toccano)

1. **Superficie tool 57 → 58.** F4-A3 introduce `memory_workstreams` e un filtro su `memory_list` (design F4 §A.3). Ogni intervento ddofds che tocca il registry/contratti (nessuno delle fasi 0–1; D10 cambia solo hint, non tool) deve seguire il pattern consolidato: contract test + discovery fixture aggiornati nello stesso commit.
2. **Consolidamento: F4-B3 (daemon-only, opt-in) vs C9/D11 (ddofds).** C9 (soglia+entity resolution) e D11 shadow-mode cambiano gli *interni* di `consolidate_loop`, non il trigger: compatibili con F4-B3 e landabili in qualsiasi ordine. D12 (response cache) non tocca il consolidation. Se F4-B3 atterra prima, C9/D11 lavorano sulla versione daemon-owned; se dopo, nessun conflitto previsto.
3. **A1 eval → Track D D3.** La baseline di retrieval quality (A1) è complementare al D3 "memoria → outcome": A1 misura l'ibis interno, D3 misura l'agente end-to-end. Fare A1 prima che D1..D3 parta in earnest rende le fasi 2–3 ddofds misurabili con Track D in parallelo.
4. **Timeout/sidecar.** WQ-1 S1.1 (client stdio persistente) e il rebuild del binario stale sono prerequisiti di misura per S1.3; l'F4-B1 (reconciliation ledger) complica il sidecar lifecycle — fare S1.3/S1.4 PRIMA di F4-B1 evita due volte le stesse misure.
5. **Versioni.** VSIX: le build debug iteracono il numero di **patch** (`1.0.1`, `1.0.2`, … — decisione owner 03/11, a ritroso dalle 1.1.0–1.1.2 provvisorie mai committate) e restano distinguishibili dalla versione a ogni reinstall; `1.1.0` resta riservato a F4. Runtime resta 0.2.0 finché il root `[Unreleased]` non viene taggato (decisione in checklist §Publish gate). F4 porterà runtime 0.3.0. Gli interventi ddofds vanno nel root `[Unreleased]` runtime, nessuna bump propria.
6. **Publish differito (decisione owner 03/11).** Il gate WQ-2 non segue più R3/R4: si pubblica a sviluppo concluso (indicativamente dopo R7/F4, timing a scelta owner). Conseguenze: il primo rilascio pubblico sarà **1.1.0** (F4) invece di 1.0.0 — le patch 1.0.x restano build locali mai pubblicate; le azioni ②③ (OIDC Trusted Publishing, token Open VSX) restano manuali owner e si fanno al momento del publish.
7. **Push differito + riscrittura storia finale (decisione owner 03/11).** Nessun push fino a fine sviluppo; a quel punto riscrittura dell'**intera** storia (47+ commit) in pochi commit precisi, che include la pulizia già registrata (claim `mem_26f1730…`/`mem_973f67a…` e le 2 fact DB-only via `memory_delete` — le fact DB-only non vivono in git, la loro rimozione resta operativa sullo store locale). Le build/prove intermedie non sopravvivono al rewrite: la coda e i doc di planning restano la fonte della verità.

## 4. Stato per stream (dettaglio minimo)

### WQ-1 — R3/R4
- Fatto: Parte A 26/26 (30/10); R3.1/R3.2 PASS; S1.1 stdio persistente (smoke 26/26); S1.2 igiene ambiente owner (pluginLocations corretto, binario Go stale rimosso). Blocker rebuild binario risolto (02/11: PATH `ibis 0.2.0`, Go stale gone).
- **Sessione debug owner conclusa (03/11)** — run S1.3 eseguito su build
  provvisorie 1.1.0–1.1.2 (strumentazione S1.3 lato VSIX/ide-host: spawn /
  cold-init / reuse / timing per tool call; mai committate). Riscontri owner
  implementati in **1.0.1** (numerino sul tree parent con guard convergente,
  refresh con spinner — il refetch esisteva già su 1.1.0, il difetto era solo
  di feedback) e **strumentazione rimossa** (fine sessione debug). Seed
  albero + runbook + [analisi tempi](./2026-11-03-s13-timing-analysis.md)
  (warm PASS ampio, cold fallito per boot tax — SurrealDB 1–4s, Ollama giù
  +8.7s). Chiusura VS Code verificata: zero processi orfani (job object ok).
- **R3.3 chiusa (03/11, registrata in checklist)** — frasi B/C verificate su
  1.0.1 dal run owner (raffiche `memory_list`/`memory_expand`/refresh ok in
  plugin.log 07:05–07:10 UTC); pass di misura S1.3 registrato (S1.3 done).
  Gap noto: cartella evidenze `artifacts/ui-tests/<run>-r33/` mai
  persistita (§6.4 analisi).
- **Decisione cold target (03/11, opzione 1: pre-warm all'attivazione)** —
  atterrata in VSIX **1.0.3** (`bf824b1` + fix): fire-and-forget in
  `activate()` — sessione stdio persistente + handshake + null call
  read-only `memory_list` (shape frase B dell'albero; 1.0.2 usava
  `ticket_get_capabilities`, rifiutato dal dynamic tool discovery senza
  ticketing — finding run owner 03/11: "pre-warm failed after 14944ms",
  boot comunque pagato dall'handshake); evidenza su canale Output
  (`pre-warm: runtime session ready in Nms`); la prima interazione reale
  riusa la sessione calda (stessa identità backend).
- **S1.4 atterrato (03/11, `e9b85b8`)** — timing `memory_search` demotati
  info→debug (invisibili al default INFO, tornano con `log_level DEBUG`);
  righe nuove a info `memory_list: done in … entries=N` /
  `memory_expand: done in … chain=N` (parità di evidenza frasi B/C, analisi
  §6.3). cargo test + clippy verdi.
- **Parte B COMPLETA (03/11 pomeriggio, run interattivo)** — R3.4–R3.8 e
  P4.1–P4.6 PASS con evidenza da log (sink `%LOCALAPPDATA%\ibis\ui-tests\`
  + plugin.log); **verifica pre-warm chiusa**: boot pagato all'attivazione
  (3.3–4.7s con Ollama su), prima fetch dell'albero 12–30ms end-to-end
  (target warm rispettato, il "cold" non esiste più sul path tree).
  Interventi atterrati durante il run: VSIX 1.0.4 (sink log) → 1.0.5
  (refresh conteggi collassati) → 1.0.6 (fix multiplexing parallelo
  ide-host) → 1.0.7 (auto-refresh post-capture + expand-all) → 1.0.9
  (expand-all rimosso, decisione owner) → 1.0.10 (fix media walkthrough) →
  1.0.11 (Quick Actions view); runtime: log esiti/rejection per tool call;
  plugin: tool-ref agenti in formato Copilot + nota brevità. Finding e gap
  nel checklist §Esito (incluso il `.mcp.json` smoke R2 rimosso e il rumore
  hook Azure Tools). Restano le azioni owner ②③ per il publish gate.
- Aperto: azioni ②③ (Trusted Publishing OIDC, token Open VSX) → publish
  gate (azioni ⑤ già fatte per i review).
- **Pulizia memorie di test (aggiornata 03/11: push differito, §3.7)**: la
  rimozione non è più una passata separata pre-push — finisce nella
  **riscrittura dell'intera storia** a fine sviluppo (claim in-repo
  `mem_26f1730…` commit `b112bd7` e `mem_973f67a…` commit `1f64f7f`
  eliminati nel rewrite; le 2 fact DB-only "Memoria fact di prova" /
  "Quick Actions fact" `mem_ff975f41…` vanno rimosse dallo store locale via
  `memory_delete`, non vivono in git). **Restano** le 3 memorie seed
  (decision/fix/gotcha, claim `a4cd24f`/`1d00552`: contenuto reale di
  progetto).

### WQ-3 — F4
- **F4-B1 atterrata (03/11, `e28d936`)** — `ibis_core::sidecar_ledger` (ledger
  JSON accanto al file dati SurrealDB: `pid`+start-time+`port`+`pgid`+owner
  identity; `reconcile` prima dello spawn: prune morte / keep con owner vivo
  / terminate orfane con owner morto; PID-reuse escluso via start-time,
  Windows FILETIME + `GetExitCodeProcess`≠STILL_ACTIVE — un terminato con
  handle aperti non è vivo; Linux `/proc` field 22; macOS kill-only,
  residuo documentato) + wiring `ProcessManager` (reconcile pre-spawn con
  ledger dir = parent del file dati, register solo per child confermato
  vivo, unregister post-stop riuscito). Test: 7 unit ibis-core (stale-kill
  e2e con sleeper reali) + live gated `IBIS_SURREAL_BIN` (owner crash
  simulato → restart pulito, 3.9s) + lifecycle riallineato. Workspace
  verde, clippy `-D warnings` verde.
- **Finding follow-up (da B1) — RISOLTO (03/11, commit fix)**: `daemon.rs`
  `stop_process_raw` apre con `TERMINATE|SYNCHRONIZE` senza query right →
  `GetExitCodeProcess` fallisce e il grace-loop D10 può leggere un processo
  vivo come già morto (mascherato col console condivisa, latente per child
  pipe-spawned). Fix: `PROCESS_QUERY_LIMITED_INFORMATION` nella maschera +
  test di regressione che pina il contratto exit-code attraverso la maschera
  esatta dello stop (`stop_handle_query_right_reads_still_active_for_a_live_process`).
  Verificato: ibis-server daemon tests 18/18.
- F4-B1 conclusa anche la parte sidecar-lifecycle-plan §Layer 3 (piano
  aggiornato, [sidecar-lifecycle-plan.md](./sidecar-lifecycle-plan.md)).
- **F4-B2 atterrata (03/11, `68c4c9d`)** — `ibis serve` alias di `run` (anche
  `-serve`/`--serve`) + profilo `--team` (F4 §B.1): nuovi flag server
  `--bind <addr>` e `--data-dir <path>` (normalizzazione single-dash estesa);
  il profilo applica i default SOLO sui knob non impostati — runtime_mode →
  `server` (personal claims `db_only` via resolver esistente), bind →
  `127.0.0.1` (esposizione LAN solo con `--bind` esplicito; config
  `bind_address` rispettato), proiezione `.ibis/memory` on salvo
  `memory.projection_enabled: false` esplicito nel config.json (nuovo
  `ibis_core::config::projection_explicitly_set`); no-op in plugin isolation.
  Banner startup `[TEAM]` a 4 righe: bind address + reminder header
  `X-Ibis-Actor` (CLI: `IBIS_ACTOR`) + boundary no-auth = reachability di
  rete + proiezione attiva. `--data-dir` override raw su `db_data_path`
  (stessa semantica CWD-relative degli altri flag; runbook B4 documenterà
  path assoluti). Test: 5 test profilo + single-dash flags (ibis-server),
  unit helper config (ibis-core); smoke live: banner `[TEAM]` + sidecar
  SurrealDB con data-dir isolata. Workspace verde (cargo test --workspace,
  clippy `-D warnings`).
- **Finding (da B2, non bloccante)** — `ibis_ai::runner::tests::default_api_url_substituted`
  fallisce quando Ollama è attivo sull'host (test `unwrap_err` sull'esito
  ensure-ollama, riceve `Ok(EnsureOllamaOutcome { process: None })`):
  coupling test↔ambiente, pre-esistente (verificato con stash del diff B2).
  Follow-up candidato: rendere il test indipendente da Ollama raggiungibile.
- **F4-B3 atterrata (03/11, `51c89d6`)** — consolidation loop daemon-only
  opt-in (F4 §B.5 / decisione T3): flag `--consolidate` (anche single-dash) +
  config `ai.consolidation.enabled` / `interval_secs` (off di default, floor
  60s, default 1h; flag > config). Nuovo modulo `ibis_server::consolidation`,
  gate: solo daemon sse non-plugin (stdio/plugin client surfaces mai);
  rifiuto del knob loggato (DB assente/degradato) — mai silenzioso. Discovery
  store-driven (`Optimizer::pending_observation_projects`: `SELECT project
  FROM memory WHERE kind='observation' AND status='current' GROUP BY project`,
  decodifica record `repo:*`) → una pass `consolidate_loop` per progetto
  (20 osservazioni, default del tool) + `maybe_project_memory` per progetto
  (parità col tool `memory_consolidate`, che resta il trigger umano: cron
  automazione plugin). Loop: primo tick immediato al boot (drain backlog),
  poi a cadenza; pass strettamente sequenziali (task singolo,
  `MissedTickBehavior::Delay`); stop col token di shutdown. Test: 4 gate
  unit + 2 discovery (fake db: repo distinte/junk scartate, errore propagato)
  + 3 flag (round-trip, override config) + 1 knob config; workspace verde,
  clippy `-D warnings` verde (unico fallimento: `ibis-ai
  runner::default_api_url_substituted` — finding pre-esistente B2, Ollama
  attivo sull'host). Nota residua (non bloccante): overlap in-process tra
  pass del loop e pass da tool concorrenti resta possibile (stessa race
  pre-esistente tra due `memory_consolidate` concorrenti); lease in-process
  candidato follow-up.
- **F4-B4 atterrata (03/11, `9921d6e`)** — packaging ufficiale del team
  server (F4 §B.2/§B.3): `deploy/Dockerfile` (build sorgente `ibis` con
  cache layer manifesti+stub → sorgenti reali; SurrealDB **3.3.5** dal
  tarball release — stessa versione pinnata dal job CI `retrieval-eval` e
  stessa fonte dell'installer runtime, glibc-safe su bookworm; ast-grep
  **0.51.1** dall'asset `app-*-unknown-linux-gnu.zip`, `sg`+`ast-grep`
  accanto al binario; `DB_AUTO_UPDATE=false` ⇒ nessun probe GitHub al
  boot; utente non-root uid 10001, `/data` volume per data file surrealkv +
  ledger sidecar, `/opt/ibis` scrivibile per `surreal.log` + sync regole,
  healthcheck su `GET /` discovery, CMD `serve --team --bind 0.0.0.0
  --data-dir /data/ibis-team.db --consolidate`) + `deploy/docker-compose.yml`
  (`build:` dal root repo, `init: true`, `restart: unless-stopped` (owner
  recovery senza job object), env commentate EMBEDDING_URL/LOG_FILE/
  SURREAL_USER/PASS, mount config.json opzionale, `image:` GHCR commentata
  — publish differito §3.6) + `.dockerignore` (contesto: workspace +
  `rules/`+`sgconfig.yml`) + `.gitattributes` (eol=lf sui tre artefatti
  docker: i RUN shell non sopravvivono al CRLF) + runbook
  `docs/team_server.md`: topologia/guardrail, install A compose e B
  systemd/NSSM (con `--data-dir` assoluti), flag+env, actor identity
  (header `X-Ibis-Actor` fidato **solo da loopback**; client LAN → argomento
  `actor` dei tool; impostazioni VSIX `ibis.identity.actor` + `IBIS_ACTOR`
  CLI arrivano con F4-A5), onboarding client (VSIX attach + `mcpUrl`,
  Cursor http variant, Claude Desktop via `mcp-bridge`, plugin http variant,
  CLI loopback-only today), ownership consolidation §B.5 (consigliato ON,
  automazione settimanale resta il trigger umano), backup/restore/upgrade,
  checklist verifica R7, note sicurezza. Verifica su host senza Docker:
  YAML validato (pyyaml), script stub RUN testato sotto Debian sh (19
  lib + 3 main = 22 target), tarball/zip sidecar ispezionati
  (`surreal`@root tgz, `sg`+`ast-grep`@root zip), asset `include_dir` tutti
  git-tracked; prima build immagine reale al gate R7 (scenario compose su
  LAN, 2+ client).
- **F4-A1 atterrata (03/11, `81fa0d3`)** — attach default al daemon personale
  (F4 §A.1): VSIX **1.0.12** con `runtime.mode` enum `auto`/**spawn**/**attach**
  e default `auto` → `resolveBackendSelection`: attach a `DEFAULT_MCP_URL`
  (override `IBIS_MCP_URL` / `runtime.mcpUrl` esplicito), provider definition
  `{ type: "http", url }` ora attiva di default e tutti i path comando via
  `callMcpTool` http. Status bar in daemon mode sonda l'endpoint
  (`settings.probeTimeoutMs`): reachable → `$(cloud) Ibis attach`, offline →
  `$(error) Ibis daemon offline` (tooltip con hint Start Daemon). Nuovo comando
  **Ibis Assistant: Start Daemon**: one-shot `<binary> start` (verbo PID-file
  del runtime — daemon detached, log su `server.log`, quindi outlives VS Code)
  + re-probe ~15s (boot SurrealDB sidecar); spawn error/timeout o endpoint mai
  raggiungibile → messaggio con istruzione manuale (`ibis start`/`ibis run`);
  **Status** in offline offre il pulsante Start Daemon. `spawn` esplicito resta
  bit-for-bit (autoAttach + probe urls inclusi; `warnDualPluginChildren` solo
  in spawn mode, messaggio aggiornato: spawn + daemon/plugin, fix team =
  plugin `mcp.json` http variant). `probeReachable` hardened (cancel del body
  SSE — il probe ora è hot path). `prewarmRuntime` resta spawn-only (il daemon
  è sempre warm; il tree refresh paga il primo contatto). README + walkthrough
  choose-backend (3 modalità) + CHANGELOG. Verifica: smoke-plugin 30/30 (4
  check nuovi: enum auto/default, Start Daemon contribuito+registrato, provider
  http shape, warning solo in spawn), ide-host test verdi (session 4/4,
  framing, resolve — machineria stdio intoccata), `node --check`, `vsce
  package` 1.0.12. Note: versione 1.0.12 (iterazione patch per build debug,
  `1.1.0` resta riservato alla chiusura F4-A — §3.5); il comportamento
  "outlives VS Code" del daemon (job del host) e il provider http su agent
  mode reale restano da verificare al **gate R7**.
- Prossimo commit previsto: `feat(vscode): memory tree status filter (current
  default)` (F4-A2 — setting `ibis.memory.statusFilter` current/all, badge
  view title, toggle command; tocca `memory_list`/`memory_search` args).
- Dipendenza interna da rispettare: B.6 (reconciliation) è precondizione strutturale del Milestone B — **soddisfatta**.

### WQ-4 — Track D
- Disallineamento README/analysis.mjs (pass@k annunciato vs pass-rate implementato) e placeholder profili: **stato atteso**, non difetto — si chiudono a D2 (runner+analysis) e D1 (pin/seed su istanza live). Registrato anche come correzione nell'analisi L2 §4.1.

### WQ-5 — ddofds
- **Fase 0 (A1) completata 02/11/2026, committata 03/11 (`9fb53da`)**:
  `eval.rs` in ibis-optimization (dataset YAML validato, `RetrievalEvalRunner`
  con mrr / recall@5 / recall@15 / per-channel, estate golden auto-seminata
  con id espliciti, `OfflineEmbedder` deterministico), CLI in-process
  `ibis eval retrieval` (niente nuovo tool MCP: registry resta a 57), dataset
  `evaluations/retrieval/dataset.yaml` (12 case), job CI `retrieval-eval`
  (SurrealDB pinnato v3.3.5 + embedder offline, soglia 0.75). Baseline offline:
  MRR 0.917, recall@5 0.958, recall@15 1.0 — 12/12 ok; baseline ollama
  (additive, 03/11): MRR 0.917, recall@5 1.0, recall@15 1.0.
- **Fase 1 (A2 RRF) implementata e validata 03/11/2026**: `rrf_fuse` (k=60,
  somma cross-channel) + `FusionMode` + config `search.fusion` (default
  additive) + "is current" come epsilon post-fusione + tie-break per id +
  eval `--fusion` con campo `fusion` nel report. Validazione
  ([evidenza](./2026-11-03-a2-rrf-validation.md)): parità offline, regressione
  su ollama (MRR 0.875 / r@5 0.958; r05 1→2, r11 secondo hit 5→9 — perdita
  di margine cosine, tradeoff noto). Default NON girato: **decisione owner**
  (opzione raccomandata: default additive, procedere con C9/D13/B6).
- Fasi 0–3 specificate in L3 con criteri di accettazione per intervento;
  **decisione owner su A2 (03/11): opzione 2 — tuning RRF prima di decidere**:
  default resta `additive` (rollback a config), il tuning (k più piccolo,
  ibride rank+cosine normalizzate — fuori scope A2 come specificato, candidati
  a chiudere il gap r05/r11) rivaluta su eval offline+ollama e solo poi si
  decide il default. C9+D13+B6 restano la fase 1 dopo il verdetto RRF.

### WQ-6 — Track B
- Nessun sub-fase iniziata: piano integrato in [PLAN.md §9-bis](./PLAN.md)
  (approvato 03/11, sessione analisi Agent Beacon vs Microsoft Scope).
  Script-first (B0/B1/B2/B4), promozione a lane nativa `ibis-ingest-traces` (B3)
  solo dopo ≥3 iterazioni utili; gate **REVIEW R8**. Prerequisito owner:
  azione manuale ⑥ (endpoint Beacon MSI, user-mode).