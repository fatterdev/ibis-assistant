# Mappa di naming — Ibis Assistant

Decisione (30/10/2026): **revert del rebrand** `threski` → `ibis`, prodotto = **Ibis Assistant**.
Nomi brevi: binario `ibis`, MCP label "Ibis" (slash `/ibis <nome>`), data dir `%LOCALAPPDATA%\ibis`.
Repo GitHub: `fatterdev/ibis-assistant` (redirect automatico da threski-studio). Conflitto con
l'omonimo framework Python ibis accettato (domini diversi).
Le uniche eccezioni ammesse: il crate `mcp-bridge` (privo del prefisso), il nome scientifico
*Threskiornis aethiopicus* nella lore (genere dell'ibis sacro) e lo schema ritirato
`threski://` usato come esempio di scheme esterno nei test.

## Tabella di revert

| Superficie | Prima (revert) | Dopo (revert) |
|---|---|---|
| Prodotto | Threski Studio | **Ibis Assistant** |
| Repo GitHub | `fatterdev/threski-studio` | `fatterdev/ibis-assistant` |
| Binario/CLI | `threski` | `ibis` |
| Crate Rust | `threski-core`, `threski-server`, … | `ibis-core`, `ibis-server`, … (`mcp-bridge` invariato) |
| Env vars | `THRESKI_*` | `IBIS_*` (`IBIS_MCP_URL`, `IBIS_BIN`, `IBIS_DATA_DIR`, `IBIS_WORKSPACE`, `IBIS_LM_BRIDGE_PORT/URL`, `IBIS_SETTINGS_URL`) |
| Data dir | `%LOCALAPPDATA%\threski\` | `%LOCALAPPDATA%\ibis\` (fresh start) |
| MCP server label | "Threski" | "Ibis" |
| MCP prompts | `/threski.readiness`, … | `/ibis.readiness`, `/ibis.status`, `/ibis.why_changed`, `/ibis.blast_radius`, `/ibis.remember` |
| MCP resources | `threski://guide`, `threski://memory/{kind}/{id}`, … | `ibis://guide`, `ibis://memory/{kind}/{id}`, `ibis://rules/{name}`, `ibis://ticket/{id}` |
| MCP App | "Threski Studio Memory Curator" (`ui://threski/memory-curator`) | "Ibis Assistant Memory Curator" (`ui://ibis/memory-curator`) |
| VSIX | `fatterdev.threski-studio`, display "Threski Studio" | `fatterdev.ibis-assistant`, display "Ibis Assistant" |
| Settings VSIX | `threskiStudio.*` (22 chiavi) | `ibis.*` |
| Commands VSIX | `Threski Studio: *` (`threskiStudio.openSettings`, …) | `Ibis Assistant: *` (`ibis.openSettings`, …) |
| Agent plugin | `threski` (skill corte) | `ibis` (skill corte: `/ibis usage`, `/ibis status`, …) |
| Skill | `usage`, `readiness`, `status`, `remember`, `set-working-state` | invariate (corte) |
| ide-host | `@threski/ide-host`, bin `threski-mcp-host.mjs`, clientInfo `threski-ide-host/1.0.0` | `@ibis/ide-host`, bin `ibis-mcp-host.mjs`, clientInfo `ibis-ide-host/1.0.0` |
| Cursor plugin | spawn `threski`, `THRESKI_*`, skill/commands `usage/-readiness/-status/-remember` | spawn `ibis`, `IBIS_*`, stessi nomi corti |
| Marketplace plugin | repo `fatterdev/threski-marketplace`, `plugin=threski` | repo `fatterdev/ibis-marketplace`, `plugin=ibis` |
| MCP registry | `server.json` name `threski` | `server.json` name `ibis` |
| Output channel / TreeView VSIX | "Threski Studio" | "Ibis Assistant" |

## Lore (da usare nel README del prodotto)

> The sacred ibis (*Threskiornis aethiopicus*) was the bird of Thoth, Egyptian god of
> writing, memory and knowledge — the scribe of the gods. **Ibis Assistant** is where
> you work with what it remembers.

## Regole di sostituzione (implementate in `scripts/migrate/rename-back-ibis.ps1`)

Ordine, dal più specifico al più generico (inverso della mappa F0.1):

1. `Threski Studio` → `Ibis Assistant`
2. `THRESKI_` → `IBIS_`
3. `threski://` → `ibis://`
4. `threskiStudio` → `ibis` (settings e command id VSIX)
5. `threski-studio` → `ibis-assistant` (repo, VSIX id)
6. `@threski/` → `@ibis/`
7. `__threski__` → `__ibis__` (namespacing tool `mcp__<server>__<tool>`)
8. `threski-<token>` → `ibis-<token>` (crate, binari, file)
9. `threski_<token>` → `ibis_<token>` (moduli Rust `use threski_core::…`)
10. `\bthreski\b` → `ibis`; `\bThreski\b` → `Ibis`; `\bTHRESKI\b` → `IBIS` (prosa;
    review manuale: *Threskiornis* resta — genere dell'ibis, non nome prodotto)

Storia: il rebrand originale `ibis → threski` (F0.1, 29/10/2026) è documentato in
`scripts/migrate/migrate-rename.ps1`; il revert usa lo stesso approccio in verso inverso.