# Task prompts (fixture repo)

Task prompts per il benchmark D-CLAIMS. Ogni task è eseguito da ogni profilo
(base / ibis-mcp / ibis-assistant-full) sugli stessi criteri (criteria.json).
Il fixture è un repo OSS piccolo; una variante replica i task sul repo ibis stesso.

## T1 — Fix a failing test

```text
The test <test-name> in this repository fails on <branch>. Reproduce it, identify the
root cause, fix it minimally, and make the whole suite pass. Do not change public APIs.
```

Criteri primari: c-task-completed, c-build-succeeds, c-tests-pass, c-ibis-grounded-answers.

## T2 — Add a small feature following conventions

```text
Add <small-feature> (e.g. a `--json` output flag to the CLI command <cmd>) following
the existing conventions of this codebase. Include tests. Update the changelog.
```

Criteri primari: c-task-completed, c-build-succeeds, c-tests-pass, c-ibis-memory-hygiene.

## T3 — Change forensics before refactor

```text
We plan to rename <symbol/module>. Before any change, report: why this code exists in
its current form, which areas are impacted by the rename, and any known constraints.
Then perform the rename with tests green.
```

Criteri primari: c-ibis-session-contract, c-ibis-grounded-answers, c-task-completed, c-tests-pass.

## T4 — Close the loop with memory

```text
Complete <task>. When done, record in project memory the durable lesson a teammate
would need (decision, fix, constraint or gotcha) — nothing more.
```

Criteri primari: c-ibis-memory-hygiene, c-ibis-session-contract, c-task-completed.

## Varianti per il repo ibis stesso

- F0/T3: "Why does the MCP registry announce tools this way?" (forensics sul crate server)
- F2/T1: fix di un test reale del runtime, con criteri Build/Test del workspace Cargo.

## Variabili di profilo da misurare (oltre ai criteri)

- Token totali e durata per profilo (da Scope).
- Numero di chiamate per tool: `load_session_context` (target 1), `settings_get` (target 0 fuori diagnosi), `memory_add` con kind valido.
- Conferme utente richieste per tool read-only Ibis (target 0).