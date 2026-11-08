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
| Per-task routing | D10: each call site pins `local:S/M/L/XL` from `model_overrides`; an explicit `model` or `always_smallest_local=true` keeps one local model |
| Cloud | Multi-cloud pool: Gemini, OpenAI-compatible, Claude; `routing.cloud_provider=auto` |
| CPU-only PCs | `use_cloud_when_no_gpu=true` — quality routes prefer cloud when keys exist |
| Plugin fallback | Context-only pack when local and cloud fail (`context_only_fallback`) |

AMD and NVIDIA are both supported for tier detection (Ollama ROCm/CUDA/Vulkan report + WMI). If a discrete GPU is present but Ollama runs on CPU, tier S is used and a diagnostic is logged.

### Local embedding pipeline (Ollama)

`POST /api/embed` calls carry three guards (`crates/ibis-ai/src/ollama.rs`); the
memory tree browses kinds with the embedding-free `memory_list` tool, so
semantic embeds fire only for real queries:

| Guard | Behavior |
|-------|----------|
| `keep_alive` | `"30m"` per call keeps `nomic-embed-text` resident (Ollama default `5m`; a cold reload costs ~4s). The API parameter overrides the server `OLLAMA_KEEP_ALIVE` for the embedding model — `IBIS_OLLAMA_KEEP_ALIVE` changes it. |
| Query cache | Identical input texts reuse the in-process embedding (cap 1024 entries, cleared wholesale when exceeded). |
| FIFO queue | One in-flight `/api/embed` per permit, default `1` (matches `OLLAMA_NUM_PARALLEL=1` and the Windows+Radeon single-model limit); `IBIS_OLLAMA_EMBED_CONCURRENCY` raises it when the local server allows more. Cache hits never queue. |

Ollama exposes no API for its concurrency settings (`/api/ps` and `/api/show`
carry no parallelism field), so the queue defaults to the safe value and is
overridable by hand.

### Persistent embedding cache (B6, opt-in)

The in-process map is L1; an **L2 disk cache on SurrealDB** survives process
restarts and re-indexes. Opt-in via `ai.embedding.disk_cache: true` (default
`false` — no behavior change on stock installs):

| Knob | Default | Behavior |
|------|---------|----------|
| `ai.embedding.disk_cache` | `false` | `OllamaProvider` consults the `embedding_cache` table (exact-key lookup, no ANN) **before** the HTTP batch and writes fresh vectors back after the response. The FIFO queue still gates only HTTP calls — an L2 hit never queues. |
| `ai.embedding.disk_cache_ttl_secs` | `2592000` (30d) | Boot-time prune of `embedding_cache` rows older than the TTL (`created_at`), same pattern as the `file_at_commit` prune. |

Cache key: `{model}:{dim}:{sha256(text)}` — model and dimension are inside the
key, so a model/dimension switch can never read stale vectors. The dimension
must match the schema's DiskANN `DIMENSION` (768); a row with a mismatched
length is treated as a miss. Store failures degrade transparently to the HTTP
path (the seam is best-effort, like `CostStore`). The L1 wholesale clear stays
as-is: the DB is strictly L2.

### Exact-match response cache (D12, opt-in)

The **exact-match tier** of the D12 response cache ships first; the semantic
tier is deliberately excluded (a wrong semantic hit is a silent quality
regression — it needs calibration data and an error metric first). Opt-in via
`ai.response_cache.enabled: true` (default `false`):

| Knob | Default | Behavior |
|------|---------|----------|
| `ai.response_cache.enabled` | `false` | Wraps the assembled reasoning stack in `ibis_ai::CachedGenerate`. Only calls that carry a `ResponseCacheSpec` consult the cache; everything else (including `ask_project`/agentic) is a transparent pass-through. |
| `ai.response_cache.ttl_secs` | `604800` (7d) | Boot-time prune of `llm_response_cache` rows older than the TTL (`created_at`). |

Only three deterministic call sites opt in:

| Call site | Invalidation context |
|-----------|----------------------|
| Semantic log analysis (`log_analyze`) | Resolved code-index HEAD (`head:<oid>`) |
| Ingest rule synthesis (`rule_synth`) | SHA-256 fingerprint of the `rules/*.yml` directory |
| ast-grep rule generation (`ast_rule_gen`) | SHA-256 fingerprint of the `rules/*.yml` directory |

Cache key: `{call_site}:{model}:{sha256(context, full_prompt)}` — the model
component is `AIProvider::cache_identity`, which composite providers override
with their routing fingerprint (tier models + cloud order), so a model switch
can never read a response cached under a different configuration. A hit
returns the stored text with no usage metadata (zero billed tokens). The store
is best-effort, like B6: a missing DB, a lookup failure or a disabled flag
degrades to the live provider; provider errors are never cached.

Runtime note: the decorator is installed when the AI stack is assembled. In
plugin mode a workspace bound *after* startup keeps the cache off until the
next start (the AI stack is not rebuilt on late binding), exactly like the B6
embedding cache.

## Prompt stability (D13)

Every cloud AI call site follows one written convention, enforced by
construction (the instructions block is a byte-stable `const` snapshot-tested
in its own crate):

> **byte-stable instructions block per call-site → variable context → query
> last**; no timestamp or request-id may enter the prefix.

| Call site | Crate | Stable prefix const |
|-----------|-------|---------------------|
| Semantic log analysis (`build_analyze_prompt`) | `ibis-ingest-logs` | `ANALYZE_INSTRUCTIONS` |
| Log-format discovery (`discover_format`) | `ibis-ingest-logs` | `DISCOVER_FORMAT_INSTRUCTIONS` |
| ast-grep rule generation (`build_ast_rule_prompt`) | `ibis-ingest-logs` | `AST_RULE_INSTRUCTIONS` |
| Ingest rule synthesis (`build_ingest_synth_prompt`) | `ibis-ingest-code` | `INGEST_SYNTH_INSTRUCTIONS` |
| Observation distillation (`build_distill_prompt`) | `ibis-optimization` | `DISTILL_INSTRUCTIONS` |

The extension fingerprint of rule synthesis and the proposed language of the
ast-rule prompt are variable content: they live **after** the stable prefix.

### Prompt-cache billing

`UsageMetadata` carries `cached_content_token_count` (provider-mapped: Gemini
`cachedContentTokenCount`, OpenAI-compat `prompt_tokens_details.cached_tokens`,
Claude `cache_read_input_tokens`). `CostTracker.record_usage` scales the billed
prompt side by the blended approximation `prompt − 0.9 × cached` (cache reads
bill at ~10% of full prompt price), so `tokens_used` accumulates **billed**
tokens and the debug log reports the saving. Blended rate stays $0.15/1M; no
per-model pricing in this phase. Anthropic `cache_control` is not sent in v1 —
v1 relies on provider-automatic caching + prefix stability.

## Per-task local model routing (D10)

Each LLM call site declares the local complexity tier it needs; the hybrid
router picks the matching `model_overrides` model instead of giving every task
the single auto-tier model. Hints are case-insensitive and keep the existing
cloud contract:

| Hint | First step | Fallback |
|------|-----------|----------|
| `local:S` / `local:M` / `local:L` / `local:XL` | local tier model | cloud (CPU-only + `use_cloud_when_no_gpu` prefers cloud first) |
| `local_only:<tier>` | local tier model | none (never cloud) |
| `quality` | cloud | local |
| `auto` / `bulk` / `bulk_local` / `local_only` | unchanged | unchanged |

| Call site | Crate | Hint |
|-----------|-------|------|
| Memory insight labels | `ibis-search` | `local:S` |
| Log root-cause refinement | `ibis-ingest-logs` | `local:S` |
| RAFT synthetic QA | `ibis-optimization` | `local:S` |
| Memory consolidation (team) | `ibis-optimization` | `local:M` |
| Memory consolidation (personal) | `ibis-optimization` | `local_only:M` |
| Bounded-decision shadow (team + personal) | `ibis-optimization` | `local_only:S` |
| Semantic log analysis / format discovery | `ibis-ingest-logs` | `local:M` |
| Ingest rule synthesis | `ibis-ingest-code` | `local:M` |
| ast-grep rule generation | `ibis-ingest-logs` | `local:M` |
| Agentic ReAct ask | `ibis-search` | `local:L` |

The tier map is built at startup from `ai.reasoning.model_overrides`; a tier
whose model equals the default local model is omitted (the router falls back to
the default provider), and an explicit `ai.reasoning.model` or
`always_smallest_local=true` pins the local lane to one model (no tier map).
Providers are cheap handles: Ollama pulls a tier model only when a call site
first uses it. One provider is pinned for the whole ReAct loop, so the model
never switches mid-task (affinity). Routing is logged at debug level
(`ibis_ai::router`, `hint`/`tier`/`local_model`) and the assembled map at
startup.

## Bounded decisions (D11)

Closed, repeated decisions ("is this observation noise?") use a typed
single-turn primitive instead of free-form JSON generation:
`decide(state, questions)` returns a probability distribution per question
plus the confidence margin (`max - second`). The caller owns the abstention
policy — auto above θ_auto, the current generative path in between, and a
conservative default below θ_escalate. The primitive never decides on its own.

`ai.decide.shadow` (default `true`) turns on shadow mode: the memory
consolidation computes the `is_noise` decision next to the generative verdict
and logs a `decide_shadow` record (`observation`, `rubric`, `decision_llm`,
`best`, `probs`, `confidence`, `agreement`) without changing behavior. The
shadow measurement routes `local_only:S` for team and personal observations
alike: it must calibrate the tier-S model, never a cloud fallback (a missing
tier-S model skips the pass with one warning). The deterministic C9 pre-match
stays zero-AI and is not shadowed.

The rubric is versioned (`is_noise.v1`) and pinned by the calibration dataset:

```bash
# Tier-S local model (Ollama)
ibis eval decisions --dataset evaluations/decisions/cases.yaml \
  --provider ollama --model granite4.1:3b --min-agreement 0.80

# Any OpenAI-compatible local server
ibis eval decisions --dataset evaluations/decisions/cases.yaml \
  --provider openai_compat --url http://127.0.0.1:8080/v1 \
  --model qwen3.8-27b --api-key local --min-agreement 0.80
```

The JSON report carries agreement, mean confidence and the
auto/escalate/default buckets at `--auto-threshold` (default 0.75) and
`--escalate-threshold` (default 0.40). The run is manual/owner (a live chat
model is required); `cargo test` validates the committed dataset and the
report math without a model. The 08/11 baseline
(`baseline-2026-11-08-qwen3.8-27b.json`) is 40/40 with agreement 0.975 on a
local `qwen3.8-27b` (the tier-S `granite4.1:3b` was not installed on the
measuring host); the single miss is in the default bucket (confidence 0.20)
and a prior run scored 1.000, so the residual is sampling variance at temp
0.001. The tier-S run remains the activation gate. Activation (routing the
consolidation decision through `decide()`) is a later phase driven by the
shadow agreement numbers.

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
      "auto_update": true,
      "disk_cache": false,
      "disk_cache_ttl_secs": 2592000
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
    },
    "response_cache": {
      "enabled": false,
      "ttl_secs": 604800
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

| Crate / module | Role |
|----------------|------|
| `crates/ibis-settings` | Probe, Recommend, Apply, `/settings` API + embedded UI |
| `crates/ibis-wizard` | CLI `config fast` / `full` |
| `crates/ibis-ai/src/ollama_chat.rs` | Local `/api/chat` |
| `crates/ibis-ai/src/openai_compat.rs` | OpenAI-compatible chat |
| `crates/ibis-ai/src/claude.rs` | Anthropic Messages API |
| `crates/ibis-ai/src/cloud_pool.rs` | Multi-cloud failover |
| `crates/ibis-ai/src/router.rs` | Hybrid ReasoningRouter |
| `crates/ibis-core/src/reasoning.rs` | Normalize + cloud helpers |

## Related history

- Embedding/reasoning interfaces: [ai_generalization_proposal.md](ai_generalization_proposal.md) (superseded for hybrid/multi-cloud by this guide)
- Original wizard proposal: [cli_and_wizard_proposal.md](cli_and_wizard_proposal.md) (`fast`/`full` replace Fast/Detailed naming)
- Runtime modes / plugin: [runtime_modes.md](runtime_modes.md)
- Typed memory layer: [memory_layer.md](memory_layer.md)
- Changelog: [changelog_20260919.md](changelog_20260919.md), [changelog_20261009.md](changelog_20261009.md)
