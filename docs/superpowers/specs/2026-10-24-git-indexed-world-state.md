# Git-indexed world state (as_of / knowledge_diff)

**Date:** 2026-10-24  
**Status:** Accepted — [ADR](2026-10-24-adr-claim-sot-annotative-layer3.md): annotative Layer 3 (A) as **foundation**; **git-tree claim store (B) adopted** per [addendum](2026-10-24-git-claim-store-addendum.md), layered on this spec. **Phases 3a–3e + claim store 4a–4d implemented** (merged to `master` via #31, `fecf21b`, 2026-10-25); **phases 5a–5b (git notes, clone-only degraded reads) implemented** via #32; **phase 3f (hot-ref code byte cache, §5.3/§6.1 strategy B) implemented** with kill-switch `world_state.code_cache_disabled` — the optional layers are complete, strategy A stays the source of truth. Resume state: [superpowers/handoffs/2026-10-24-layer3-implementation-state.md](../handoffs/2026-10-24-layer3-implementation-state.md) (thread closed); **architecture review absorbed** (gaps G1–G4 → normative decisions in §4.5–§6, §5.5–§5.6, §14). Git-as-store fork **decided** (ADR + addendum); parking list consumed via #34.  
**Supersedes / extends:** completes the May 2026 intent that #31 only partially delivered  
**Does not replace:** [memory absorption](2026-10-09-memory-absorption-design.md) (Oct #28) or [commit-meaning temporal layer](../../commit_meaning_temporal.md) (#31)  
**Resume later:** [handoff — decision parking](../handoffs/2026-10-24-git-knowledge-sot-decision-parking.md) (Layer 3 vs optional git knowledge backend; no SoT decision locked)

**Related (surveyed):**

| Doc | Role vs this spec |
|-----|-------------------|
| [changelog_20260507.md](../../changelog_20260507.md) | May: `commit_chunk` historicization; demote experimental reinforce |
| [commit_meaning_temporal.md](../../commit_meaning_temporal.md) | #31: retrieve Δ + HEAD currentness + path stale |
| [temporal_freshness_plan_antigravity_review.md](../../temporal_freshness_plan_antigravity_review.md) | Historical traps for #31 (pre-ship) |
| [2026-10-09-memory-absorption-design.md](2026-10-09-memory-absorption-design.md) | Oct: typed memory, wall-clock `valid_*`, supersession, visibility |
| [memory_layer.md](../../memory_layer.md) | Operator guide for #28 (“true **now**”) |
| [spec_cognitive_architecture.md](../../spec_cognitive_architecture.md) | Historical Memify/reinforce sketches |
| [git_generalization_proposal.md](../../git_generalization_proposal.md) | Git access/credentials — orthogonal |
| Schema: `commit`, `commit_chunk`, `changed`, `parent_of`, `source_file`, `file_chunk`, `memory` | Existing graph atoms |

---

## 0. One-sentence thesis

Git already gives Ibis a **changelog of code transitions** and a way to obtain **code state at any ref**.  
Ibis today stores and retrieves the changelog semantically (`commit_chunk`) and the **HEAD** code/memory slice.  
This spec defines the missing formal object: a **world slice at ref R** — code + typed claims valid at R — plus operators `as_of` and `knowledge_diff`, so multiple contemporaneous truths can coexist along the git DAG without reinforcement as the primary timeline.

---

## 1. Problem statement (analytical)

### 1.1 Two duals (must not be conflated)

Let a repository’s history be a DAG of commits \(C\).

| Dual | Informal name | Git analogue | Ibis today |
|------|---------------|--------------|------------|
| **Δ (transition)** | Semantic changelog | `git show` / `git log -p` | `commit_chunk` (+ `changed` edges) |
| **Σ (state)** | World at a point | tree object / `git show R:path` | Mostly **HEAD only** (`file_chunk`/`source_file` with `is_head`; memory `status=current`) |

A changelog **implies** state if and only if there exists a well-defined **materialization function**

\[
\mathsf{Materialize}(R) = \Sigma_R
\]

Computable either by reading a snapshot keyed by \(R\), or by folding Δ from a checkpoint to \(R\).  
Without \(\mathsf{Materialize}\), searching Δ answers *what changed*, not *what was true*.

### 1.2 What #31 actually closed

- Ask retrieves Δ (`commit_chunk` scored by `commit_date`).
- Default truth paths filter memory by **logical currency** (`status`, wall-clock `valid_until`) and **privacy** (`visibility`).
- After HEAD ingest, path-based `RefreshStaleMemories` can demote claims whose **file content hash** no longer matches HEAD.

That is **HEAD hygiene + searchable changelog**, not \(\Sigma_R\) for arbitrary \(R\).

### 1.3 What Oct #28 actually closed (and what it is *not*)

October memory absorption introduced **concurrency of claims in the Memify sense**:

- kinds, `status ∈ {current, superseded, stale, archived}`;
- wall-clock `valid_from` / `valid_until` (datetime);
- `supersedes` lineage; `derived_from` → file/commit/issue;
- `provenance.source_hash` as **content hash** for dedup / HEAD-stale, **not** a git commit id.

It aligned **“which claim wins *now* on this SurrealDB instance”** (and who may see personal rows).  
It did **not** define **“which claim is true at commit `abc` on branch `feature`”**.

So the aside is correct in spirit but needs a precise label:

> Losing the project thread meant treating Oct’s *logical currentness / supersession* as if it were May’s *git-indexed multi-truth*. They share vocabulary (“temporal”, “stale”, “provenance”) but live on **different time axes** (wall-clock + HEAD hash vs git DAG).

### 1.4 May intent (restated without romanticism)

Replace usage reinforcement as the **primary** notion of “what matters temporally” with:

1. historicize each commit’s **meaning** (Δ) — largely done (produce + retrieve);
2. treat the git DAG as the index of **possible presents** — **not done**;
3. compare knowledge between two presents — **not done**.

Reinforcement (`save_reasoning_outcome` / RAFT weights) remains a **utility signal**, not a truth index.

---

## 2. Terminology (normative)

| Term | Definition |
|------|------------|
| **Ref** \(R\) | A resolvable git object name: commit OID (full or unambiguous prefix), or branch/tag that peels to a commit via existing `branch`→`pointed_to`→`commit` (or live `git rev-parse`). |
| **Repo** \(P\) | Project/repo record already used by ingest (`repo` / `projects[]`). |
| **Code state** \(\Sigma^{\mathrm{code}}_R\) | Map \(\mathrm{path} \mapsto \mathrm{blob\_oid}\) (and optionally cached text/chunks) for tree of \(R\). |
| **Claim** \(m\) | A typed `memory` row (or future claim node) that asserts something about \(P\). |
| **Git validity interval** | For claim \(m\), a half-open interval on the DAG: introduced at commit \(I(m)\), invalidated at commit \(X(m)\) or open-ended. Membership is **reachability-aware** (see §4). |
| **World slice** \(\Sigma_R\) | \(\Sigma^{\mathrm{code}}_R\) ∪ \(\{ m \mid m \text{ holds at } R \}\) under visibility rules. |
| **HOLDAt** \((m, R)\) | Predicate: claim \(m\) is part of \(\Sigma_R\) (§4). |
| **as_of** | Operator returning \(\Sigma_R\) (or a projection: code paths, claims, or both). |
| **knowledge_diff** | Operator \(\mathrm{Diff}(A,B) = \) structured delta between \(\Sigma_A\) and \(\Sigma_B\). |
| **Changelog hit** | A `commit_chunk` (or commit) relevant to a query; never asserted as \(\Sigma\) unless materialization says so. |

**Time axes (do not mix in predicates):**

| Axis | Fields / signals | Used for |
|------|------------------|----------|
| **Git DAG** | `introduced_in`, `invalidated_at`, commit OID, `parent_of` | HOLDAt, as_of, knowledge_diff |
| **Wall-clock** | `valid_from`, `valid_until` (datetime) | Session “now”, human expiry, Oct readers |
| **HEAD content** | `provenance.source_hash` vs `source_file.hash` where `is_head` | Auto-`stale` when file bytes move at HEAD |
| **Logical lifecycle** | `status` | Agent-facing enum; may be *derived* from axes above |

---

## 3. Layer map (what exists vs proposed)

```text
Layer 0  Graph atoms          commit, parent_of, changed, source_file, file_chunk
Layer 1  Δ semantic log       commit_chunk + ask/why_changed          [May + #31]
Layer 2  HEAD truth hygiene   memory status/visibility + RefreshStale [Oct + #31]
Layer 3  Σ at ref (THIS SPEC) as_of(R), knowledge_diff(A,B), HOLDAt
```

Layer 3 **consumes** 0–2; it must not delete Oct supersession or May chunks.

---

## 4. Formal model

### 4.1 Code state

\[
\Sigma^{\mathrm{code}}_R(\mathrm{path}) =
\begin{cases}
\mathrm{blob}(R,\mathrm{path}) & \text{if path exists in tree}(R) \\
\mathrm{undefined} & \text{otherwise}
\end{cases}
\]

**Normative implementation order (prefer cheapest correct):**

1. **Authoritative:** `git ls-tree -r R` / `git cat-file` / `git show R:path` against the repo’s working clone (personal/plugin working tree or server `discovery_root/dynamic` clone).  
2. **Cache (optional):** Surreal rows keyed by `(repo, commit_oid, path)` or content-addressed `file_chunk` linked to that commit — never required for correctness.  
3. **Forbidden as sole Σ:** inferring file text at \(R\) only from unordered `commit_chunk` patches without a fold algorithm and checkpoint policy.

Folding patches from root every query is allowed only with an explicit checkpoint strategy (see §6); default is git object read.

### 4.2 Claim HOLDAt (git axis)

Assume each claim \(m\) eventually carries:

- \(I(m)\): introducing commit OID (required for git-bound claims);
- \(X(m)\): optional invalidating commit OID (exclusive end);
- optional `derived_from` → `source_file` / `commit` for audit.

**Linear history (single branch peel):**  
\(\mathrm{HOLDAt}(m,R)\) iff \(I(m) \le R < X(m)\) in commit date/topo order on that line (exact order = `git merge-base` / rev-list ancestry: \(I\) is ancestor of \(R\), and \(X\) is not an ancestor of \(R\) or is undefined).

**DAG-precise (normative for merge commits):**

\[
\mathrm{HOLDAt}(m,R) \iff
\mathrm{Ancestor}(I(m), R)
\;\land\;
\bigl( X(m)=\bot \;\lor\; \neg\mathrm{Ancestor}(X(m), R) \bigr)
\]

where \(\mathrm{Ancestor}(A,B)\) includes \(A=B\).

**Unanchored claims:** if \(I(m)\) is missing, \(\mathrm{HOLDAt}(m,R)\) is **false for all \(R \neq \mathrm{HEAD}\)** under strict mode; at HEAD, Oct rules apply (`status=current` ∧ wall-clock ∧ visibility). Spec default: **strict for `as_of`**, **HEAD-compatible for omit-ref ask** (today’s behavior).

### 4.3 Combining axes at HEAD

For \(R = \mathrm{HEAD}(P)\):

\[
m \in \Sigma_{\mathrm{HEAD}}
\iff
\mathrm{Visible}(m,\mathrm{actor})
\;\land\;
\mathrm{OctCurrent}(m)
\;\land\;
\bigl( \neg\mathrm{GitBound}(m) \;\lor\; \mathrm{HOLDAt}(m,\mathrm{HEAD}) \bigr)
\]

where \(\mathrm{OctCurrent}\) = `status ∈ {current}` ∧ (`valid_until` open or future) as today.  
`stale` from `RefreshStaleMemories` remains a **HEAD content-hash** signal; implementations may set \(X(m):=\mathrm{HEAD}\) when marking stale (recommended) so git axis and status stay aligned.

### 4.4 as_of

\[
\mathsf{as\_of}(P, R, \pi) =
\pi\bigl(\Sigma^{\mathrm{code}}_R \cup \{ m \mid \mathrm{HOLDAt}(m,R) \land \mathrm{Visible}(m,\mathrm{actor}) \}\bigr)
\]

Projection \(\pi \in \{\mathrm{code}, \mathrm{claims}, \mathrm{both}, \mathrm{paths}(S)\}\).

### 4.5 knowledge_diff (symmetric difference only)

Given write rules (§5.2) that always close the old interval on supersede (`\(X:=\mathrm{HEAD}\)`), a claim cannot HOLD at both \(A\) and \(B\) with different content under the same record id. Therefore:

\[
\begin{align*}
\mathrm{Diff}(A,B)_{\mathrm{claims}}^{+} &= \{ m \mid \mathrm{HOLDAt}(m,B) \land \neg\mathrm{HOLDAt}(m,A) \} \\
\mathrm{Diff}(A,B)_{\mathrm{claims}}^{-} &= \{ m \mid \mathrm{HOLDAt}(m,A) \land \neg\mathrm{HOLDAt}(m,B) \} \\
\mathrm{Diff}(A,B)_{\mathrm{code}} &= \mathrm{git\_diff}(A,B) \text{ (paths; patch optional / capped)}
\end{align*}
\]

No separate `content_changed` channel. Supersession already appears as \(-m_{\mathrm{old}}+m_{\mathrm{new}}\) in the ± sets.

**Churn control (long-lived branches):** callers SHOULD pass at least one of: `path_prefix`, `kinds[]`, `max_claims` (default soft cap e.g. 200 with `truncated=true`), or require `merge_base`-relative mode `diff_mode=since_divergence` (only claims whose \(I\) or \(X\) lies on the symmetric difference of rev-lists). Default without filters: return capped ± plus `truncated` warning — never unbounded dump.

### 4.6 Ask under `at_ref` — honest retrieval contract (closes G4)

**Non-promise:** Ibis does **not** maintain per-commit embedding corpora. Full semantic search over arbitrary historical trees is a non-goal (§8).

When `ask_project` receives `at_ref=R` (resolved OID):

| Channel | Behavior at \(R\) |
|---------|-------------------|
| **Claims** | Vector/keyword search over git-bound candidates, then **filter** \(\mathrm{HOLDAt}(m,R)\) (via AncestorSet, §6). HOLDAt wins over Oct `status=current` for historical refs. |
| **Code (semantic)** | Run hybrid search on **existing HEAD `file_chunk` embeddings**, then **keep only hits whose path exists in `tree(R)`** (and optionally re-rank). For surviving paths, **replace hit text** with `git show R:path` (or chunk slice of that blob) so the agent sees bytes at \(R\), not HEAD drift. Label `as_of_ref=R`, `content_source=git_show`. |
| **Code (exact)** | If the query names a path / symbol locator, skip semantic: `git show R:path` (+ optional on-demand embed of that blob for one-shot similarity — not persisted). |
| **Δ `commit_chunk`** | Unchanged searchable changelog context; `source_kind=commit_chunk`, `is_current=false`. Prefer commits reachable from \(R\) (AncestorSet) when filtering noise. |

When `at_ref` omitted: preserve #31/#28 HEAD contract (semantic = true HEAD chunks).

MCP descriptions MUST state this hybrid honestly — never “semantic index of every historical tree”.

---

## 5. Schema deltas (proposed)

Additive; Surreal stays schemaless-friendly with DEFINE FIELD.

### 5.1 Memory / claim git binding

```text
provenance.introduced_in_commit : option<record<commit>> | option<string oid>
provenance.invalidated_at_commit : option<record<commit>> | option<string oid>
provenance.source_path          : already in #31
provenance.source_hash          : HEAD content hash (unchanged meaning)
```

Optional denormalized scalars for index: `introduced_in_oid`, `invalidated_at_oid` (string, 40-char hex).

Indexes: `(project, introduced_in_oid)`, `(project, invalidated_at_oid)`.

### 5.2 Write path rules

| Event | Effect on git axis |
|-------|--------------------|
| memory_add with source_refs including commit:… or explicit introduced_in | Set \(I(m)\); reject if OID unknown in object DB |
| memory_add with only source_file | **§5.5 dirty-tree policy** (not blind I:=HEAD) |
| RefreshStaleMemories marks stale | Set status=stale **and** \(X(m):=\mathrm{HEAD}\) at mark time |
| memory_update supersede | New row \(I:=\mathrm{HEAD}\) only if clean tip (§5.5); old row \(X:=\mathrm{HEAD}\), `status=superseded` |
| Import without commit | Leave unanchored; excluded from historical as_of (strict) |
| Sync / fetch detects unreachable OID | **§5.6 orphan policy** |

### 5.3 Code cache (optional phase)

Table or fields: `file_at_commit(repo, oid, path, hash, chunk_ids…)` — **cache only**. Correctness must not depend on full historical backfill.

### 5.4 What we explicitly do *not* add

- Blockchain / attestation chains.
- Second database.
- Replacing `valid_from`/`valid_until` datetime (keep for wall-clock; git axis is parallel).
- Making Dual-Loop the index of truth.
- Per-commit full-tree embedding backfill.

### 5.5 Dirty working tree (closes G2) — refuse silent HEAD lies

Personal/plugin ingest the **live working tree**. Bytes may not equal HEAD:path.

**When binding from source_file / path:**

1. work_hash = hash(working tree bytes); head_hash = hash(git show HEAD:path) (absent ⇒ undefined).
2. If clean (work_hash == head_hash): \(I(m)=\mathrm{HEAD}\), source_hash=head_hash, provenance.dirty=false.
3. If dirty/untracked: **never** pretend \(I=\mathrm{HEAD}\). Use `anchor_mode` (default **worktree** personal/plugin; **require_clean** server):

| `anchor_mode` | Behavior |
|---------------|----------|
| `require_clean` | Reject git-binding with dirty_path; allow unanchored Oct write or commit first |
| worktree | provenance.dirty=true, worktree_hash=work_hash; \(I\) unset → HOLDAt false for all OIDs; may still appear in HEAD Oct ask if current |
| promote_on_commit | As worktree, then on commit containing path+hash set \(I\) and clear dirty |

**Audacious:** first-class slice \(\Sigma_{\mathrm{WORKTREE}}\) via `as_of(ref=WORKTREE)` — never aliased to `HEAD`. Hits MUST carry `as_of_ref=WORKTREE`.

### 5.6 Rewritten history / orphan OIDs (closes G3)

**Assumptions:** server as_of clone is **full** (or deepen on demand); shallow → fail with shallow_history, do not silently empty Σ. Personal/plugin: document degradation on shallow clones.

**RefreshOrphanedGitBounds** (after fetch/sync, with or inside RefreshStaleMemories):

1. Batch-check all distinct introduced_in_oid / invalidated_at_oid (git cat-file --batch-check, one process).
2. If \(I\) missing: status=stale, provenance.orphan_oid=I; HOLDAt false everywhere; do not delete.
3. Optional rebind (default **off**): if path+hash still reachable, git log -1 --find-object=<blob> on default branch → new \(I\); set provenance.rebound_from.

---

## 6. Materialization & ancestry cost (closes G1)

### 6.1 Code Σ

| Strategy | Use when | Cost |
|----------|----------|------|
| **A. Git object read** | Default \(\Sigma^{\mathrm{code}}_R\) | O(tree)/path; correct |
| **B. Cached chunks at OID** | Hot refs only | Storage |
| **C. Checkpoint + Δ fold** | Explicit offline only | Never default |

### 6.2 AncestorSet — normative HOLDAt engine (bold)

**Forbidden as default:** one git merge-base --is-ancestor (or equivalent) **per claim**.

**Required for as_of / ask at_ref / knowledge_diff (claims):**

1. Resolve \(R\) → full OID; fail closed if unknown.
2. **Prefilter** in Surreal: introduced_in_oid != NONE; visibility/kind/project; optional commit-date \(\le R\) when denormalized; hard LIMIT (e.g. 2k) + overflow flag.
3. **Build AncestorSet(R) once:** git rev-list R (commit-graph aware) → in-memory set/bitmap; Surreal parent_of hydration only if coverage proven.
4. \(\mathrm{HOLDAt}(m,R) \iff I\in\mathrm{AncestorSet}(R) \land (X=\bot \lor X\notin\mathrm{AncestorSet}(R))\).
5. Process-local TTL cache keyed by (repo_path, R); invalidate on fetch.

**Cost target:** \(O(|\mathrm{rev\!-\!list}\,R|) + O(|\mathrm{candidates}|)\) with **one** rev-list-family invocation per query — not O(|claims|) git spawns.

No new query DSL in v1.

---

## 7. Agent / MCP contract (proposed)

| Tool | Behavior |
|------|----------|
| `ask_project(query, at_ref?)` | **Primary** entry. Omit ref → HEAD (#31). With ref → §4.6 honest hybrid |
| as_of_project(project, ref, projection?) | **Thin wrapper** over the same HOLDAt + Materialize core (list/summary shape). Not a second semantic engine |
| knowledge_diff(project, from_ref, to_ref, filters?) | §4.5 ± only; churn caps / since_divergence |
| why_changed | Δ-oriented; optional path+ref range |
| memory_add | §5.2 + §5.5; unbound/dirty claims skip OID as_of |

Observability: every hit under `at_ref` / `WORKTREE` includes `as_of_ref` and `source_kind`.

---

## 8. Non-goals

- Full per-commit embedding of entire trees in Surreal by default.  
- Time-travel UI in Settings v1.  
- Replacing git with Surreal as object store.  
- Automatic rewrite of all historical memories to invent \(I(m)\).  
- Treating Oct `valid_until` as a git ref.  
- Implying ask `at_ref` has a historical semantic corpus equal to HEAD (§4.6).

---

## 9. Phased delivery

| Phase | Deliverable | Exit criteria |
|-------|-------------|---------------|
| **3a Spec freeze** | This document + G1–G4 decisions | Cross-doc consistency; review gaps closed in text |
| **3b Bind writes** | \(I\)/\(X\) + dirty policy §5.5 + orphan pass §5.6 | Tests: clean bind, dirty refuse/worktree, orphan→stale |
| **3b′ AncestorSet** | Shared `AncestorSet(R)` helper (**before** 3c) | Bench: 10k fake OIDs + 500 claims, one rev-list; no per-claim spawn |
| **3c as_of claims** | HOLDAt + ask `at_ref` claims / thin `as_of_project` | Round-trip A→B→C; unanchored excluded |
| **3d as_of code** | git show/ls-tree + §4.6 path filter of HEAD embeddings | Bytes match `git show`; MCP text states hybrid limits |
| **3e knowledge_diff** | ± HOLDAt + caps / since_divergence | Diff tests + truncation flag |
| **3f Cache (optional)** | Hot-ref code byte cache (`file_at_commit`, OID+path keyed) | Kill-switch `world_state.code_cache_disabled` leaves A correct — **implemented** |

**Merge #31 first** (or in parallel without blocking): Layer 3 builds on Layer 1–2 HEAD behavior.

---

## 10. Acceptance tests (normative sketches)

1. **Ancestry HOLDAt:** `A→B→C`; \(I=A\), \(X=C\); HOLDAt(B)=true; HOLDAt(C)=false.  
2. **Unanchored:** no \(I\); `as_of(B)` excludes; HEAD ask may still return if Oct-current.  
3. **Stale alignment:** file hash change → RefreshStale → stale ∧ \(X=\mathrm{HEAD}\).  
4. **knowledge_diff:** A→C lists claim in `removed` when \(X=C\); no phantom `content_changed`.  
5. **Code as_of:** path bytes equal `git show R:path`.  
6. **Privacy:** personal claim absent for non-owner.  
7. **Δ ≠ Σ:** commit_chunk hits never `is_current`.  
8. **G1 perf:** as_of does not spawn merge-base per claim (assert via test double / counter).  
9. **G2 dirty:** dirty path + `require_clean` → error; `worktree` → no OID HOLDAt; WORKTREE projection optional.  
10. **G3 orphan:** delete OID from test repo / rewrite → RefreshOrphaned → stale + orphan_oid.  
11. **G4 honesty:** ask `at_ref` with path only in old tree returns git_show content; embedding hit for path absent at \(R\) dropped.

---

## 11. Documentation debt this spec pays

| Confusion | Resolution |
|-----------|------------|
| “Temporal layer” meant multi-truth at every commit | No — that is Layer 3 (this doc). #31 is Layer 1+2. |
| Oct `valid_*` = git range | No — wall-clock; git range is `introduced_in`/`invalidated_at`. |
| `source_hash` = commit id | No — file content hash for HEAD stale. |
| `commit_chunk` = state at commit | No — Δ only; state needs Materialize. |
| Reinforcement replaced historicization | Intent yes for **timeline**; reinforce remains utility. |

---

## 12. Open questions (remaining — G1–G4 decided)

| # | Decision status |
|---|-----------------|
| G1 ancestry cost | **Decided:** AncestorSet + prefilter (§6.2); phase **3b′** before 3c |
| G2 dirty tree | **Decided:** refuse silent HEAD bind; `anchor_mode` + optional \(\Sigma_{\mathrm{WORKTREE}}\) (§5.5) |
| G3 orphan OID | **Decided:** RefreshOrphanedGitBounds + full-clone assumption (§5.6) |
| G4 retrieval honesty | **Decided:** HEAD-embed ∩ tree(R) + git_show text (§4.6) |

Still open before 3c implementation:

1. Branch-qualified refs: OID-only vs `(branch, oid)` for scratch — default **OID-only**; `working_state` stays Oct personal latest-wins.  
2. Merge commits: full AncestorSet vs first-parent-only for release lines — default **full DAG**; optional `ancestry_mode=first_parent` later.  
3. Backfill: from `derived_from → commit` when present; else unanchored.  
4. `status=current`: remain **independently writable**; RefreshStale/orphan/supersede **write** git bounds; as_of **reads** HOLDAt primarily.

---

## 13. Summary for operators

- **May / #31:** searchable *what changed* + clean *what is true at HEAD*.  
- **Oct / #28:** typed claims, supersession, privacy, wall-clock validity.  
- **This spec:** *what was true at ref R* and *diff of truths between A and B* — the formalization that makes the graph git-bound in the sense you meant in May, without pretending the Oct concurrency model already was that.

---

## 14. Architecture review absorption (2026-10-24)

External review verified Layer 0–3 cross-doc consistency (3a exit) and flagged G1–G4 as make-or-break. Responses:

| Gap | Bold solution locked in spec |
|-----|------------------------------|
| **G1** | One `AncestorSet(R)` per query; Surreal prefilter; ban per-claim `merge-base`; phase 3b′ gate |
| **G2** | Dirty ≠ HEAD; `require_clean` / `worktree` / `promote_on_commit`; optional WORKTREE slice |
| **G3** | Orphan→stale; full clone for server as_of; optional rebind off-by-default |
| **G4** | Honest hybrid: HEAD vectors ∩ paths@R + `git show` body — no fake historical corpus |

Minor: knowledge_diff = ± HOLDAt only; `as_of_project` = thin wrapper; churn caps on diff.
