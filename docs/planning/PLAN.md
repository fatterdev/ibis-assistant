# Ibis Assistant — Piano di implementazione (bloccato)

> Stato: **approvato, pronto all'esecuzione**. Sorgente delle decisioni: sessione di
> ricerca e brainstorming del 29/10/2026 su VS Code 1.144, Agent Plugins 1.0, MCP,
> Copilot Memory e Microsoft Scope.
> Regola operativa: **nessuna modifica al repo sorgente `C:\devsrc\ibis-assistant`**
> (che viene letto in sola lettura dallo script di migrazione); il prodotto nasce qui.
> Aggiornamento 29/10/2026: **F0.1 completata** e runtime migrato promosso a root del
> prodotto; i riferimenti a `runtime/` nelle fasi vanno letti come root del repo.
> **Aggiornamento 30/10/2026 — revert del rebrand**: prodotto = **Ibis Assistant**
> (binario `ibis`, MCP label "Ibis", data dir `%LOCALAPPDATA%\ibis`, repo
> `fatterdev/ibis-assistant`). Le F0.2–F0.7 sono complete anche dopo il revert
> (rename meccanico, zero cambi funzionali); la mappa del naming del rebrand
> revertito resta come documento storico in [naming-map.md](naming-map.md).
> **Aggiornamento 30/10/2026 — gate R2 chiuso**: review R2 superata (Parte A
> protocollo 23/23 via `scripts/smoke-mcp.mjs`; Parte B B1–B5 con evidenza owner in
> VS Code 1.144 — zero conferme read-only, `/ibis.*`, resources in Add Context,
> sampling attivo via `chat.mcp.serverSampling` + registro sampling requests,
> MCP App con fallback testuale). Evidenze e finding in
> [review-r2-checklist.md](review-r2-checklist.md). Fix BatchManager record-id
> incluso; runtime rilasciato **0.2.0** (tag `v0.2.0`). Da qui: **[F1 ∥ F2]**.
> **Aggiornamento 01/11/2026 — F4 approvata**: round S1 debug chiuso (client
> stdio persistente in ide-host, pipeline embed corazzata, igiene ambiente);
> brainstorm [memory-view-team-server-brainstorm.md](memory-view-team-server-brainstorm.md)
> deciso (§6) e spec
> [2026-11-01-memory-view-team-server-design.md](../superpowers/2026-11-01-memory-view-team-server-design.md)
> approvata: **F4 A** (memory view, VSIX 1.1.0, backend daemon sempre-on) e
> **F4 B** (team server, runtime 0.3.0) dopo il publish R3/R4, prima di F3.

## 1. Decisioni di prodotto

| Decisione | Esito |
|---|---|
| Prodotto | **Ibis Assistant** — living project memory for real codebases (revert 30/10/2026: il nome del rebrand è stato scartato dall'owner) |
| Radice brand | `ibis` (l'ibis sacro di Thoth, *Threskiornis aethiopicus* nella lore) — revert globale dal rebrand F0.1, zero eccezioni |
| Crate Rust interni | `ibis-*` (revert del prefisso F0.1; solo `mcp-bridge` resta perché privo del prefisso) |
| Env vars | `IBIS_*` (revert del prefisso F0.1) |
| Binario/CLI | `ibis` (`ibis run`, `ibis memory add`) — nome corto |
| VSIX | `fatterdev.ibis-assistant`, display "Ibis Assistant", settings `ibis.*` |
| Agent plugin | `ibis` (Agent Plugins 1.0; skill corte → slash `/ibis usage`), marketplace repo `fatterdev/ibis-marketplace` |
| Reasoning | **MCP sampling** come via primaria; LM bridge custom deprecato ma mantenuto come fallback; Ollama fallback finale |
| MCP Apps | **Incluso nel primo rilascio** ("Memory Curator", con degradazione testuale sempre garantita) |
| API VS Code | engines `^1.144.0`, uso delle API stabili più recenti (`chatInstructions`, LM tools, MCP provider, MCP Apps) |
| Publishing | Full distribution: VS Marketplace (publisher `fatterdev`, OIDC Trusted Publishing) + Open VSX + MCP registry + plugin marketplace + `vscode:mcp/install` |
| Track D (Scope) | Sperimentale, config-as-code; **D-CLAIMS** = benchmark interno dei claims (gate pre-publishing); **D-INSIGHTS** = Scope Insights come fonte di memoria (import script-first, promozione a ingest lane solo dopo ≥3 iterazioni utili) |
| Track B (Beacon) | Sperimentale, **script-first** (specchio di Track D): tracce JSONL Beacon → `observation` high-signal → `memory_consolidate` distilla; import memoria review-gated (B4). Beacon = cattura, Ibis = memoria tipizzata (le tracce sono fonte di ingest come logs/tickets); **zero Beacon Cloud**; visibility configurabile, default `team` |
| Skill duplicati | Il contratto di sessione vive come skill `usage` nel plugin e come `chatInstructions` nella VSIX — mai entrambi come skill; skill e comandi non si duplicano (le skill corte sono l'unica superficie slash `/ibis <nome>`) |
| Skill sync Copilot Memory | Sì, sperimentale (`ibis-sync-copilot-memory`) |

## 2. Mappa di naming

| Superficie | Nome |
|---|---|
| Repo GitHub | `fatterdev/ibis-assistant` (redirect automatico dal nome del rebrand) |
| Binario/CLI | `ibis` |
| Data dir | `%LOCALAPPDATA%\ibis\` |
| MCP server (label per agenti) | "Ibis"; prompts `/ibis.<nome>`; risorse `ibis://…` |
| MCP registry (`server.json`) | `ibis` |
| VSIX | `fatterdev.ibis-assistant` — "Ibis Assistant" |
| Plugin | `ibis` — skill `usage/-readiness/-status/-remember/-set-working-state` (slash `/ibis <nome>`; label MCP VSIX "Ibis") |
| Cursor plugin | binario `ibis`, env `IBIS_*`, skill/commands `ibis-*` |
| ide-host | `@ibis/ide-host`, default `IBIS_BIN="ibis"`, clientInfo `ibis-ide-host/1.0.0` |
| Crate Rust | `ibis-core`, `ibis-server`, `ibis-ai`, … (`mcp-bridge` invariato) |
| MCP App | "Ibis Assistant Memory Curator" (`ui://ibis/memory-curator`) |
| Lore README | Thoth, l'ibis sacro (*Threskiornis aethiopicus*), Casa della Vita — i riferimenti all'uccello "ibis" nei testi di lore sono **intenzionali** |

## 3. Convenzioni di lavoro

- Commit convenzionali lowercase, in stile repo: `feat:`, `fix:`, `docs:`, `refactor!:`, `chore(evals):` — **1 commit per sottofase**, mai mega-commit.
- Root `CHANGELOG.md`: sezione `## [Unreleased]` accumulata durante le fasi, rilasciata ai gate come `0.2.0`.
- Verifica obbligatoria `cargo test` (786+ test) a ogni tocco dei crate; `cargo xtask dist` (layout gate) e `vsce package` dove indicato.
- Review gate = checkpoint con l'owner prima di procedere oltre.

## 4. Azioni manuali dell'owner (fuori codice)

1. Rename repo GitHub → `ibis-assistant` (redirect automatico dal nome del rebrand) + update remote locale.
2. Partner Center: publisher `fatterdev` + policy **Trusted Publishing** (OIDC da GitHub Actions, vsce ≥ 4.0.2).
3. Account Eclipse Foundation per **Open VSX** (canale separato, non Microsoft).
4. Istanza **Scope** (docker-compose self-hosted) + preview/API key.
5. Installazione locale di VSIX e plugin per i review R3/R4.
6. Installazione locale **Beacon endpoint** (MSI Windows, user-mode) + `beacon endpoint test-event` (prerequisito del campionamento B0).

## 5. FASE 0 — Runtime (Rust)

### F0.1 — Migrazione del runtime con rename globale (primo commit del prodotto)
Eseguita il 29/10/2026 con `scripts/migrate/migrate-rename.ps1` (legge il repo
sorgente in sola lettura, scrive l'albero migrato in `<repo>/runtime/`); il rebrand
fu poi revertito il 30/10/2026. Sub-fasi, commit-message e verifiche nello storico
della migrazione: [scripts/migrate/migrate-checklist.md](../../scripts/migrate/migrate-checklist.md)
e mappa [naming-map.md](naming-map.md) (documenti storici).

**→ REVIEW R1**: cargo test green, dist builds, VSIX installa, repo rinominato.

### F0.2 — Tool annotations
`ibis-server/src/registry.rs`: `readOnlyHint: true` + `title` per tutti i read-only
(`ask_project`, `analyze_blast_radius`, `find_dead_code`, `memory_search/expand/history/insights`,
`why_changed`, `as_of_project`, `knowledge_diff`, `get_working_state`, `load_session_context`,
`recent_failures`, `settings_get`, `ticket_get/search/search_my/list_statuses/list_projects/search_users/get_capabilities`,
`get_rules`); esplicito `readOnlyHint: false` per i mutanti. Test di completezza (ogni tool dichiara l'annotation).
Commit: `feat(server): read-only tool annotations (readonlyhint, titles)`.

### F0.3 — MCP prompts
`prompts/list` + `prompts/get`: `readiness`, `status`, `why_changed`, `blast_radius`, `remember`
(testi derivati dal contratto di sessione, argomenti con completions). Test integrazione protocollo.
Commit: `feat(server): mcp prompts (/ibis.*)`.

### F0.4 — MCP resources + templates
`resources/list/read` + templates `ibis://memory/{kind}/{id}`, `ibis://rules/{name}`,
`ibis://ticket/{id}`; `resources/subscribe` + notifiche `resources/updated` su mutazioni memoria (lane SSE).
Commit: `feat(server): memory, rules and tickets as mcp resources (templates + live updates)`.

### F0.5 — Reasoning via MCP sampling
`ibis-ai`: provider `mcp_sampling` (request `sampling/createMessage` sul canale MCP del client),
catena fallback `sampling → lm_bridge → ollama`; setting `ai.reasoning.provider`
(default `ide_sampling` in plugin mode); LM bridge deprecato ma funzionante (`IBIS_LM_BRIDGE_*` solo fallback).
Verify: unit test fallback + e2e stdio. Commit: `feat(ai): reasoning via mcp sampling with lm-bridge fallback`.

### F0.6 — Dynamic tool discovery
Annuncio solo tool core se ticketing non configurato; `notifications/tools/list_changed` al cambio
config (anche via `settings_apply`). Commit: `feat(server): dynamic tool discovery for unconfigured ticketing`.

### F0.7 — MCP App "Ibis Assistant Memory Curator"
`ibis-httpserver`: asset statico `ui://ibis/memory-curator` (`text/html;profile=mcp-app`,
SDK `@modelcontextprotocol/ext-apps`, CSP solo loopback); `memory_search`/`memory_expand`/`memory_add`
tornano `_meta.ui.resourceUri` + **output testuale sempre presente**.
Verify: manuale in VS Code (MCP Apps experimental) + test fallback. Commit: `feat(server): memory curator mcp app with text fallback`.

**→ REVIEW R2**: smoke MCP in VS Code — zero conferme sui read-only, `/ibis.*` in chat,
resources in "Add Context", sampling attivo, App visibile.

## 6. FASE 1 — VSIX "Ibis Assistant" (parallela a Fase 2, dopo F0.2+)

| Sub | Contenuto | Commit |
|---|---|---|
| F1.1 | engines `^1.144.0`, `@types/vscode`, vsce 4.x, sync `@ibis/ide-host` | `build(vscode): engine 1.144 and vsce 4.x` |
| F1.2 | StatusBarItem (spawn/attach/bridge/provider, click → status, refresh on config) | `feat(vscode): status bar item` |
| F1.3 | TreeView "Ibis Assistant" (kinds → entries via `memory_search`, ricerca, dettaglio, refresh) | `feat(vscode): memory tree view` |
| F1.4 | **"Ibis Assistant: Remember Selection"** da `editor/context` (selezione + file + righe → `memory_add` con provenance) + variante file | `feat(vscode): remember selection with provenance` |
| F1.5 | `chatInstructions` "Ibis Assistant session contract" + walkthrough onboarding (4 step) | `feat(vscode): session-contract instructions and walkthrough` |
| F1.6 | CI publish: workflow OIDC (`vsce publish --oidc`), Open VSX (`ovsx`), `server.json` MCP registry, badge `vscode:mcp/install` | `build(vscode): oidc publishing, open vsx and mcp registry listing` |

**→ REVIEW R3** (azioni manuali ② ③ ⑤ prima): VSIX installata, UI verificata; publish su Marketplace + Open VSX + registry.

## 7. FASE 2 — Agent plugin "ibis" (parallela a Fase 1)

> Aggiornamento 30/10/2026: l'id del plugin è `ibis`
> e le skill portano nomi corti, così i comandi slash sono `/ibis usage`,
> `/ibis status`, … (la VS Code li renderizza come `<plugin> <skill>`).
> Nessun `commands/` duplicato: le skill sono l'unica superficie slash.

| Sub | Contenuto | Commit |
|---|---|---|
| F2.1 | `plugin/plugin.json` (Agent Plugins 1.0) + `plugin/mcp.json` (spawn `ibis -mode stdio -runtime-mode plugin`; attach `IBIS_MCP_URL`) | `feat(plugin): ibis agent plugin scaffold` |
| F2.2 | 5 skill (scaffold già in `plugin/skills/`): `usage` (contratto di sessione + mappa tool + igiene memoria), `readiness`, `status`, `remember`, `set-working-state` | `feat(plugin): skills incl. session-contract usage skill` |
| F2.3 | `com.github.copilot/hooks/hooks.json` + script: `sessionStart` → contratto, `agentStop` → `capture_observation` condizionale (verificare payload con Copilot hooks reference) | `feat(plugin): session start/stop hooks` |
| F2.4 | agents `ibis-forensics.agent.md` (read-only), `ibis-curator.agent.md` (la superficie slash resta solo le skill corte, senza `commands/` duplicati) | `feat(plugin): forensics/curator agents` |
| F2.5 | `automations/`: `nightly-ingest.automation.md`, `weekly-consolidate.automation.md` | `feat(plugin): ingest and consolidation automation templates` |
| F2.6 | Repo `fatterdev/ibis-marketplace` (marketplace.json schema Claude-compat) + README + badge `vscode://chat-plugin/install` + snippet team `.github/copilot/settings.json` | `feat(plugin): marketplace and workspace recommendations` |

**→ REVIEW R4**: plugin installato da sorgente (`chat.pluginLocations`), skill in `/`, hooks fire, MCP server del plugin avviato.

## 8. FASE 3 — Docs/posizionamento (dopo R3/R4)
README: "Ibis Assistant vs Copilot Memory" + feature matrix aggiornata; `docs/ide-integrations.md`
(matrice VS Code / Copilot CLI / Copilot App / Claude Code / CLI); fix drift documentale
(riferimenti Go in `CLAUDE_INTEGRATION.md`, `ide_host_and_lm_bridge.md`); skill sperimentale
`ibis-sync-copilot-memory`. Commit: `docs: positioning vs copilot memory and integration matrix`.
**→ REVIEW R5**: docs review.

## 9. TRACK D — Microsoft Scope (dopo F0.3–0.6 + F2.2; azione manuale ④)

Principi: infrastruttura **esterna al runtime** (docker-compose self-hosted, immagini pinned);
tutto config-as-code in `evaluations/scope/` (criteri, profili, task prompt, script di seed/run).
Scaffold già presente in `evaluations/scope/`.

| Sub | Contenuto | Commit |
|---|---|---|
| D1 | Vendor upstream compose, pin immagine, `seed.mjs` (registra MCP, importa skill da GitHub, seed criteria/profiles) | `chore(evals): scope config-as-code scaffold and seed` |
| D2 | `run.mjs` + `analysis.mjs`: matrice 2 worker (VS Code Copilot, Claude Code) × 3 profili (base / +Ibis MCP / +MCP+skills+hooks+VSIX) × N task su fixture OSS + repo ibis-assistant stesso | `chore(evals): benchmark runner and analysis` |
| D3 | **D-CLAIMS** esecuzione → findings. Claims: memoria→pass-rate/token; skills+hooks→igiene d'uso tool (1× `load_session_context`, kind corretti); annotations→attrito conferme; prompts→scoperta tool | `docs(evals): first benchmark results` |
| D4 | **D-INSIGHTS**: `scripts/scope-insights-import.mjs` (insights upvoted → `memory_add` tipizzato, dedup `source_hash`, ciclo di vita downvote/block → delete/supersede) | `feat(scripts): scope insights to ibis memory import` |
| D5 | `docs/scope-devax.md` metodologia + risultati; numeri nel README se favorevoli | `docs: devax methodology and evidence` |

Ciclo chiuso: **Scope finds → import → memoria Ibis → skill/rules/descrizioni migliorano → Scope re-measures**.
**→ REVIEW R6**: risultati D-CLAIMS → **gate di publish** del marketplace; D-INSIGHTS in loop continuo.

## 9-bis. TRACK B — Ingest tracce Beacon (in parallelo a Track D; approvata 03/11/2026)

> Fonte: analisi Agent Beacon (Asymptote-Labs) vs Microsoft Scope. Beacon cattura
> sessioni cross-harness (Cursor, Codex, OpenCode, Claude Code, Cline, 20+) e le
> normalizza in JSONL locale (`~/.beacon/endpoint/logs/runtime.jsonl`, user-mode;
> rotazione 10 MiB ×5 archivi; schema v1.0, `event.id` UUID deterministico,
> `event.fidelity` observed/inferred; Beacon già redige/tronca prima di scrivere).
> Principi: **script-first** (specchio della regola D-INSIGHTS di §9), **Beacon =
> cattura, Ibis = memoria tipizzata** — le tracce sono una fonte di ingest come
> logs/tickets e restano valide le decisioni "not chat-transcript storage" (README);
> **zero Beacon Cloud**; osservazioni deterministiche high-signal, la distillazione
> resta a `memory_consolidate` (nessuna AI nella lane); visibility configurabile,
> default `team`. Riferimenti docs: schema eventi `/telemetry-schema/event-schema`,
> JSONL locale `/log-forwarding/local-jsonl`, memoria `/concepts/cross-harness-memory`
> e `/cli/memory`, MCP locale `/cli/mcp-serve`.

| Sub | Contenuto | Commit |
|---|---|---|
| B0 | **Azione manuale ⑥** (endpoint Beacon) + campionamento `runtime.jsonl` reale su Windows: frequenze `event.action`, coverage `session.working_directory`/`repository`/`branch`, tasso redaction, volumi/rotazione → nota `evaluations/beacon/campionamento.md` + decisione mapping repo→progetto | — |
| B1 | `scripts/beacon-traces-import.mjs` (pattern `scripts/scope-insights-import.mjs`): read JSONL con offset+rotazione → filtri high-signal deterministici (approval denials con `fidelity=observed`; `command.exit_code≠0` ripetuti ≥N sulla stessa command; errori `mcp.tool_invoked`/tool; session digest bounded) → mapping `working_directory`→progetti ibis registrati → `memory_add` kind=observation (confidence 0.5, visibility da config, tag `trace:<harness>`, `source_hash=sha256("beacon:event:<id>")`; session id nel content — `provenance.session` pieno arriva con B3, `memory_add` non lo espone) → state file, dedup `event.id`, `--dry-run`, `--since`, `--min-repeat-failures` | `feat(scripts): beacon traces to ibis observation import` |
| B2 | `docs/planning/beacon-notes.md` (formato del §Scope in `vscode-platform-notes.md`) + **guard anti-overlap** (non registrare Beacon MCP/skills nei progetti ibis-managed: Beacon=cattura, Ibis=memoria) + riga README feature matrix se i numeri reggono | `docs: beacon trace ingest notes and guardrails` |
| B4 | `scripts/beacon-memory-import.mjs` (gemello di D-INSIGHTS): memoria Beacon **già approvata da umano** (`beacon memory list --json` / MCP loopback read-only `search_memory`) → mapping kind (workflow→rule, correction→fix, debugging_pattern→fix, gotcha→gotcha, convention→rule) → `memory_add` con `source_hash=sha256("beacon:memory:<id>")`; `--sync` (superseded/rejected → `memory_search`+`memory_delete`, stile scope:132-153) | `feat(scripts): beacon approved memory to ibis import` |
| B3 | **Promozione a lane nativa** (gate: ≥3 iterazioni utili di B1, regola §1 Track D): crate `ibis-ingest-traces` — Watcher/Tailer (pattern `ibis-ingest-logs/src/watcher.rs`), dedup tabella `trace_processed_event` (stile `log_processed_file`), config `session_traces` in `ibis-core/src/config.rs` + `config_master.json`, wiring `main.rs`/`assembly.rs` (pattern `start_log_watcher`), `provenance.session` pieno via `add_memory` core, test unit + `live_traces.rs` (sidecar SurrealDB + fake events, skip grazioso) | `feat(ingest): native beacon trace lane with typed provenance` |

Config B3 (default conservativi, lane **disabilitata** di default):

```json
{ "session_traces": { "enabled": false, "jsonl_paths": [], "visibility": "team",
    "min_repeat_command_failures": 3, "max_observations_per_session": 5,
    "include_session_digest": true } }
```

Ciclo chiuso: **sessioni cross-harness → osservazioni high-signal → consolidate →
memoria tipizzata sul grafo → recuperata dagli stessi harness via ibis MCP**;
B4 alimenta direttamente la memoria review-gated (stessa direzione di D-INSIGHTS).
Effort: B 2–3½g (B0..B4) + 2g (B3, solo se promossa).

**→ REVIEW R8**: idempotenza re-run (zero duplicati), zero prompt verbatim nel
content, cap per sessione rispettato, unknown-rate documentato, guard B2 presente.

## 10. Sequencing, versioning, effort

```
F0.1 (rename) → R1 → F0.2..F0.7 → R2 → [F1 ∥ F2] → R3/R4 (publish 1.0.0)
  → [F4-B (team server 0.3.0) ∥ F4-A (memory view VSIX 1.1.0)]
  → WQ-7 (SurrealDB 4 greenfield, crate+sidecar+image v4.0.0)
  → R7 → F3 → [D1..D3 ∥ B0..B2] → R6 (gate publish) → [D4/D5 ∥ B4] loop → B3 (se promossa)
```
(F4: sezione §13 — decisioni 01/11/2026, sequencing e commit nella
[spec F4](../superpowers/2026-11-01-memory-view-team-server-design.md).
WQ-7 davanti a R7: decisione owner 05/11, [master work queue §3.8](./2026-11-02-master-work-queue.md),
[spec SurrealDB 4](./2026-11-05-surrealdb-4-upgrade.md).)
- Runtime root: `0.2.0` a F0 completata (CHANGELOG `[Unreleased]` → `[0.2.0]`, tag); `0.3.0` a F4-B.
- VSIX resta `1.0.0` fino al publish; `1.1.0` a F4-A; plugin `0.1.0`.
- Effort stimato: F0.1 ½–1g · F0.2–0.7 2–4g · F1 2–4g · F2 1–2g · F4 2–3g ∥ 2–3g · F3 ½g · D 2–3g (+costi run agenti) · B 2–3½g (B0..B4) + 2g (B3, solo se promossa).

## 11. Rischi e mitigazioni
| Rischio | Mitigazione |
|---|---|
| Rename meccanico rompe il build | Script + `cargo test` + e2e gate + grep residui; lore "ibis" consentita solo in docs |
| Sampling variabile per client | Catena fallback sampling → LM bridge → Ollama |
| MCP Apps experimental | Degradazione testuale sempre presente; CSP solo loopback |
| Scope research-preview (usciuto 29/10/2026) | Immagini pinned, tutto config-as-code, nessuna hard dependency; gate R6 |
| Skill duplicati VSIX/plugin | Contratto di sessione: skill nel plugin, instructions nella VSIX |
| Schema Beacon in rapida evoluzione (v1.0, ~3k commit) | Check `schema_version`, parser tollerante (skip unknown), contatore unknown-rate nel report di import |
| Doppio layer memoria (Beacon MCP/skills nei progetti ibis) | Guard B2: Beacon=cattura, Ibis=memoria tipizzata; mai Beacon MCP/skills nei progetti ibis-managed |
| Privacy/volume tracce (prompt nel JSONL) | Mai prompt verbatim nel content (lezione + pointer), cap osservazioni/sessione, Beacon già redige; `session_traces.enabled=false` default |

## 12. Riferimenti piattaforma (ricerca 29/10/2026)
- VS Code 1.144 (28/10/2026): Agent Host + AHP v1.0, Agents window, Copilot harness default, assisted permissions GA, Copilot Memory GA, OIDC trusted publishing.
- MCP Dev Guide (corrente): `contributes.mcpServerDefinitionProviders` + `vscode.lm.registerMcpServerDefinitionProvider` validi; annotations (`readOnlyHint`), dynamic tool discovery, prompts, resources+templates, sampling, OAuth DCR, MCP Apps (`@modelcontextprotocol/ext-apps`), `vscode:mcp/install`, dev mode watch/debug.
- Agent Plugins 1.0 (agent-plugins.org): `plugin.json`, `skills/`, `mcp.json`, namespace `com.github.copilot/` (agents, commands, rules, hooks), `automations/*.automation.md`, `${PLUGIN_ROOT}`/`${PLUGIN_DATA}`, marketplace Git (schema Claude Code compat), deep link `vscode://chat-plugin/install`, raccomandazioni team via `.github/copilot/settings.json` (`extraKnownMarketplaces`, `enabledPlugins`).
- Hooks Copilot: eventi `sessionStart`, `userPromptSubmitted`, `preToolUse`, `postToolUse`, `preCompact`, `agentStop`, `sessionEnd`; command hook = JSON su stdin, decisione su stdout.
- Copilot Memory: repository facts cloud con TTL 28 giorni — posizionamento Ibis: memoria tipizzata, provenance git, self-hosted, grafo-aware.
- Contribution points correnti: `chatAgents`, `chatInstructions`, `chatPromptFiles`, `chatSkills`, `languageModelTools`, `languageModelChatProviders`; **Chat Participants deprecati**.
- Publishing: `vsce publish --oidc` (Trusted Publishing, GitHub Actions); Open VSX separato; MCP registry open-source con `server.json`.

## 13. FASE 4 — Memory view + Team server (dopo publish R3/R4; §13 prima di F3)

> Decisioni owner 01/11/2026 ([brainstorm §6](memory-view-team-server-brainstorm.md)) e
> design nella [spec F4](../superpowers/2026-11-01-memory-view-team-server-design.md).
> Due milestone: **F4-A** memory view (VSIX `1.1.0`), **F4-B** team server
> (runtime `0.3.0`). Decisivo: la VSIX passa a **backend daemon sempre-on**
> (attach, un solo runtime); il team server arriva con `ibis serve --team`,
> compose ufficiale, identity actor, consolidamento solo-daemon e
> reconciliation Layer 3. Sub-fasi e commit-message nella spec (F4-B1..B4,
> F4-A1..A5, 1 commit per sub-fase).

**→ REVIEW R7** (dopo WQ-7): team server sull'immagine **SurrealDB `v4.0.0`**
(compose o `serve --team` su LAN, volume greenfield) con 2+ client reali
attaccati (VSIX + plugin), attribution actor visibile sulle righe condivise,
nessun consolidamento duplicato, restart daemon pulisce i sidecar stale.
Checklist: [team_server.md §8](../team_server.md). Un pretest sulla pin
`v3.3.5` non chiude il gate.