# threski-studio — agent plugin

Agent Plugins 1.0 per Threski Studio: skill, hook di sessione, agenti specializzati,
slash command e automation templates sopra il runtime MCP Threski.

## Installazione

Da marketplace (una volta pubblicato `fatterdev/threski-marketplace`):

```text
vscode://chat-plugin/install?source=fatterdev/threski-marketplace&plugin=threski-studio
```

Da sorgente: **Chat: Install Plugin from Source** → URL del repo o cartella locale `plugin/`.
Test locale senza installare: setting `chat.pluginLocations` con `{ "<path>/plugin": true }`.

Raccomandazione per team (nel repo del progetto):

```json
{
  "extraKnownMarketplaces": {
    "threski": { "source": { "source": "github", "repo": "fatterdev/threski-marketplace" } }
  },
  "enabledPlugins": { "threski-studio@threski": true }
}
```

## Runtime: spawn vs attach

- `plugin/mcp.json` (default): spawn di `threski -mode stdio -runtime-mode plugin`
  (binario `threski` sul PATH, plugin data dir isolata).
- Attach a un runtime personale condiviso: sostituire con
  `{ "type": "http", "url": "http://127.0.0.1:3030/mcp" }` — equivalente a esportare
  `THRESKI_MCP_URL`. Consigliato quando Cursor/CLI puntano allo stesso daemon.

## Componenti

| Percorso | Ruolo |
|---|---|
| `skills/threski-usage` | Contratto di sessione: quando e come usare Threski (1× `load_session_context`, mappa tool, igiene memoria) |
| `skills/threski-readiness` | Checklist primo uso / finestra vuota con tabella di readiness |
| `skills/threski-status` | Stato backend/AI/ticketing via `settings_get` (+ `ticket_get_capabilities` solo in diagnosi) |
| `skills/threski-remember` | Cattura memoria tipizzata con `memory_add` (7 kind, visibilità team/personal) |
| `skills/threski-set-working-state` | Stato di lavoro corrente via `set_working_state` |
| `com.github.copilot/hooks/hooks.json` | `sessionStart` → contratto; `agentStop` → promemoria `capture_observation` condizionale |
| `com.github.copilot/agents/` | `threski-forensics` (read-only: why_changed/blast radius/ask), `threski-curator` (curation memoria) |
| `com.github.copilot/commands/` | Slash command `/threski-status`, `/threski-remember` |
| `automations/` | `nightly-ingest` (cron 03:00), `weekly-consolidate` (cron ven 10:30) |

## Verifica post-installazione

1. Customizations → Plugins: `threski-studio` abilitato per l'harness target.
2. MCP server `threski` avviato (MCP: List Servers) — i server di plugin partono implicitamente fidati.
3. Nuova sessione: `/threski-status` risponde; skill compaiono in `/`; l'hook `sessionStart`
   inietta il contratto (verificare nei log/debug della sessione).

Nota: gli hook e l'exact schema degli output vanno verificati contro la
Copilot hooks reference (docs.github.com/copilot/reference/hooks-reference) — punto F2.3 di PLAN.md.