# Commit-meaning temporal layer

**Date:** 2026-10-09 (docs refreshed 2026-10-25)  
**Status:** **Merged to `master`** — #31 (`fecf21b`, 2026-10-25). The merged branch also carried **Layer 3 implementation** (phases 3a–3e, `as_of`/`knowledge_diff`/HOLDAt — [implementation state handoff](superpowers/handoffs/2026-10-24-layer3-implementation-state.md)) **and the git-tree claim store (addendum phases 4a–4d: codec, dual-write, bootstrap + policy knob, unified tree ∪ HOLDAt)**; **phase 5 (git notes + clone-only degraded reads)** via #32.  
**Plan:** [superpowers/plans/2026-10-09-commit-meaning-temporal-layer.md](superpowers/plans/2026-10-09-commit-meaning-temporal-layer.md)  
**Independent review (historical):** [temporal_freshness_plan_antigravity_review.md](temporal_freshness_plan_antigravity_review.md) — traps absorbed; claims about unwired staleness describe pre-#31 state.  
**Next layer (implemented):** git-indexed **world state at ref** (`as_of` / `knowledge_diff`) — [superpowers/specs/2026-10-24-git-indexed-world-state.md](superpowers/specs/2026-10-24-git-indexed-world-state.md) (phases 3a–3e + claim store 4a–4d shipped via #31; phase 5 — git notes + degraded reads — via #32; optional residue in the handoff [parking list](superpowers/handoffs/2026-10-24-layer3-implementation-state.md)). This doc (#31) is Layer 1–2 (Δ log + HEAD hygiene), **not** multi-truth Σ at every commit.  
**SoT decision (2026-10-24):** annotative Layer 3 as foundation **+ git-tree durable claim store adopted** — [ADR](superpowers/specs/2026-10-24-adr-claim-sot-annotative-layer3.md) + [addendum](superpowers/specs/2026-10-24-git-claim-store-addendum.md); bootstrap forward, no history rewrite; personal-claims knob (`stub` default personal, `db_only` team). [Parking handoff](superpowers/handoffs/2026-10-24-git-knowledge-sot-decision-parking.md) kept as decision trail.

## North star

Threski fights AI temporal blindness with **stacked layers** (see formal Layer 0–3 map in the [git-indexed world state spec](superpowers/specs/2026-10-24-git-indexed-world-state.md)):

1. **Δ Commit meaning (May + #31)** — each git commit historicized as `commit_chunk` (message + patch embeddings). Answers *why / when / what changed* (changelog dual).
2. **HEAD truth hygiene (Oct #28 + #31)** — typed memory with `status`, wall-clock `valid_until`, visibility, supersession, path-based stale vs HEAD file hash. Answers *what is true for the team **now** (at HEAD)*.
3. **Σ World at ref (proposed)** — `as_of(R)` / `knowledge_diff(A,B)` so multiple contemporaneous truths coexist on the git DAG. **Not shipped.**

Oct #28 is **logical currentness / concurrency of claims** (Memify), **not** git multi-truth. Confusing those two time axes (wall-clock + HEAD hash vs DAG ancestry) is how the May thread was lost.

Typed memory does **not** replace commit-meaning historicization. Outcome/path reinforcement (`save_reasoning_outcome`, RAFT) remains a thinner Dual-Loop signal, not the primary timeline.

## May vs October

| Era | What landed | What was incomplete |
|-----|-------------|---------------------|
| **2026-05-07** | Demote experimental `reinforce_path` tooling; produce `commit_chunk` from pending commits via BatchManager | `AskProject` never queried `commit_chunk` |
| **2026-10-09 #28** | Typed memory kinds, supersession, session tools, consolidate | Changelog claimed “git-driven staleness”; `RefreshStaleMemories` was tests-only; ask ignored memory status/visibility |

## Agent contract

| Question shape | Prefer |
|----------------|--------|
| How is the code **now**? | HEAD `file_chunk` (`is_current`) + **current team** memory |
| **Why / when** did it change? | `commit_chunk` via `ask_project` + `why_changed` |
| Durable team truth | `load_session_context` / `memory_search` (defaults current) |
| Forensics / history | `memory_history`, explicit non-current status |

## Freshness mechanics

- **HEAD currency:** `file_chunk.hash == source_file.hash` → `is_current`.
- **Commit decay:** score `commit_chunk` with `commit_date` (commit authorship time), never embedding ingest time.
- **Memory currentness:** default readers require `status=current` and open `valid_until`.
- **Memory privacy:** default ask uses `visibility=team`, or personal only for a trusted actor.
- **Git-driven stale:** after HEAD ingest, `RefreshStaleMemories` compares `provenance.source_hash` to the **current path’s** `source_file.hash` (path-based; prefers `is_head`; missing HEAD path → stale).
- **Provenance write path:** `memory_add` should set `source_hash` / `source_path` (or resolve from `source_file` refs); without them, refresh has nothing to invalidate.

## Limits (honest)

Wiring closes the May retrieval gap and production stale loop. It does **not** auto-solve temporal blindness if agents omit provenance, or if ingest fails to mark `is_head`. Full ingest→ask→stale E2E smoke is not a dedicated CI job (unit/grep coverage + manual smoke).

## Closing the remaining gap

Two different problems; do not conflate them.

### A. “Why / when did code change?” (commit meaning)

Already the primary fix: BatchManager writes `commit_chunk`; hybrid ask / `why_changed` retrieve them scored by `commit_date`. Dual-Loop / `save_reasoning_outcome` / path reinforce stay **secondary** — useful for “this answer helped”, not a substitute for the git timeline. No further product pivot needed here; keep agent prompts pointing at `ask_project` + `why_changed` for change forensics.

### B. “Is this team memory still true after HEAD moved?” (typed-memory stale)

Refresh only runs when a memory has `provenance.source_hash` (+ durable `source_path`) and ingest keeps a true HEAD via `source_file.is_head`. Close the loop as follows:

| Layer | What to do |
|-------|------------|
| **Write (agents/hooks)** | Always pass `source_refs` to `source_file:*` (or explicit `source_hash`/`source_path`) on `memory_add` / Remember / capture when the claim is about a file. Server already auto-fills hash/path from `source_file` refs when omitted (`resolveMemoryProvenanceFromSources`). |
| **Write (defaults)** | Prefer requiring at least one `source_file` ref for kinds that claim code truth (`decision`/`fix`/`gotcha` tied to paths); soft-warn or reject bare claims that should be path-bound. |
| **Ingest** | Keep clearing prior `is_head` and setting `is_head=true` on the live path row (already in code ingest). Guard with a regression test that HEAD lookup never falls back to a non-head hash-suffixed twin. |
| **Refresh** | Already path-based + missing-path→stale; call after every successful HEAD ingest (wired). |
| **Ask contract** | Default readers already gate `status=current` + `valid_until` + team visibility — stale memories drop out of “what is true now”. |
| **Verify** | Add one CI/smoke: ingest file @ hash H → memory with provenance H → re-ingest file @ H′ → `RefreshStaleMemories` marks stale → ask no longer returns it as current. |

Until B’s write discipline is habitual (or enforced), commit meaning (A) still answers change history; only **code-linked team memories** can silently stay `current` without provenance.

## Related docs

- [changelog_20260507.md](changelog_20260507.md) — May pivot
- [memory_layer.md](memory_layer.md) — typed memory operator guide
- [changelog_20261009.md](changelog_20261009.md) — #28 + honesty notes on staleness wiring
