# Checklist F0.1 — Global rename `ibis` → `threski`

Prerequisiti: repo sorgente `C:\devsrc\ibis-assistant` presente (lettura sola, mai
modificato); questo repo pulito (il prodotto migrato atterra in `runtime/`, gitignored).

## 1. Migrazione meccanica

```powershell
pwsh scripts/migrate/migrate-rename.ps1 -Source C:\devsrc\ibis-assistant -Destination C:\devsrc\threski-studio\runtime -Force
```

Lo script: copia l'albero (escludendo `.git`, `target`, `node_modules`, `dist`,
`.ruff_cache`, log), rinomina le directory dei crate `ibis-*` → `threski-*`, applica le
regole di sostituzione in ordine (vedi `docs/planning/naming-map.md` §Regole) e stampa i residui
`ibis` da revisionare manualmente.

## 2. Verifica build

```bash
cd runtime
cargo test          # estate 786+ test
cargo xtask dist    # produce dist/threski.exe (layout gate)
cd vscode-extension
npm run package     # VSIX con @threski/ide-host vendored
```

## 3. Revisione manuale (residui attesi)

- **Lore**: i riferimenti all'uccello "ibis" nel README (`Threskiornis`, Thoth) sono
  **intenzionali** — non "correggerli".
- `CHANGELOG.md` (root e vscode-extension): aggiungere la sezione `0.2.0` con la nota
  di rebrand.
- `config_master.json` e `config/`: confermare che non restino chiavi/valori con `ibis`
  (lo script li ha già sostituiti; qui si controlla il senso, non il testo).
- Wizard/CLI: testi utente con il nuovo nome prodotto "Threski Studio".

## 4. Commit (dal tree `runtime/`, che diventerà il prodotto)

```text
refactor!: rename crates ibis-* to threski-*
refactor!: binary, env vars and data dirs to threski
refactor: ide surfaces to threski studio (ide-host, cursor plugin, vsix)
docs: rebrand to threski studio with sacred-ibis lore
```

## 5. Review gate R1 — **chiuso 29/10/2026**

- [x] cargo test green — 71 suite, 799 passed / 0 failed (1 test flaky timing in
  `threski-ai`, verde al re-run; pre-esistente)
- [x] cargo xtask dist → `dist/threski.exe` (smoke: `threski 0.1.0`)
- [x] VSIX installa — `fatterdev.threski-studio-1.0.0.vsix` (ide-host vendored);
  la registrazione del provider MCP in VS Code 1.144 si verifica all'uso (owner)
- [x] grep residui: solo lore (README "Why the name" + note storiche CHANGELOG 0.2.0)
- [x] owner: rename repo GitHub → `fatterdev/threski-studio` (redirect attivo,
  verificato via API) + default branch `main`
- CI su `main`: run d584594 **green** (rust windows+ubuntu con clippy -D warnings,
  ide-host framing test) — 29/10/2026
- Promozione: `runtime/` promosso a root del prodotto (commit 8f8b920); planning
  kit in `docs/planning/`; remote `origin` = `github-personale` (chiave fatterdev)

Quando `runtime/` viene promosso a root del prodotto, spostare i file di piano/docs
secondo la nuova layout e aggiornare README/PLAN. — **fatto** (29/10/2026).