# Git-tree claim store (B) — design addendum

**Date:** 2026-10-24
**Status:** Accepted per [ADR](2026-10-24-adr-claim-sot-annotative-layer3.md) §2 (final decision). **Phases 4a–4d implemented** (merged to `master` via #31, 2026-10-25: codec, one-way dual-write + ingest reconciliation, bootstrap + personal-claims policy knob, unified tree ∪ HOLDAt as_of/diff; review follow-ups: `since_divergence`, `promote_on_commit` hook, `shallow_history` marker). **Phases 5a–5b implemented** on branch `feat/claim-store-phase5` via #32 (git notes pointers + `memory rebuild-git-notes`; clone-only degraded reads on `as_of_project`/`ask_project at_ref`/`knowledge_diff` with `degraded` flags, stubs title-only) — state + parking list in the [implementation-state handoff](../handoffs/2026-10-24-layer3-implementation-state.md).
**Builds on:** [2026-10-24-git-indexed-world-state.md](2026-10-24-git-indexed-world-state.md) — Layer 3 (HOLDAt, `as_of`, `knowledge_diff`, AncestorSet, dirty/orphan policies) is a hard prerequisite
**Supersedes:** the Option B sketch in [2026-10-24-git-knowledge-sot-decision-parking.md](../handoffs/2026-10-24-git-knowledge-sot-decision-parking.md) §6

---

## 1. Scope and ownership

| Claim class | Store | Notes |
|-------------|-------|-------|
| `decision`, `rule`, `fix` with `visibility=team` | **git** (`.threski/claims/`) + derived Surreal index | durable, PR-reviewable |
| `observation`, `working_state` (any visibility) | **Surreal only** | operational/ephemeral by nature |
| `visibility=personal` (any kind) | per policy (§4) | owner actor only |
| Per-claim `store=deny` | Surreal only | beats policy and kind map |

Every Surreal claim row carries `store: git | surreal`. Git-owned rows are **derived**: the tree is their SoT. A hand edit of a claim file is a legitimate write (the repo is SoT; the codec is a convenience, not a gate).

## 2. File language — designed as an open standard

The file language is deliberately **tool-agnostic** so the store can become an accepted cross-tool convention (EditorConfig/MCP pattern): plain markdown + YAML frontmatter, truth predicate = plain git (file presence in tree), **no proprietary required fields**. Threski is the reference implementation; conformance is defined by §10 tests. The well-known directory is `.threski/claims/`; format evolution is governed by the `schema` field (integer, current `1`).

One file ↔ one claim: `.threski/claims/<id>.md`

```markdown
---
schema: 1                      # format version (integer)
id: dec-2026-10-24-001        # stable, immutable, never reused
kind: decision                 # decision | rule | fix
title: Short imperative title
visibility: team
related_paths:
  - internal/search/memory_add.go
supersedes: dec-2026-09-30-007 # optional; only on the superseding file
created: 2026-10-24T15:04:05Z
---
Body markdown. The claim.
```

**Minimal core any conforming tool must honor:** `schema`, `id`, `kind`, `title`, `created` + body; truth predicate = file presence in `tree(R)`; supersession via `supersedes` + old-file deletion. Everything else is optional extension space.

**Truth predicate (git-native):** a git-owned claim holds at ref R ⟺ `.threski/claims/<id>.md` exists in `tree(R)`.

- **I(m)** = first commit containing the file; **X(m)** = commit that removes it (⊥ if present at HEAD). Derived from file history at ingest and denormalized into the Surreal index — never hand-written OIDs.
- **Supersession** = new claim file added **and** old file deleted in the same commit; the new file carries `supersedes: <old-id>`. History preserves the old version for `as_of`.
- **Withdrawal** = file deletion, commit convention `chore(claims): withdraw <id>`.
- **Dirty tree:** §5.5 of the Layer 3 spec applies verbatim to claim files (`anchor_mode`: `require_clean` / `worktree` / `promote_on_commit`).
- Malformed file → ingest warns and skips; ingest never fails because of one bad claim.

## 3. Write path — strictly one-way

```text
memory_add / consolidate
   → Surreal row (store decided by kind map + policy §4)
   → [git-owned] codec renders .threski/claims/<id>.md
   → staged into pending commit set → commit (chore(claims): add <id>)
   → ingest reads tree → upserts Surreal index row (store: git, I/X from file history)
```

- Never: DB-side mutation of git-owned claim content (supersede/withdraw of a git-owned claim is performed **as a commit**, never as a direct `status` flip).
- Never: file generation outside the commit path (no working-tree droplets left uncommitted; §5.5 governs).
- Ingest marks rows with `policy_applied: <level>` (§4) for auditability.

## 4. Personal-claims policy (operator knob)

Setting `memory.git_store.personal_claims: db_only | stub | in_repo` — governs `visibility=personal` rows only.

| Level | In tree | Body | as_of |
|-------|---------|------|-------|
| `db_only` — **default on team/server** | nothing | DB | HOLDAt only |
| `stub` — **default in personal mode** | `id`, `kind`, `title`, anchors; `redacted: stub` frontmatter, empty body | DB (history via `memory_history`) | existence from tree; body from DB history; clone-only → title only |
| `in_repo` | full claim | replicated | self-sufficient at every ref |

Rules: scope is the **owner actor** (other actors' personal rows always `db_only` on a server); per-claim `store=deny` wins over any level; switching to `in_repo` while a remote is configured emits a leak warning (repo access becomes the visibility boundary; MCP actor visibility does not extend to clones).

## 5. Bootstrap (no history rewrite)

CLI (under the `memory` command group): `memory bootstrap-git-store [--dry-run]`.

- Scans current durable Surreal claims (git-eligible kinds, visibility per policy §4).
- Renders files into `.threski/claims/` and creates **one** commit: `chore(claims): bootstrap git claim store (N claims)`.
- **Pre-bootstrap honesty:** git-owned claims have I(m) = bootstrap commit; earlier wall-clock history remains Surreal-only (`derived_from`, `memory_history`). `as_of` at refs before the bootstrap returns no git-owned claims — documented behavior, not a bug.
- Idempotent: claims already present in the tree are skipped; second run produces zero new commits.

## 6. git notes (optional, phase 5, gated)

`refs/notes/threski-claims` anchors **historical** commits to claim ids (cross-links enriching commit meaning). Notes are pointers (`claim=<id>`), never claim storage, never required for correctness, rebuildable from the DB. They do not appear in PR diffs by default and must not be treated as reviewable knowledge.

## 7. Clone-only degraded mode (phase 5)

With no Surreal reachable: parse `.threski/claims/` from `tree(HEAD)` (or any `as_of` ref). Git-owned claims are fully readable; `stub` files show title only. MCP read tools work in this mode; hits are labeled `source_kind=claim_file`.

## 8. Conflicts

| Situation | Resolution |
|-----------|------------|
| Same id as file in tree **and** Surreal row with mismatched content | **git wins**; DB row reconciled at ingest |
| Hand edit of claim file | accepted (repo is SoT) |
| Malformed file | warn + skip |
| `store=deny` claim that somehow has a file | file wins too (git wins rule); flag for manual cleanup |

## 9. Phased delivery

| Phase | Deliverable | Exit criteria |
|-------|-------------|---------------|
| **4a** | Codec + file language round-trip | claim ↔ file byte-stable; frontmatter validation tests |
| **4b** | Dual-write in `memory_add`/consolidation + commit integration + ingest indexing | claim add → file → commit → index row `store: git` with I/X |
| **4c** | Bootstrap command + policy enforcement (wizard/Settings knob) | personal mode → stub default; team → db_only; idempotency test |
| **4d** | `as_of`/`knowledge_diff` unified over tree ∪ HOLDAt | Σ_R = tree claims ∪ HOLDAt claims; diff lists tree removals |
| **5a** | `git notes` backfill + rebuild | notes rebuildable from DB; correctness independent |
| **5b** | Clone-only degraded ask | fresh clone, no DB → git-owned claims + stub titles |

## 10. Acceptance tests (normative sketches)

1. **Round-trip:** `memory_add` decision → file exists at HEAD, frontmatter valid, body byte-identical.
2. **as_of via tree:** file added at A, superseded at B (old deleted) → holds at A, absent at B; `knowledge_diff(A,B)` lists removal with `supersedes` link.
3. **Policy stub (personal default):** personal claim file has empty body + `redacted: stub`; DB row intact; `as_of` body served via `memory_history`.
4. **Policy db_only (team default):** personal claim → no file, DB only.
5. **`store=deny`:** no file regardless of policy.
6. **Bootstrap idempotency:** second run → zero new commits.
7. **Hand edit:** external edit of a claim file → ingest indexes the change (repo is SoT).
8. **Malformed file:** ingest warns, skips, keeps running.
9. **One-way supersede:** superseding a git-owned claim produces one commit (add new + delete old), never a DB status flip.
10. **Clone-only:** fresh clone without DB → MCP read returns git-owned claims; stubs show titles only.

## 11. Standard ambition

The operator's stated goal is for this storage method to become an **accepted standard** for project knowledge on git. That ambition imposes the design constraints already baked in above:

- **Minimal core schema** (`schema: 1`): five required fields, YAML frontmatter + markdown body, no tool lock-in.
- **Git-native truth predicate** (file presence) — readable by any git client, no database required.
- **Well-known path** (`.threski/claims/`) + versioned format for evolution.
- **Threski as reference implementation**: codec, ingest, `as_of`, degraded clone-only mode; conformance defined by §10 tests.

Third-party implementations only need: parse frontmatter, list tree paths, respect `supersedes` + deletion. Everything Threski-specific (embeddings, actor visibility, policy enforcement) is index-side, invisible to the standard.

## 12. Non-goals

- History rewrite / retro-fill of any kind.
- Two-way synchronization (the reverse direction is forbidden by design).
- Notes as claim storage.
- Encrypting or obfuscating personal claims in tree — if secrecy matters, `db_only`.
- Time-travel UI.