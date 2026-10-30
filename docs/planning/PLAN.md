# Threski Studio — Piano di implementazione (bloccato)

> Stato: **approvato, pronto all'esecuzione**. Sorgente delle decisioni: sessione di
> ricerca e brainstorming del 29/10/2026 su VS Code 1.144, Agent Plugins 1.0, MCP,
> Copilot Memory e Microsoft Scope.
> Regola operativa: **nessuna modifica al repo sorgente `C:\devsrc\ibis-assistant`**
> (che viene letto in sola lettura dallo script di migrazione); il prodotto nasce qui.
> Aggiornamento 29/10/2026: **F0.1 completata** e runtime migrato promosso a root del
> prodotto; i riferimenti a `runtime/` nelle fasi vanno letti come root del repo.

## 1. Decisioni di prodotto

| Decisione | Esito |
|---|---|
| Prodotto | **Threski Studio** — living project memory for real codebases |
| Radice brand | `threski` (da *Threskiornis*, l'ibis sacro di Thoth) — rename **globale**, zero eccezioni |
| Crate Rust interni | `ibis-*` → `threski-*` (rinominati anche loro, nessuna eccezione; solo `mcp-bridge` resta perché privo del prefisso) |
| Env vars | `IBIS_*` → `THRESKI_*` |
| Binario/CLI | `threski` (`threski run`, `threski memory add`) |
| VSIX | `fatterdev.threski-studio`, display "Threski Studio", settings `threskiStudio.*` |
| Agent plugin | `threski` (Agent Plugins 1.0; skill corte → slash `/threski usage`), marketplace repo `fatterdev/threski-marketplace` |
| Reasoning | **MCP sampling** come via primaria; LM bridge custom deprecato ma mantenuto come fallback; Ollama fallback finale |
| MCP Apps | **Incluso nel primo rilascio** ("Memory Curator", con degradazione testuale sempre garantita) |
| API VS Code | engines `^1.144.0`, uso delle API stabili più recenti (`chatInstructions`, LM tools, MCP provider, MCP Apps) |
| Publishing | Full distribution: VS Marketplace (publisher `fatterdev`, OIDC Trusted Publishing) + Open VSX + MCP registry + plugin marketplace + `vscode:mcp/install` |
| Track D (Scope) | Sperimentale, config-as-code; **D-CLAIMS** = benchmark interno dei claims (gate pre-publishing); **D-INSIGHTS** = Scope Insights come fonte di memoria (import script-first, promozione a ingest lane solo dopo ≥3 iterazioni utili) |
| Skill duplicati | Il contratto di sessione vive come skill `usage` nel plugin e come `chatInstructions` nella VSIX — mai entrambi come skill; skill e comandi non si duplicano (le skill corte sono l'unica superficie slash `/threski <nome>`) |
| Skill sync Copilot Memory | Sì, sperimentale (`threski-sync-copilot-memory`) |

## 2. Mappa di naming

| Superficie | Nome |
|---|---|
| Repo GitHub | `fatterdev/threski-studio` (redirect automatico dal vecchio) |
| Binario/CLI | `threski` |
| Data dir | `%LOCALAPPDATA%\threski\` |
| MCP server (label per agenti) | "Threski"; prompts `/threski.<nome>`; risorse `threski://…` |
| MCP registry (`server.json`) | `threski` |
| VSIX | `fatterdev.threski-studio` — "Threski Studio" |
| Plugin | `threski` — skill `usage/-readiness/-status/-remember/-set-working-state` (slash `/threski <nome>`; label MCP VSIX "Threski") |
| Cursor plugin | binario `threski`, env `THRESKI_*`, skill/commands `threski-*` |
| ide-host | `@threski/ide-host`, default `THRESKI_BIN="threski"`, clientInfo `threski-ide-host/1.0.0` |
| Crate Rust | `threski-core`, `threski-server`, `threski-ai`, … (`mcp-bridge` invariato) |
| MCP App | "Threski Studio Memory Curator" (`ui://threski/memory-curator`) |
| Lore README | Thoth, l'ibis sacro (*Threskiornis aethiopicus*), Casa della Vita — i riferimenti all'uccello "ibis" nei testi di lore sono **intenzionali** |

## 3. Convenzioni di lavoro

- Commit convenzionali lowercase, in stile repo: `feat:`, `fix:`, `docs:`, `refactor!:`, `chore(evals):` — **1 commit per sottofase**, mai mega-commit.
- Root `CHANGELOG.md`: sezione `## [Unreleased]` accumulata durante le fasi, rilasciata ai gate come `0.2.0`.
- Verifica obbligatoria `cargo test` (786+ test) a ogni tocco dei crate; `cargo xtask dist` (layout gate) e `vsce package` dove indicato.
- Review gate = checkpoint con l'owner prima di procedere oltre.

## 4. Azioni manuali dell'owner (fuori codice)

1. Rename repo GitHub → `threski-studio` (redirect automatico) + update remote locale.
2. Partner Center: publisher `fatterdev` + policy **Trusted Publishing** (OIDC da GitHub Actions, vsce ≥ 4.0.2).
3. Account Eclipse Foundation per **Open VSX** (canale separato, non Microsoft).
4. Istanza **Scope** (docker-compose self-hosted) + preview/API key.
5. Installazione locale di VSIX e plugin per i review R3/R4.

## 5. FASE 0 — Runtime (Rust)

### F0.1 — Global rename `ibis` → `threski` (primo commit del prodotto)
Strumento: `scripts/migrate/migrate-rename.ps1` (legge il repo sorgente in sola lettura, scrive l'albero migrato in `<repo>/runtime/`).

| Sub | Azioni | Verifica | Commit |
|---|---|---|---|
| a | Rinomina directory + package dei crate `crates/ibis-*` → `crates/threski-*` (core, logger, git, db, ai, ingest-*, search, ticketing-*, repopr, settings, wizard, cli, httpserver, server, optimization, xtask; `mcp-bridge` invariato), workspace `Cargo.toml`, `use ibis_*` → `use threski_*`, fixture, test | `cargo test` green | `refactor!: rename crates ibis-* to threski-*` |
| b | Binario: xtask → `dist/threski.exe`; env `THRESKI_*`; data dir `threski`; schema risorse `threski://`; clientInfo; wizard/testi CLI; default binario | `cargo test` + `cargo xtask dist` | `refactor!: binary, env vars and data dirs to threski` |
| c | ide-host: `@threski/ide-host`, `THRESKI_BIN`, clientInfo; cursor-plugin: `mcp.json`, env, skill/commands `threski-*`, README/STATUS | framing test + `vsce package` | `refactor: ide surfaces to threski studio` |
| d | VSIX: id/display/settings `threskiStudio.*`/commands "Threski Studio: *"/output channel/icona | `vsce package` + install locale | (unito a c) |
| e | Docs: README (lore), `docs/*`, CHANGELOG root+vscode, scan `config_master.json`/`config/` | grep `ibis` residuo = solo lore | `docs: rebrand to threski studio with sacred-ibis lore` |
| f | Azione manuale ①: rename repo GitHub + remote | — | — |

**→ REVIEW R1**: cargo test green, dist builds, VSIX installa, repo rinominato.

### F0.2 — Tool annotations
`threski-server/src/registry.rs`: `readOnlyHint: true` + `title` per tutti i read-only
(`ask_project`, `analyze_blast_radius`, `find_dead_code`, `memory_search/expand/history/insights`,
`why_changed`, `as_of_project`, `knowledge_diff`, `get_working_state`, `load_session_context`,
`recent_failures`, `settings_get`, `ticket_get/search/search_my/list_statuses/list_projects/search_users/get_capabilities`,
`get_rules`); esplicito `readOnlyHint: false` per i mutanti. Test di completezza (ogni tool dichiara l'annotation).
Commit: `feat(server): read-only tool annotations (readonlyhint, titles)`.

### F0.3 — MCP prompts
`prompts/list` + `prompts/get`: `readiness`, `status`, `why_changed`, `blast_radius`, `remember`
(testi derivati dal contratto di sessione, argomenti con completions). Test integrazione protocollo.
Commit: `feat(server): mcp prompts (/threski.*)`.

### F0.4 — MCP resources + templates
`resources/list/read` + templates `threski://memory/{kind}/{id}`, `threski://rules/{name}`,
`threski://ticket/{id}`; `resources/subscribe` + notifiche `resources/updated` su mutazioni memoria (lane SSE).
Commit: `feat(server): memory, rules and tickets as mcp resources (templates + live updates)`.

### F0.5 — Reasoning via MCP sampling
`threski-ai`: provider `mcp_sampling` (request `sampling/createMessage` sul canale MCP del client),
catena fallback `sampling → lm_bridge → ollama`; setting `ai.reasoning.provider`
(default `ide_sampling` in plugin mode); LM bridge deprecato ma funzionante (`THRESKI_LM_BRIDGE_*` solo fallback).
Verify: unit test fallback + e2e stdio. Commit: `feat(ai): reasoning via mcp sampling with lm-bridge fallback`.

### F0.6 — Dynamic tool discovery
Annuncio solo tool core se ticketing non configurato; `notifications/tools/list_changed` al cambio
config (anche via `settings_apply`). Commit: `feat(server): dynamic tool discovery for unconfigured ticketing`.

### F0.7 — MCP App "Threski Studio Memory Curator"
`threski-httpserver`: asset statico `ui://threski/memory-curator` (`text/html;profile=mcp-app`,
SDK `@modelcontextprotocol/ext-apps`, CSP solo loopback); `memory_search`/`memory_expand`/`memory_add`
tornano `_meta.ui.resourceUri` + **output testuale sempre presente**.
Verify: manuale in VS Code (MCP Apps experimental) + test fallback. Commit: `feat(server): memory curator mcp app with text fallback`.

**→ REVIEW R2**: smoke MCP in VS Code — zero conferme sui read-only, `/threski.*` in chat,
resources in "Add Context", sampling attivo, App visibile.

## 6. FASE 1 — VSIX "Threski Studio" (parallela a Fase 2, dopo F0.2+)

| Sub | Contenuto | Commit |
|---|---|---|
| F1.1 | engines `^1.144.0`, `@types/vscode`, vsce 4.x, sync `@threski/ide-host` | `build(vscode): engine 1.144 and vsce 4.x` |
| F1.2 | StatusBarItem (spawn/attach/bridge/provider, click → status, refresh on config) | `feat(vscode): status bar item` |
| F1.3 | TreeView "Threski Studio" (kinds → entries via `memory_search`, ricerca, dettaglio, refresh) | `feat(vscode): memory tree view` |
| F1.4 | **"Threski Studio: Remember Selection"** da `editor/context` (selezione + file + righe → `memory_add` con provenance) + variante file | `feat(vscode): remember selection with provenance` |
| F1.5 | `chatInstructions` "Threski Studio session contract" + walkthrough onboarding (4 step) | `feat(vscode): session-contract instructions and walkthrough` |
| F1.6 | CI publish: workflow OIDC (`vsce publish --oidc`), Open VSX (`ovsx`), `server.json` MCP registry, badge `vscode:mcp/install` | `build(vscode): oidc publishing, open vsx and mcp registry listing` |

**→ REVIEW R3** (azioni manuali ② ③ ⑤ prima): VSIX installata, UI verificata; publish su Marketplace + Open VSX + registry.

## 7. FASE 2 — Agent plugin "threski" (parallela a Fase 1)

> Aggiornamento 30/10/2026: l'id del plugin è `threski` (non `threski-studio`)
> e le skill portano nomi corti, così i comandi slash sono `/threski usage`,
> `/threski status`, … (la VS Code li renderizza come `<plugin> <skill>`).
> Nessun `commands/` duplicato: le skill sono l'unica superficie slash.

| Sub | Contenuto | Commit |
|---|---|---|
| F2.1 | `plugin/plugin.json` (Agent Plugins 1.0) + `plugin/mcp.json` (spawn `threski -mode stdio -runtime-mode plugin`; attach `THRESKI_MCP_URL`) | `feat(plugin): threski agent plugin scaffold` |
| F2.2 | 5 skill (scaffold già in `plugin/skills/`): `usage` (contratto di sessione + mappa tool + igiene memoria), `readiness`, `status`, `remember`, `set-working-state` | `feat(plugin): skills incl. session-contract usage skill` |
| F2.3 | `com.github.copilot/hooks/hooks.json` + script: `sessionStart` → contratto, `agentStop` → `capture_observation` condizionale (verificare payload con Copilot hooks reference) | `feat(plugin): session start/stop hooks` |
| F2.4 | agents `threski-forensics.agent.md` (read-only), `threski-curator.agent.md` (la superficie slash resta solo le skill corte, senza `commands/` duplicati) | `feat(plugin): forensics/curator agents` |
| F2.5 | `automations/`: `nightly-ingest.automation.md`, `weekly-consolidate.automation.md` | `feat(plugin): ingest and consolidation automation templates` |
| F2.6 | Repo `fatterdev/threski-marketplace` (marketplace.json schema Claude-compat) + README + badge `vscode://chat-plugin/install` + snippet team `.github/copilot/settings.json` | `feat(plugin): marketplace and workspace recommendations` |

**→ REVIEW R4**: plugin installato da sorgente (`chat.pluginLocations`), skill in `/`, hooks fire, MCP server del plugin avviato.

## 8. FASE 3 — Docs/posizionamento (dopo R3/R4)
README: "Threski Studio vs Copilot Memory" + feature matrix aggiornata; `docs/ide-integrations.md`
(matrice VS Code / Copilot CLI / Copilot App / Claude Code / CLI); fix drift documentale
(riferimenti Go in `CLAUDE_INTEGRATION.md`, `ide_host_and_lm_bridge.md`); skill sperimentale
`threski-sync-copilot-memory`. Commit: `docs: positioning vs copilot memory and integration matrix`.
**→ REVIEW R5**: docs review.

## 9. TRACK D — Microsoft Scope (dopo F0.3–0.6 + F2.2; azione manuale ④)

Principi: infrastruttura **esterna al runtime** (docker-compose self-hosted, immagini pinned);
tutto config-as-code in `evaluations/scope/` (criteri, profili, task prompt, script di seed/run).
Scaffold già presente in `evaluations/scope/`.

| Sub | Contenuto | Commit |
|---|---|---|
| D1 | Vendor upstream compose, pin immagine, `seed.mjs` (registra MCP, importa skill da GitHub, seed criteria/profiles) | `chore(evals): scope config-as-code scaffold and seed` |
| D2 | `run.mjs` + `analysis.mjs`: matrice 2 worker (VS Code Copilot, Claude Code) × 3 profili (base / +Threski MCP / +MCP+skills+hooks+VSIX) × N task su fixture OSS + repo threski stesso | `chore(evals): benchmark runner and analysis` |
| D3 | **D-CLAIMS** esecuzione → findings. Claims: memoria→pass-rate/token; skills+hooks→igiene d'uso tool (1× `load_session_context`, kind corretti); annotations→attrito conferme; prompts→scoperta tool | `docs(evals): first benchmark results` |
| D4 | **D-INSIGHTS**: `scripts/scope-insights-import.mjs` (insights upvoted → `memory_add` tipizzato, dedup `source_hash`, ciclo di vita downvote/block → delete/supersede) | `feat(scripts): scope insights to threski memory import` |
| D5 | `docs/scope-devax.md` metodologia + risultati; numeri nel README se favorevoli | `docs: devax methodology and evidence` |

Ciclo chiuso: **Scope finds → import → memoria Threski → skill/rules/descrizioni migliorano → Scope re-measures**.
**→ REVIEW R6**: risultati D-CLAIMS → **gate di publish** del marketplace; D-INSIGHTS in loop continuo.

## 10. Sequencing, versioning, effort

```
F0.1 (rename) → R1 → F0.2..F0.7 → R2 → [F1 ∥ F2] → R3/R4 → F3 → D1..D3 → R6 (gate publish) → D4/D5 loop
```
- Runtime root: `0.2.0` a F0 completata (CHANGELOG `[Unreleased]` → `[0.2.0]`, tag).
- VSIX resta `1.0.0` fino al publish; plugin `0.1.0`.
- Effort stimato: F0.1 ½–1g · F0.2–0.7 2–4g · F1 2–4g · F2 1–2g · F3 ½g · D 2–3g (+costi run agenti).

## 11. Rischi e mitigazioni
| Rischio | Mitigazione |
|---|---|
| Rename meccanico rompe il build | Script + `cargo test` + e2e gate + grep residui; lore "ibis" consentita solo in docs |
| Sampling variabile per client | Catena fallback sampling → LM bridge → Ollama |
| MCP Apps experimental | Degradazione testuale sempre presente; CSP solo loopback |
| Scope research-preview (usciuto 29/10/2026) | Immagini pinned, tutto config-as-code, nessuna hard dependency; gate R6 |
| Skill duplicati VSIX/plugin | Contratto di sessione: skill nel plugin, instructions nella VSIX |

## 12. Riferimenti piattaforma (ricerca 29/10/2026)
- VS Code 1.144 (28/10/2026): Agent Host + AHP v1.0, Agents window, Copilot harness default, assisted permissions GA, Copilot Memory GA, OIDC trusted publishing.
- MCP Dev Guide (corrente): `contributes.mcpServerDefinitionProviders` + `vscode.lm.registerMcpServerDefinitionProvider` validi; annotations (`readOnlyHint`), dynamic tool discovery, prompts, resources+templates, sampling, OAuth DCR, MCP Apps (`@modelcontextprotocol/ext-apps`), `vscode:mcp/install`, dev mode watch/debug.
- Agent Plugins 1.0 (agent-plugins.org): `plugin.json`, `skills/`, `mcp.json`, namespace `com.github.copilot/` (agents, commands, rules, hooks), `automations/*.automation.md`, `${PLUGIN_ROOT}`/`${PLUGIN_DATA}`, marketplace Git (schema Claude Code compat), deep link `vscode://chat-plugin/install`, raccomandazioni team via `.github/copilot/settings.json` (`extraKnownMarketplaces`, `enabledPlugins`).
- Hooks Copilot: eventi `sessionStart`, `userPromptSubmitted`, `preToolUse`, `postToolUse`, `preCompact`, `agentStop`, `sessionEnd`; command hook = JSON su stdin, decisione su stdout.
- Copilot Memory: repository facts cloud con TTL 28 giorni — posizionamento Threski: memoria tipizzata, provenance git, self-hosted, grafo-aware.
- Contribution points correnti: `chatAgents`, `chatInstructions`, `chatPromptFiles`, `chatSkills`, `languageModelTools`, `languageModelChatProviders`; **Chat Participants deprecati**.
- Publishing: `vsce publish --oidc` (Trusted Publishing, GitHub Actions); Open VSX separato; MCP registry open-source con `server.json`.