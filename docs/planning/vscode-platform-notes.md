# Note di ricerca piattaforma — VS Code 1.144 (28 ottobre 2026)

Riferimenti chiave emersi dalla ricerca del 29/10/2026, usati per le decisioni in PLAN.md.

## MCP — dev guide corrente (validata al 28/10/2026)
- `contributes.mcpServerDefinitionProviders` + `vscode.lm.registerMcpServerDefinitionProvider`
  **restano l'API corrente** per registrare MCP server da estensioni (provider dinamico
  `McpStdioServerDefinition` / `McpHttpServerDefinition`, con `resolveMcpServerDefinition`
  per azioni che richiedono interazione utente).
- Feature MCP supportate da VS Code (spec 2025-06-18): tool annotations (`title`,
  `readOnlyHint` → niente conferma sui read-only), **dynamic tool discovery**,
  **prompts** come slash command `/server.prompt`, **resources + resource templates**
  (Add Context → MCP Resource; aggiornamenti real-time), **sampling**
  (`sampling/createMessage` con consenso utente e "Configure Model Access" per server),
  elicitation, OAuth 2.1/DCR, icons, **MCP Apps** (Tool + UI Resource pattern:
  tool torna `_meta.ui.resourceUri`, risorsa `ui://` con MIME
  `text/html;profile=mcp-app`, SDK `@modelcontextprotocol/ext-apps`, render inline-only,
  sandboxed iframe con CSP `connectDomains/resourceDomains/frameDomains`).
- Installazione: deep link `vscode:mcp/install?{json-config}`; gallery MCP in Extensions
  view con search `@mcp`; destinazioni config: Copilot Global `~/.copilot/mcp-config.json`,
  workspace `.mcp.json`; **`.vscode/mcp.json` etichettato deprecated** nella guided flow.
- Dev mode per server MCP: chiave `dev` con `watch` (glob) e `debug` (node/debugpy).
- Chat Participants: **deprecati** (guide "Chat Participants (Deprecated)").

## Agent Plugins 1.0 (agent-plugins.org — standard aperto)
- Struttura: `plugin.json` (`$schema: https://agent-plugins.org/schemas/1.0.0/plugin.schema.json`,
  `name` kebab-case), `skills/<skill>/SKILL.md` (name kebab-case = nome dir),
  `mcp.json` portabile (mcpServers top-level).
- Namespace client-specific: `com.github.copilot/` con `agents/*.agent.md`, `commands/`,
  `rules/`, `hooks/hooks.json`; altri client ignorano il namespace.
- `automations/*.automation.md` a root plugin: frontmatter `version/id/name/description/schedule`
  (kind: manual, hourly, cron 5-campi con `timeZone: local` per daily/weekly).
- Env: `${PLUGIN_ROOT}` e `${PLUGIN_DATA}` espansi in command/args/env/cwd/url/headers di
  MCP e hooks (in VS Code 1.144 il parser è `src/vs/platform/agentPlugins/common/pluginParsers.ts`).
- MCP server di plugin: partono quando il plugin è abilitato, **implicitamente fidati**.
- Distribuzione: marketplace = repo Git con `marketplace.json` (schema compatibile Claude
  Code); `chat.plugins.marketplaces` setting; raccomandazioni team via
  `.github/copilot/settings.json` (`extraKnownMarketplaces`, `enabledPlugins`, id
  `<plugin>@<marketplace>`); deep link `vscode://chat-plugin/install?source=owner/repo&plugin=nome`;
  install da sorgente (`Chat: Install Plugin from Source`); test locale via
  `chat.pluginLocations` (experimental).
- Agenti supportati oggi da Scope: GitHub Copilot, VS Code Copilot, Claude Code.

## Hooks Copilot (Preview in VS Code)
- Eventi: `sessionStart`, `userPromptSubmitted`, `preToolUse`, `postToolUse`, `preCompact`,
  `subagentStart/Stop`, `agentStop`, `sessionEnd`.
- Command hook: JSON evento su **stdin**, un oggetto JSON su **stdout**;
  `preToolUse` può decidere `permissionDecision: deny/ask`; `postToolUse` può aggiungere
  `additionalContext`; output di `userPromptSubmitted` ignorato (verificare per sessionStart).
- Sorgenti: user `~/.copilot/hooks/`, repo `.github/hooks/*.json`, settings inline,
  **plugin**, agent-scoped (frontmatter dell'agente).
- `timeoutSec`, `env`, fallback cross-platform `command` vs `bash`/`powershell`.

## Copilot Memory (competitore diretto della memory layer)
- Cloud: *repository facts* (citazioni sul codice, condivise per repo, richiede piano
  Copilot e repo GitHub) + *user preferences* (account, cross-repo). Recall a session
  start, upvote/downvote, **TTL 28 giorni se inutilizzata**, on-by-default.
- Local harness: memory file `/memories/`, `/memories/repo/`, `/memories/session/`.
- Posizionamento Threski: memoria **tipizzata** (decision/fix/rule/constraint/gotcha/design/fact),
  **supersession + history**, **provenance sul grafo git**, retrieval ibrida
  (vector+time+keyword+graph), legami con `why_changed`/blast radius, log e ticket,
  **self-hosted** con actor identity di team. Copilot Memory = preferenze/fatti cloud;
  Threski = knowledge graph di progetto.

## Contribution points correnti (rilevanti)
`chatAgents` (.agent.md), `chatInstructions` (istruzioni auto-attaccate quando rilevanti),
`chatPromptFiles`, `chatSkills` (SKILL.md secondo spec agentskills.io),
`languageModelTools` (`vscode.lm.registerTool`), `languageModelChatProviders`,
`views/viewsContainers/viewsWelcome`, `walkthroughs`, `menus`, `configuration`.

## Publishing
- **VS Marketplace**: publisher via Partner Center (account Microsoft, gratuito per
  estensioni); **Trusted Publishing**: `vsce publish --oidc` (vsce ≥ 4.0.2) da GitHub
  Actions con `id-token: write` — niente PAT come secret; global PAT di Azure DevOps in
  dismissione 12/2026.
- **Open VSX**: account separato Eclipse Foundation (`ovsx` CLI).
- **MCP registry**: open-source, `server.json`.
- Deep link install MCP: `vscode:mcp/install?{json-configuration-url-encoded}`.

## Microsoft Scope (research preview, 29/10/2026, MIT)
- Piattaforma di valutazione DevAX: task prompt + **criteria graph** (gate ordinati),
  **profiles versionate e immutabili** (workerType, model, `mcpServers` slugs,
  `skillRevisions` pinned a commit, `extensions` — solo worker VS Code Copilot),
  evidenze (trajectory ATIF, tool-calls JSONL, log, report con judge),
  metriche `pass@k/success@T`, **insights** con upvote/downvote/block.
- REST API completa (`/api/v1/…`): criteria seed bulk, profiles CRUD+versioni,
  skills import/discovery da repo GitHub, insights CRUD+vote, reports, analysis.
- Self-hosting: docker-compose (dev) o Kubernetes+KEDA+Flux (prod). Agenti oggi:
  Copilot, VS Code Copilot, Claude Code (Cursor non integrato).

## Riferimenti
- Release notes: https://code.visualstudio.com/updates/v1_144
- MCP dev guide: https://code.visualstudio.com/api/extension-guides/ai/mcp
- Agent plugins: https://code.visualstudio.com/docs/agent-customization/agent-plugins
- Plugin authoring: https://code.visualstudio.com/docs/agents/reference/agent-plugin-reference
- Hooks: https://code.visualstudio.com/docs/agent-customization/hooks
- MCP user docs: https://code.visualstudio.com/docs/agent-customization/mcp-servers
- Copilot Memory: https://code.visualstudio.com/docs/agents/run/memory
- Scope: https://github.com/microsoft/scope · https://microsoft.github.io/scope/
- Publishing OIDC: https://code.visualstudio.com/api/working-with-extensions/publishing-extension