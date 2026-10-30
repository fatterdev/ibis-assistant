# ibis — agent plugin

Agent Plugins 1.0 per Ibis: skill, hook di sessione, agenti specializzati
e automation templates sopra il runtime MCP Ibis.

## Installazione

Da marketplace (una volta pubblicato `fatterdev/ibis-marketplace`):

```text
vscode://chat-plugin/install?source=fatterdev/ibis-marketplace&plugin=ibis
```

Da sorgente: **Chat: Install Plugin from Source** → URL del repo o cartella locale `plugin/`.
Test locale senza installare: setting `chat.pluginLocations` con `{ "<path>/plugin": true }`.

Raccomandazione per team (nel repo del progetto):

```json
{
  "extraKnownMarketplaces": {
    "ibis": { "source": { "source": "github", "repo": "fatterdev/ibis-marketplace" } }
  },
  "enabledPlugins": { "ibis@ibis": true }
}
```

## Runtime: spawn vs attach

- `plugin/mcp.json` (default): spawn di `ibis -mode stdio -runtime-mode plugin`
  (binario `ibis` sul PATH, plugin data dir isolata).
- Attach a un runtime personale condiviso: sostituire con
  `{ "type": "http", "url": "http://127.0.0.1:3030/mcp" }` — equivalente a esportare
  `IBIS_MCP_URL`. Consigliato quando Cursor/CLI puntano allo stesso daemon.

## Componenti

| Percorso | Ruolo |
|---|---|
| `skills/usage` | Contratto di sessione: quando e come usare Ibis (1× `load_session_context`, mappa tool, igiene memoria) |
| `skills/readiness` | Checklist primo uso / finestra vuota con tabella di readiness |
| `skills/status` | Stato backend/AI/ticketing via `settings_get` (+ `ticket_get_capabilities` solo in diagnosi) |
| `skills/remember` | Cattura memoria tipizzata con `memory_add` (7 kind, visibilità team/personal) |
| `skills/set-working-state` | Stato di lavoro corrente via `set_working_state` |
| `com.github.copilot/hooks/hooks.json` | `sessionStart` → contratto; `agentStop` → promemoria `capture_observation` condizionale |
| `com.github.copilot/agents/` | `ibis-forensics` (read-only: why_changed/blast radius/ask), `ibis-curator` (curation memoria) |
| `automations/` | `nightly-ingest` (cron 03:00), `weekly-consolidate` (cron ven 10:30) |

## Verifica post-installazione

1. Customizations → Plugins: `ibis` abilitato per l'harness target.
2. MCP server `ibis` avviato (MCP: List Servers) — i server di plugin partono implicitamente fidati.
3. Nuova sessione: `/ibis status` risponde; le skill compaiono in `/` come `/ibis <nome>`; l'hook `sessionStart`
   inietta il contratto (verificare nei log/debug della sessione).

Nota: gli hook e l'exact schema degli output vanno verificati contro la
Copilot hooks reference (docs.github.com/copilot/reference/hooks-reference) — punto F2.3 di PLAN.md.