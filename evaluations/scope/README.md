# Track D — Threski DevAX benchmark con Microsoft Scope

Config-as-code per il benchmark descritto in PLAN.md §9. Tutto ciò che definisce gli
esperimenti vive in questo repo: criteri, profili, task prompt, script di seed/run.
Nessuna configurazione effimera nel Portal.

## Struttura

| File | Ruolo |
|---|---|
| `docker-compose.yml` | Istanza Scope self-hosted — **immagine da pinare a D1** (vendor del compose upstream e blocca il tag) |
| `.env.example` | `SCOPE_URL`, `SCOPE_API_KEY`, `THRESKI_MCP_URL` |
| `criteria.json` | Libreria criteri per i claims (gate Requirements/Build/Test, con dipendenze) |
| `profiles.json` | Matrice profili: `base` (no Threski) / `threski-mcp` (solo MCP server) / `threski-studio` (MCP + skill + hook + VSIX) |
| `task-prompts/` | Task prompt per il fixture repo (e variante per il repo threski stesso) |
| `seed.mjs` | Crea su Scope: MCP server `threski`, import skill da GitHub, criteria seed bulk, profili; scrive gli id in `.state/seed-result.json` |
| `run.mjs` | Sottomette la matrice worker × profilo × task, attende, scarica tool-calls JSONL |
| `analysis.mjs` | pass@k / success@T per profilo, delta, tabella finale |

## Claims misurati (D-CLAIMS)

1. **Memoria → outcome**: base vs `threski-mcp` (pass-rate, token, durata).
2. **Skill + hook → igiene tool**: `threski-mcp` vs `threski-studio`
   (1× `load_session_context`, kind corretti in `memory_add`, zero `settings_get` per turno)
   — verificato sui tool-calls JSONL.
3. **Annotations → attrito**: numero/tempi delle conferme tool nei run con annotations.
4. **Prompts → scoperta**: uso corretto dei tool con e senza `/threski.*`.

## Workflow

```bash
cp .env.example .env          # SCOPE_URL=..., SCOPE_API_KEY=...
docker compose up -d          # dopo aver pinato l'immagine (D1)
node seed.mjs                 # crea MCP server, import skill, criteria, profili
node run.mjs                  # sottomette la matrice
node analysis.mjs             # delta e tabella
node ../../scripts/scope-insights-import.mjs   # D-INSIGHTS → memoria Threski
```

## Da validare a D1 (scaffold)

I nomi esatti dei campi API vanno verificati contro l'istanza live:
`GET /api/v1/…` (reference: https://microsoft.github.io/scope/reference/rest-api/).
In particolare: `workerType` per Claude Code, schema criteria (`/reference/criteria-schema`),
schema profili (`/reference/profile-schema`), endpoint analysis. Gli script segnalano
gli errori di schema invece di indovinare.