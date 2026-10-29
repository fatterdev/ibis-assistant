# ADR: Claim Source of Truth — hybrid: annotative Layer 3 (A) as foundation + git-tree durable store (B)

**Date:** 2026-10-24
**Status:** Accepted (initial decision by operator delegation; **amended same day** after operator feasibility correction — see §3)
**Supersedes the open fork in:** [2026-10-24-git-knowledge-sot-decision-parking.md](../handoffs/2026-10-24-git-knowledge-sot-decision-parking.md) §6
**Adopts:**
- [2026-10-24-git-indexed-world-state.md](2026-10-24-git-indexed-world-state.md) (Layer 3 spec, G1–G4 answered) — foundation, implementation contract
- [2026-10-24-git-claim-store-addendum.md](2026-10-24-git-claim-store-addendum.md) — Option B design (file language, policy, bootstrap)

---

## 1. Context

Layer 3 (world state at ref: `as_of` / `knowledge_diff` / HOLDAt) raised a storage question the Layer 3 spec deliberately did not answer: **what is the Source of Truth for durable team claims?**

Three options were framed (parking doc §6):

- **A — Annotative Layer 3 only:** Surreal stays claim SoT; git supplies temporal anchors (`introduced_in` / `invalidated_at` OIDs). Implement `as_of` / `knowledge_diff` per the spec.
- **B — Git as additional/alternative durable store:** durable team claims live as files in the repo; Surreal remains the operational index; per-claim ownership explicit.
- **C — Git as sole SoT for all knowledge:** analyzed, rejected as totalizing (embeddings, personal rows, ephemera, tickets do not belong in commits).

## 2. Decision (final)

**Hybrid A+B, sequenced:**

1. **Option A is binding as the foundation, now.** Implement Layer 3 per the spec (phases 3b → 3b′ → 3c → 3d → 3e; 3f optional) with Surreal as the operational claim index and git-bound anchors (HOLDAt, I(m)/X(m), AncestorSet, dirty/orphan policies).
2. **Option B is adopted** for the durable claim subset (`decision` / `rule` / `fix`): claims live as files in the repo (`.threski/claims/`), **entered via a bootstrap commit — never via history rewrite** — with Surreal as the derived one-way index. Design in the addendum; implementation phases 4a–4d follow Layer 3 phases 3b–3e.
3. **Option C remains rejected** as totalizing. A later git-owned *subset* is B, not C.

#31 (Layer 1–2) stays unblocked and merges independently.

## 3. Decision trail (audit)

- **Initial decision (2026-10-24, operator delegation):** A only; B deferred with re-entry conditions. Rationale: B presupposes A's machinery; time-to-value; zero sync tax; reversibility (§4 below).
- **Operator correction (same day):** repo-as-SoT is infeasible on an already-consolidated repo *without* history rewrite — retro-writing claims into past commits is impossible. Therefore **A+B is necessary**, not optional: B is only feasible as *bootstrap forward + optional notes for the past*, never as retro-fill. This correction removes the main deferral reason (B's "future maybe") — B becomes the target state, A its required foundation.
- **Policy addition (operator):** personal-claims inclusion in the repo SoT must be **parameterized**, not hard-denied (§5).

## 4. Rationale

1. **Topological sequencing.** B presupposes A's machinery (HOLDAt, I/X binding, AncestorSet, dirty/orphan policies). A first wastes nothing; B layers on it. A alone is complete; A+B is the target.
2. **Feasibility (operator).** History is immutable: no retro-fill. Bootstrap commit + `git notes` (pointers only) give B without rewriting a consolidated repo. Pre-bootstrap truth stays honestly Surreal-anchored.
3. **Time-to-value with bounded risk.** A is spec-frozen (G1–G4 answered, 11 acceptance tests, additive schema). B ships in phases behind the one-way ingest contract; its core risk (dual-truth drift) is contained by the hard rule **git wins** for git-owned ids.
4. **DNA fit.** Sharing is server-first; claim *anchors* are git-bound; the differentiator is git-bound commit-meaning memory. B upgrades this to claims *traveling with the repo* — PR-reviewable, blameable, cloneable — without relocating the operational index.
5. **No sync tax.** One-way only: consolidation → file → commit → ingest → index. The reverse is forbidden by design, not convention.

## 5. Claim ownership and personal-claims policy

**Kind map (fixed):** `decision`, `rule`, `fix` → git-eligible; `observation`, `working_state` → DB-only, always.

**Ownership:** every claim row carries `store: git | surreal` (row state); `store=deny` is a write-time directive (per-claim escape hatch) that beats any policy.

**Personal claims knob** — `memory.git_store.personal_claims`:

| Level | In tree | Body location | as_of behavior |
|-------|---------|---------------|----------------|
| `db_only` (default: team/server) | nothing | DB | HOLDAt only |
| `stub` (default: personal mode) | `id`, `kind`, `title`, anchors — **no body** | DB (versions via `memory_history`) | existence from tree; body reconstructed from DB history; clone-only → title only |
| `in_repo` | full claim | replicated | self-sufficient at every ref |

- **Scope = owner actor:** on team servers the policy covers the owner's personal rows only; other actors' personal rows are always `db_only`.
- **Leak warning:** switching to `in_repo` with a configured remote warns that repo access becomes the visibility boundary (MCP actor visibility does not extend to clones).
- Defaults are wizard-visible and mutable (`config fast/full`, Settings GUI, `Threski: Open Settings`).

## 6. Consequences

- **Immediate:** Layer 3 phases start after #31 merges (parallel allowed); addendum phases 4a–4d follow; commits and PRs stay separate per phase.
- **Schema:** adopt spec §5.1 as-is for Surreal-owned anchors; git-owned claims derive I/X from **file history** (file in tree(R) ⟺ claim holds at R), denormalized into the index at ingest. No `store:` ambiguity: git wins on id conflict.
- **Markdown projection:** unchanged — optional, regenerable, one-way (Oct #28 rule).
- **Two access models, documented:** Surreal visibility = MCP actor model; tree visibility = repo access. `stub`/`in_repo` make that trade-off deliberately.
- **Supersession mechanics:** git-owned claim superseded → new claim file + old file **deleted in the same commit** (history preserves the old version; tree(R) presence is the truth predicate). Withdrawal = file deletion with `chore(claims): withdraw <id>` convention.

## 7. Rollback

If B proves costly: flip git-eligible kinds back to Surreal-owned (existing files stay in tree history; claims keep DB rows). Kill-switch at codec/consolidation level; no data migration required in either direction.

## 8. Standard ambition

The operator's goal is for the git-tree claim store to become an **accepted cross-tool standard** (EditorConfig/MCP pattern). This ADR therefore requires the file language to stay minimal, tool-agnostic and versioned (`schema: 1` core — see addendum §2/§11); Threski is its reference implementation, not its owner.

## 9. What this ADR does not decide

- Scheduling beyond "after #31" (plan-level concern).
- `git notes` backfill scope (phase 5, gated; pointers only, never claim storage).
- Clone-only degraded mode shipping date (phase 5b).