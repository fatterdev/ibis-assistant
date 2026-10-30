# Hybrid AI and Settings UX

**Date:** 2026-09-19  
**Status:** Implemented  
**Design:** [superpowers/specs/2026-09-19-hybrid-reasoning-settings-design.md](superpowers/specs/2026-09-19-hybrid-reasoning-settings-design.md)  
**Plan:** [superpowers/plans/2026-09-19-hybrid-reasoning-settings.md](superpowers/plans/2026-09-19-hybrid-reasoning-settings.md)

Operator guide for embeddings, hybrid reasoning, multi-cloud keys, hardware auto-tier, CLI wizard, and the Settings UI.

## Defaults

| Area | Default |
|------|---------|
| Embeddings | Ollama `nomic-embed-text` (dedicated; never reuse chat models for vectors) |
| Reasoning | `provider=hybrid`, `model=auto` |
| Local auto-tier | S→`granite4.1:3b`, M→`qwen2.5-coder:7b`, L→`gemma4:12b`, XL→`muse-glimmer` |
| Cloud | Multi-cloud pool: Gemini, OpenAI-compatible, Claude; `routing.cloud_provider=auto` |
| CPU-only PCs | `use_cloud_when_no_gpu=true` — quality routes prefer cloud when keys exist |
| Plugin fallback | Context-only pack when local and cloud fail (`context_only_fallback`) |

AMD and NVIDIA are both supported for tier detection (Ollama ROCm/CUDA/Vulkan report + WMI). If a discrete GPU is present but Ollama runs on CPU, tier S is used and a diagnostic is logged.

## Plug-and-play keys

Enter API keys in the **wizard** or **Settings UI**; they are stored in `config.json` (plugin data dir when `runtime_mode=plugin`).

Env vars (`GEMINI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_BASE_URL`, …) are **optional overrides** only when file keys are empty. Do not treat ENV as the happy path.

## CLI

| Command | Behavior |
|---------|----------|
| `ibis config` | Same as `config fast` |
| `ibis config fast` | Probe + hybrid/local/cloud/context-only + keys + local model + paths |
| `ibis config full` | Fast plus DB, embed advanced, SMTP, ticketing, timeouts |
| `ibis config show` | Masked summary + `/settings` URL |
| `ibis config recommend` | JSON from SettingsEngine |

First launch with no `config.json` runs **fast**.

## Settings UI

Same static app for:

- Personal / server HTTP: `http://127.0.0.1:<port>/settings/`
- Cursor / plugin stdio: command **Ibis: Open Settings** → MCP `settings_open_ui` (ephemeral loopback exchanges a one-shot `?token=` for a same-site session cookie so CSS/JS/API requests succeed)
- VS Code **Ibis: Open Settings** probes personal HTTP, else calls MCP `settings_open_ui` through ide-host
  - Reuses personal `/settings/` when reachable on loopback
  - Otherwise starts an **ephemeral** `127.0.0.1:<random>` listener with a one-shot `?token=` (plugin mode only)

MCP tools: `settings_get`, `settings_apply`, `settings_open_ui`.

Screens: overview (probe), AI mode, cloud keys, local models (including IDE-provided models toggle), paths, **Memory** (browse/filter, personal↔team toggle, JSON export, optional `.ibis/memory/*.md` projection), save.

Memory projection (default off) is stored as:

```json
{
  "memory": {
    "projection_enabled": false
  }
}
```

SurrealDB remains the source of truth; projection is regenerable output only. Full memory operator guide: [memory_layer.md](memory_layer.md).

IDE packaging and shared runtime: [ide_host_and_lm_bridge.md](ide_host_and_lm_bridge.md).

## Config sketch

```json
{
  "ai": {
    "embedding": {
      "provider": "ollama",
      "model": "nomic-embed-text",
      "url": "http://127.0.0.1:11434",
      "auto_start": true,
      "auto_update": true
    },
    "reasoning": {
      "provider": "hybrid",
      "model": "auto",
      "context_only_fallback": true,
      "clouds": {
        "gemini": { "keys": [{ "key": "", "rpm": 100, "owner": "default" }] },
        "openai_compat": {
          "base_url": "https://api.openai.com/v1",
          "api_key": "",
          "model": "gpt-4.1"
        },
        "claude": { "api_key": "", "model": "claude-sonnet-4" }
      },
      "routing": {
        "mode": "auto",
        "local_provider": "ollama",
        "cloud_provider": "auto",
        "cloud_fallback_order": ["gemini", "openai_compat", "claude"],
        "use_cloud_when_no_gpu": true,
        "local_timeout_ms": 120000
      },
      "model_overrides": {
        "S": "granite4.1:3b",
        "M": "qwen2.5-coder:7b",
        "L": "gemma4:12b",
        "XL": "muse-glimmer"
      }
    }
  }
}
```

Legacy `ai.reasoning.keys` / top-level `gemini_keys` migrate into `clouds.gemini.keys` on load.

### `reasoning.provider`

| Value | Behavior |
|-------|----------|
| `hybrid` | Local Ollama + cloud pool (default outside plugin mode) |
| `ide_sampling` / `mcp_sampling` | Reasoning via MCP `sampling/createMessage` on the client session, fallback chain `sampling → lm_bridge → ollama` (default in plugin runtime mode) |
| `ollama` | Local only |
| `gemini` / `openai_compat` / `claude` / `cursor` | That provider only |
| `none` | Context-only (no GenerateContent) |

The `ide_sampling` chain skips legs that are not functional: no attached
sampling-capable MCP client (background work, clients without the
`capability.sampling` declaration) moves on to the LM bridge
(`IBIS_LM_BRIDGE_URL`, deprecated but still working), then to local
Ollama. With `context_only_fallback` on, an all-legs-failed request
degrades to the context-only pack instead of erroring.

## Code map

| Package / area | Role |
|----------------|------|
| `internal/settings` | Probe, Recommend, Apply, `/settings` API + embedded UI |
| `internal/wizard` | CLI `config fast` / `full` |
| `internal/ai/ollama_chat.go` | Local `/api/chat` |
| `internal/ai/openai_compat.go` | OpenAI-compatible chat |
| `internal/ai/claude.go` | Anthropic Messages API |
| `internal/ai/cloud_pool.go` | Multi-cloud failover |
| `internal/ai/router.go` | Hybrid ReasoningRouter |
| `internal/config/reasoning.go` | Normalize + cloud helpers |

## Related history

- Embedding/reasoning interfaces: [ai_generalization_proposal.md](ai_generalization_proposal.md) (superseded for hybrid/multi-cloud by this guide)
- Original wizard proposal: [cli_and_wizard_proposal.md](cli_and_wizard_proposal.md) (`fast`/`full` replace Fast/Detailed naming)
- Runtime modes / plugin: [runtime_modes.md](runtime_modes.md)
- Typed memory layer: [memory_layer.md](memory_layer.md)
- Changelog: [changelog_20260919.md](changelog_20260919.md), [changelog_20261009.md](changelog_20261009.md)
