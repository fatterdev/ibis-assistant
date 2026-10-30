# Mappa di naming — Threski Studio

Decisione: **rename globale** `ibis` → `threski`, prodotto = **Threski Studio**.
Le uniche eccezioni ammesse: il crate `mcp-bridge` (privo del prefisso) e i riferimenti
all'uccello "ibis" nella lore (intenzionali).

## Tabella completa

| Superficie | Prima | Dopo |
|---|---|---|
| Prodotto | Ibis Assistant | **Threski Studio** |
| Repo GitHub | `fatterdev/ibis-assistant` | `fatterdev/threski-studio` |
| Binario/CLI | `ibis-assistant` | `threski` |
| Crate Rust | `ibis-core`, `ibis-server`, `ibis-ai`, … | `threski-core`, `threski-server`, `threski-ai`, … (`mcp-bridge` invariato) |
| Env vars | `IBIS_MCP_URL`, `IBIS_BIN`, `IBIS_DATA_DIR`, `IBIS_WORKSPACE`, `IBIS_LM_BRIDGE_PORT/URL`, `IBIS_SETTINGS_URL` | `THRESKI_MCP_URL`, `THRESKI_BIN`, `THRESKI_DATA_DIR`, `THRESKI_WORKSPACE`, `THRESKI_LM_BRIDGE_PORT/URL`, `THRESKI_SETTINGS_URL` |
| Data dir | `%LOCALAPPDATA%\ibis-assistant\` | `%LOCALAPPDATA%\threski\` |
| MCP server label | "Ibis Assistant" | "Threski" |
| MCP prompts | — | `/threski.readiness`, `/threski.status`, `/threski.why_changed`, `/threski.blast_radius`, `/threski.remember` |
| MCP resources | `ibis://guide` | `threski://guide`, `threski://memory/{kind}/{id}`, `threski://rules/{name}`, `threski://ticket/{id}` |
| MCP App | — | "Threski Studio Memory Curator" (`ui://threski/memory-curator`) |
| VSIX | `fatterdev.ibis-assistant` | `fatterdev.threski-studio`, display "Threski Studio" |
| Settings VSIX | `ibis.*` (22 chiavi) | `threskiStudio.*` |
| Commands VSIX | `Ibis: *` (`ibis.openSettings`, …) | `Threski Studio: *` (`threskiStudio.openSettings`, …) |
| Agent plugin | — | `threski` (skill corte: `/threski usage`, `/threski status`, …) |
| Skill | `ibis-readiness` (Cursor) | `usage`, `readiness`, `status`, `remember`, `set-working-state` (slash `/threski <nome>`) |
| ide-host | `@ibis-assistant/ide-host`, bin `ibis-mcp-host.mjs`, clientInfo `ibis-ide-host/1.0.0` | `@threski/ide-host`, bin `threski-mcp-host.mjs`, clientInfo `threski-ide-host/1.0.0` |
| Cursor plugin | spawn `ibis-assistant`, `IBIS_*` | spawn `threski`, `THRESKI_*`, skill/commands `usage/-readiness/-status/-remember` |
| Marketplace plugin | — | repo `fatterdev/threski-marketplace`, deep link `vscode://chat-plugin/install?source=fatterdev/threski-marketplace&plugin=threski` |
| MCP registry | — | `server.json` name `threski` |
| Output channel / TreeView VSIX | "Ibis Assistant" | "Threski Studio" |

## Lore (da usare nel README del prodotto)

> The sacred ibis (*Threskiornis aethiopicus*) was the bird of Thoth, Egyptian god of
> writing, memory and knowledge — the scribe of the gods. **Threski** is the engine;
> **Threski Studio** is where you work with what it remembers.

## Regole di sostituzione (implementate in `scripts/migrate/migrate-rename.ps1`)

Ordine, dal più specifico al più generico:

1. `ibis-assistant` → `threski`
2. `IBIS_` → `THRESKI_`
3. `ibis://` → `threski://`
4. `Ibis Assistant` → `Threski Studio`
5. `@ibis-assistant/` → `@threski/`
6. `ibis-<token>` → `threski-<token>` (crate, binari, skill, file)
7. `ibis_<token>` → `threski_<token>` (moduli Rust `use ibis_core::…`)
8. chiavi JSON `ibis.` → `threskiStudio.` (settings e command id VSIX) e link `#ibis.x#` → `#threskiStudio.x#`
9. `\bibis\b` → `threski`; `\bIbis\b` → `Threski` (prosa; review manuale per la lore)