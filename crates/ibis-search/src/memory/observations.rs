//! C8 — deterministic pattern-recognition observations (steps 1–4).
//!
//! Contract (§4.2–§4.7 of `2026-11-09-dailydoseofds-improvement-analysis-l3-fasi-4-5.md`):
//! - Extraction: 7 bounded SurrealQL projections; bound arrays ride the
//!   `$params` map and a query is skipped entirely when its bound array is
//!   empty (`IN []` must never mean "everything"); any query failure aborts
//!   the pass with `Err` (no partial extraction). Every projection carries an
//!   explicit `LIMIT` and a truncation flag: a truncated window never
//!   authorizes archive/supersede of rows it may simply not have seen.
//! - Signature model: normalization, `Signature` tuples with the closed
//!   root-cause stoplist, `signature_hash` (row identity) and `evidence_hash`
//!   (the synthesis/idempotency gate) over an explicit `canonical_json`.
//! - Components: union-find over the bipartite memory↔signature graph, hub
//!   exclusion before union (degree > `hub_degree_max`), min/max member
//!   filters — oversized components are DROPPED, never truncated; components
//!   beyond `max_clusters_per_repo` are returned as overflow and their
//!   signature hashes are protected from archive/supersede.
//! - Persistence: deterministic `memory:⟨obs_<signature_hash>⟩` ids,
//!   `store='derived'` (read-only), the full C7 `details` rubric, member +
//!   evidence `derived_from` edges, `has_memory`, and split/merge
//!   `supersedes` edges (never `invalidated_at_oid`).
//! - Modes: `InvalidateOnly` (hook contract: zero LLM, zero embedding, zero
//!   CREATE; marks stale what changed and archives what vanished) and `Full`
//!   (synthesis + create). Steps 1–4 persist WITHOUT an LLM: the synthesis is
//!   the deterministic fallback summary (`generated_by=fallback`); the
//!   embedder (when wired) reuses the existing `ai.embed_text` plumbing.
//!
//! Determinism: every ordering is total and input-order independent — members
//! by id, signatures by `(family_priority, key)`, components by
//! `(member_count desc, anchor asc)`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sha2::{Digest, Sha256};

use ibis_db::executor::{Executor, Vars};
use ibis_db::value::{Object, RecordId, RecordIdKey, Value};

use super::{
    bind, details, repo_record_id, Bindings, ES_DERIVED_FROM, ES_HAS_MEMORY, ES_SUPERSEDES,
    TABLE_MEMORY,
};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Family: `fix` rows sharing the same normalized `details.root_cause`.
pub const FAMILY_FIX_ROOT_CAUSE: &str = "fix_root_cause";
/// Family: memories anchored to commits implementing the same issue.
pub const FAMILY_ISSUE: &str = "issue";
/// Family: memories anchored to a file whose symbol name is a token of the
/// memory text.
pub const FAMILY_SYMBOL: &str = "symbol";
/// Family: memories anchored to the same source file.
pub const FAMILY_FILE: &str = "file";

/// Version of the extraction+synthesis rubric (`policy_applied`/`rubric`).
pub const OBSERVATION_RUBRIC: &str = "observation.v1";
/// `policy_applied` marker of the observation producer.
pub const OBSERVATION_POLICY: &str = "observation.v1";
/// `provenance.agent` of the observation producer (audit).
pub const OBSERVATION_AGENT: &str = "ibis.observation_engine";
/// `provenance.tool` of the observation producer (audit).
pub const OBSERVATION_TOOL: &str = "memory_observations";

/// Deterministic `root_cause` stoplist (matched after `normalize_key`):
/// prevents the trivial hubs (`unknown`, `n/a`, …) from swallowing the repo.
pub const ROOT_CAUSE_STOPLIST: [&str; 8] =
    ["", "none", "n/a", "na", "unknown", "tbd", "misc", "various"];

/// The `$kinds` bound set for Q1 (§4.2).
pub const OBSERVATION_KINDS: [&str; 8] = [
    "decision",
    "fix",
    "constraint",
    "rule",
    "gotcha",
    "design",
    "fact",
    "reasoning",
];

/// Hard cap on the Q1 window (`max_members_examined`).
pub const OBSERVATION_MAX_EXAMINED: usize = 500;

/// Hard cap on every non-Q1 graph query (Q2/Q3/Q6/Q7): rows beyond the cap
/// are dropped and flag the extraction as truncated, which suppresses
/// archive/supersede for the whole pass.
pub const MAX_GRAPH_ROWS: usize = 5000;

/// Typed bounds of one reconcile pass (§4.11 defaults; the `ai.observations`
/// config wiring is step 6 and out of this scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationLimits {
    pub min_cluster_size: usize,
    pub max_members: usize,
    pub hub_degree_max: usize,
    pub max_clusters_per_repo: usize,
    pub max_members_examined: usize,
}

impl Default for ObservationLimits {
    fn default() -> Self {
        ObservationLimits {
            min_cluster_size: 2,
            max_members: 12,
            hub_degree_max: 10,
            max_clusters_per_repo: 25,
            max_members_examined: OBSERVATION_MAX_EXAMINED,
        }
    }
}

/// Every v1 family in anchor order (the `ai.observations.families` whitelist
/// default).
pub const OBSERVATION_FAMILIES: [&str; 4] = [
    FAMILY_FIX_ROOT_CAUSE,
    FAMILY_ISSUE,
    FAMILY_SYMBOL,
    FAMILY_FILE,
];

/// Resolved `ai.observations` knobs (C8 step 6). `enabled = false` by
/// default: the ingest hook and the synthesis pass stay inert until an
/// operator opts in, exactly like `ai.consolidation`.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationSettings {
    /// Master switch for the ingest hook (`InvalidateOnly`) and the
    /// consolidation-pass synthesis (`Full`).
    pub enabled: bool,
    /// Structural extractor tunables (L3 §4.4).
    pub limits: ObservationLimits,
    /// Families allowed to produce signatures (whitelist; unknown names are
    /// dropped with a warning at config resolution).
    pub families: Vec<String>,
    /// Scope cap per project pass: `team` plus up to `max_scopes - 1`
    /// personal owners; the rest wait for the next pass.
    pub max_scopes_per_project: usize,
    /// Maximum LLM summaries per pass (the token-spending cap).
    pub max_llm_summaries_per_pass: usize,
    /// Optional D11 shadow measurement of the deterministic cluster filters
    /// (`observation_accept.v1`); default off, log-only.
    pub decide_shadow: bool,
}

impl Default for ObservationSettings {
    fn default() -> Self {
        ObservationSettings {
            enabled: false,
            limits: ObservationLimits::default(),
            families: OBSERVATION_FAMILIES
                .iter()
                .map(|family| family.to_string())
                .collect(),
            max_scopes_per_project: 8,
            max_llm_summaries_per_pass: 10,
            decide_shadow: false,
        }
    }
}

impl ObservationSettings {
    /// Resolves the raw `ai.observations` block (C8 step 6): clamp every
    /// numeric knob, whitelist the families. Never fails the boot; every
    /// clamp/unknown value logs a warning.
    pub fn from_config(cfg: &ibis_core::config::ObservationsConfig) -> Self {
        let mut families: Vec<String> = Vec::new();
        for raw in &cfg.families {
            let family = raw.trim().to_lowercase();
            if OBSERVATION_FAMILIES.contains(&family.as_str()) {
                if !families.contains(&family) {
                    families.push(family);
                }
            } else {
                tracing::warn!("unknown ai.observations.families value {raw:?}; dropped")
            }
        }
        if families.is_empty() {
            tracing::warn!(
                "ai.observations.families resolved empty; using all families (an empty \
                 whitelist would archive every derived observation)"
            );
            families = OBSERVATION_FAMILIES
                .iter()
                .map(|family| family.to_string())
                .collect();
        }
        let clamp = |value: u32, min: u32, max: u32, key: &str| -> u32 {
            let clamped = value.clamp(min, max);
            if clamped != value {
                tracing::warn!("{key} {value} clamped to {clamped}");
            }
            clamped
        };
        let limits = ObservationLimits {
            min_cluster_size: clamp(
                cfg.min_cluster_size,
                2,
                50,
                "ai.observations.min_cluster_size",
            ) as usize,
            max_members: clamp(cfg.max_members, 2, 100, "ai.observations.max_members") as usize,
            hub_degree_max: clamp(cfg.hub_degree_max, 1, 500, "ai.observations.hub_degree_max")
                as usize,
            max_clusters_per_repo: clamp(
                cfg.max_clusters_per_repo,
                1,
                500,
                "ai.observations.max_clusters_per_repo",
            ) as usize,
            max_members_examined: clamp(
                cfg.max_members_examined,
                1,
                OBSERVATION_MAX_EXAMINED as u32,
                "ai.observations.max_members_examined",
            ) as usize,
        };
        ObservationSettings {
            enabled: cfg.enabled,
            limits,
            families,
            max_scopes_per_project: clamp(
                cfg.max_scopes_per_project,
                1,
                64,
                "ai.observations.max_scopes_per_project",
            ) as usize,
            max_llm_summaries_per_pass: clamp(
                cfg.max_llm_summaries_per_pass,
                1,
                100,
                "ai.observations.max_llm_summaries_per_pass",
            ) as usize,
            decide_shadow: cfg.decide_shadow,
        }
    }

    /// True when `family` is in the resolved whitelist.
    pub fn family_allowed(&self, family: &str) -> bool {
        self.families.iter().any(|allowed| allowed == family)
    }
}

/// Reconcile mode: `InvalidateOnly` is the post-ingest hook contract (zero
/// LLM, zero embedding, zero CREATE), `Full` is the consolidation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReconcileMode {
    #[default]
    InvalidateOnly,
    Full,
}

/// One reconcile report (§4.7). The `create`/`refresh`/`stale`/`archive`/
/// `supersede` counters mirror the documented action set; `skipped` counts
/// explicit no-ops (unchanged rows, failed embedding retries).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub scope: String,
    pub components: usize,
    pub create: usize,
    pub refresh: usize,
    pub stale: usize,
    pub archive: usize,
    pub supersede: usize,
    pub skipped: usize,
    pub hub_excluded: usize,
    pub capped: usize,
    pub overflow: usize,
}

// ---------------------------------------------------------------------------
// §4.3 Normalization
// ---------------------------------------------------------------------------

/// `normalize_key`: lowercase → trim → collapse whitespace → strip trailing
/// punctuation `[.!?:;,]` → cap 200 runes. Same rules as C9 `normalize_title`
/// minus the path-separator replacement (paths stay readable in keys).
pub fn normalize_key(value: &str) -> String {
    let lowered = value.trim().to_lowercase();
    let mut collapsed = String::with_capacity(lowered.len());
    let mut previous_space = true;
    for ch in lowered.chars() {
        if ch.is_whitespace() {
            if !previous_space {
                collapsed.push(' ');
                previous_space = true;
            }
        } else {
            collapsed.push(ch);
            previous_space = false;
        }
    }
    let mut runes: Vec<char> = collapsed.chars().collect();
    loop {
        if runes
            .last()
            .is_some_and(|c| matches!(c, '.' | '!' | '?' | ':' | ';' | ','))
            || runes.last().is_some_and(|c| c.is_whitespace())
        {
            runes.pop();
            continue;
        }
        break;
    }
    runes.truncate(200);
    runes.into_iter().collect()
}

/// `normalize_path`: `\` → `/`, strip a leading `./`, then `normalize_key`.
pub fn normalize_path(path: &str) -> String {
    let replaced = path.trim().replace('\\', "/");
    let trimmed = replaced.strip_prefix("./").unwrap_or(&replaced);
    normalize_key(trimmed)
}

/// `normalize_symbol`: lowercase + trim (CamelCase matching happens on the
/// tokens, not on the key).
pub fn normalize_symbol(symbol: &str) -> String {
    symbol.trim().to_lowercase()
}

// ---------------------------------------------------------------------------
// §4.3 Signatures
// ---------------------------------------------------------------------------

/// A `(family, key)` pair — the entity-signature node of the pattern graph.
/// The total order is `(family_priority, key)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Signature {
    pub family: String,
    pub key: String,
}

impl Signature {
    pub fn new(family: &str, key: impl Into<String>) -> Self {
        Signature {
            family: family.to_string(),
            key: key.into(),
        }
    }

    /// The readable form `family|key` (node key, debug/UX payload).
    pub fn render(&self) -> String {
        format!("{}|{}", self.family, self.key)
    }

    pub fn priority(&self) -> u8 {
        family_priority(&self.family)
    }
}

impl PartialOrd for Signature {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Signature {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority()
            .cmp(&other.priority())
            .then_with(|| self.key.cmp(&other.key))
    }
}

/// Anchor ordering: `fix_root_cause=0, issue=1, symbol=2, file=3`; unknown
/// families sort last and never compare equal to a known one by priority.
pub fn family_priority(family: &str) -> u8 {
    match family {
        FAMILY_FIX_ROOT_CAUSE => 0,
        FAMILY_ISSUE => 1,
        FAMILY_SYMBOL => 2,
        FAMILY_FILE => 3,
        _ => u8::MAX,
    }
}

/// Human label used by the deterministic fallback name (≤ 80 chars).
pub fn family_label(family: &str) -> &'static str {
    match family {
        FAMILY_FIX_ROOT_CAUSE => "Fix root cause",
        FAMILY_ISSUE => "Issue",
        FAMILY_SYMBOL => "Symbol",
        FAMILY_FILE => "File",
        _ => "Observation",
    }
}

/// `fix_root_cause|normalize_key(root_cause)`; `None` when the normalized
/// value is empty or in the stoplist (hub avoidance).
pub fn fix_root_cause_signature(root_cause: &str) -> Option<Signature> {
    let key = normalize_key(root_cause);
    if ROOT_CAUSE_STOPLIST.contains(&key.as_str()) {
        return None;
    }
    Some(Signature::new(FAMILY_FIX_ROOT_CAUSE, key))
}

/// `issue|<provider_lower>#<external_key_trim>`; `external_key` must be
/// non-empty.
pub fn issue_signature(provider: &str, external_key: &str) -> Option<Signature> {
    let external = external_key.trim();
    if external.is_empty() {
        return None;
    }
    Some(Signature::new(
        FAMILY_ISSUE,
        format!("{}#{}", provider.trim().to_lowercase(), external),
    ))
}

/// `symbol|<normalize_path(rel_path)>#<normalize_symbol(symbol_name)>`.
pub fn symbol_signature(rel_path: &str, symbol_name: &str) -> Option<Signature> {
    let path = normalize_path(rel_path);
    let symbol = normalize_symbol(symbol_name);
    if path.is_empty() || symbol.is_empty() {
        return None;
    }
    Some(Signature::new(FAMILY_SYMBOL, format!("{path}#{symbol}")))
}

/// `file|<normalize_path(rel_path)>`; empty paths yield no signature.
pub fn file_signature(rel_path: &str) -> Option<Signature> {
    let path = normalize_path(rel_path);
    if path.is_empty() {
        return None;
    }
    Some(Signature::new(FAMILY_FILE, path))
}

/// Symbol token matching (§4.3): the memory text `title + " " + content` is
/// tokenized with the shared `retrieval_boost::tokenize_query` (CamelCase
/// split, lowercase); the symbol matches when ALL of its tokens appear.
/// Substrings never match (a token is a token).
pub fn symbol_tokens_match(text: &str, symbol_name: &str) -> bool {
    let (keywords, entities) = crate::retrieval_boost::tokenize_query(text);
    let available: BTreeSet<&str> = keywords
        .iter()
        .chain(entities.iter())
        .map(String::as_str)
        .collect();
    let (symbol_keywords, symbol_entities) = crate::retrieval_boost::tokenize_query(symbol_name);
    let mut symbol_tokens: BTreeSet<&str> = BTreeSet::new();
    symbol_tokens.extend(symbol_keywords.iter().map(String::as_str));
    symbol_tokens.extend(symbol_entities.iter().map(String::as_str));
    if symbol_tokens.is_empty() {
        return false;
    }
    symbol_tokens.iter().all(|token| available.contains(token))
}

// ---------------------------------------------------------------------------
// §4.3 Hash + canonical JSON
// ---------------------------------------------------------------------------

/// 32 hex chars (128 bits) of a SHA-256 digest.
fn hex32(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(32);
    hex
}

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// `signature_hash = sha256("ibis.observation.v1\0" + project_key + "\0" +
/// scope + "\0" + family + "\0" + key)[..32]`. Identity of the observation
/// row: `memory:⟨obs_<signature_hash>⟩`.
pub fn signature_hash(project_key: &str, scope: &str, family: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ibis.observation.v1\0");
    hasher.update(project_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(scope.as_bytes());
    hasher.update(b"\0");
    hasher.update(family.as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    hex32(&hasher.finalize())
}

fn write_json_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Explicit canonical JSON encoder (sorted keys, no spaces): the evidence
/// hash must not depend on `serde_json`/`surrealdb` map insertion order.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical_json(value, &mut out);
    out
}

fn write_canonical_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(number) => out.push_str(&number.to_string()),
        serde_json::Value::String(text) => write_json_string(text, out),
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_canonical_json(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// One member memory of a component, in the shape the evidence hash sees.
/// `title` is semantic evidence too: the deterministic fallback summary and
/// the footer use it, so a title edit must invalidate the observation.
#[derive(Debug, Clone, Default)]
pub struct EvidenceMember {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub content: String,
    pub title: String,
    pub details: serde_json::Value,
}

/// `evidence_hash = sha256("ibis.observation.evidence.v1\0" +
/// canonical_members + "\0" + canonical_signatures + "\0" + canonical_refs)
/// [..32]`. Members are ordered by id, signatures by their total order, refs
/// lexicographically — all inside the function, so the hash is input-order
/// independent. It ignores `updated_at` by construction.
pub fn evidence_hash(
    members: &[EvidenceMember],
    signatures: &[Signature],
    evidence_refs: &[String],
) -> String {
    let mut sorted_members: Vec<&EvidenceMember> = members.iter().collect();
    sorted_members.sort_by(|a, b| a.id.cmp(&b.id));
    let canonical_members = sorted_members
        .iter()
        .map(|member| {
            format!(
                "{}\x1f{}\x1f{}\x1f{}\x1f{}\x1f{}",
                member.id,
                member.kind,
                member.status,
                sha256_hex(&member.title),
                sha256_hex(&member.content),
                sha256_hex(&canonical_json(&member.details)),
            )
        })
        .collect::<Vec<String>>()
        .join("\x1f");
    let mut sorted_signatures: Vec<&Signature> = signatures.iter().collect();
    sorted_signatures.sort();
    let canonical_signatures = sorted_signatures
        .iter()
        .map(|signature| signature.render())
        .collect::<Vec<String>>()
        .join("\x1f");
    let mut sorted_refs: Vec<&String> = evidence_refs.iter().collect();
    sorted_refs.sort();
    let canonical_refs = sorted_refs
        .iter()
        .map(|reference| reference.as_str())
        .collect::<Vec<&str>>()
        .join("\x1f");
    let mut hasher = Sha256::new();
    hasher.update(b"ibis.observation.evidence.v1\0");
    hasher.update(canonical_members.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_signatures.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_refs.as_bytes());
    hex32(&hasher.finalize())
}

// ---------------------------------------------------------------------------
// §4.4 Connected components (union-find, hub exclusion, caps)
// ---------------------------------------------------------------------------

/// One memory with its signature list (the bipartite input).
#[derive(Debug, Clone, Default)]
pub struct ObservationInput {
    pub memory_id: String,
    pub signatures: Vec<Signature>,
}

/// One connected component of the bipartite graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Component {
    /// Member memory ids, sorted by id.
    pub members: Vec<String>,
    /// Signatures of the component, sorted `(family_priority, key)`.
    pub signatures: Vec<Signature>,
    /// Anchor signature = first of `signatures`.
    pub anchor: Signature,
}

impl Component {
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    pub fn anchor_key(&self) -> String {
        self.anchor.render()
    }
}

/// The component computation outcome (persisted set + dropped/logged sets).
#[derive(Debug, Clone, Default)]
pub struct ComponentSet {
    /// Components to persist, ordered `(member_count desc, anchor asc)`
    /// and capped at `max_clusters_per_repo`.
    pub components: Vec<Component>,
    /// Signatures dropped before union (degree > `hub_degree_max`).
    pub hub_excluded: Vec<Signature>,
    /// Oversized components dropped, never truncated (`> max_members`).
    pub capped: Vec<Component>,
    /// Components beyond `max_clusters_per_repo` (not persisted; their
    /// signature hashes are protected from archive/supersede).
    pub overflow: Vec<Component>,
    /// Components below `min_cluster_size` (dropped).
    pub singletons: usize,
}

#[derive(Debug, Default)]
struct UnionFind {
    parent: HashMap<String, String>,
    size: HashMap<String, usize>,
}

impl UnionFind {
    fn find(&mut self, key: &str) -> String {
        let mut root = key.to_string();
        loop {
            let parent = match self.parent.get(&root) {
                Some(parent) if parent != &root => parent.clone(),
                _ => break,
            };
            root = parent;
        }
        let mut node = key.to_string();
        loop {
            let parent = match self.parent.get(&node) {
                Some(parent) if parent != &node => parent.clone(),
                _ => break,
            };
            self.parent.insert(node.clone(), root.clone());
            node = parent;
        }
        root
    }

    fn union(&mut self, a: &str, b: &str) {
        let root_a = self.find(a);
        let root_b = self.find(b);
        if root_a == root_b {
            return;
        }
        let size_a = *self.size.get(&root_a).unwrap_or(&1);
        let size_b = *self.size.get(&root_b).unwrap_or(&1);
        if size_a < size_b {
            self.parent.insert(root_a, root_b.clone());
            self.size.insert(root_b, size_a + size_b);
        } else {
            self.parent.insert(root_b, root_a.clone());
            self.size.insert(root_a, size_a + size_b);
        }
    }
}

/// Builds the connected components (§4.4). Steps: merge duplicate memory
/// inputs, hub-exclude signatures with degree > `hub_degree_max`, union in
/// memory-id order, collect members/signatures in sorted order, drop
/// singletons, drop oversized components, order and cap the output.
pub fn build_components(inputs: &[ObservationInput], limits: &ObservationLimits) -> ComponentSet {
    let mut membership: BTreeMap<String, BTreeSet<Signature>> = BTreeMap::new();
    for input in inputs {
        if input.memory_id.trim().is_empty() {
            continue;
        }
        let entry = membership.entry(input.memory_id.clone()).or_default();
        for signature in &input.signatures {
            entry.insert(signature.clone());
        }
    }

    let mut degree: BTreeMap<Signature, usize> = BTreeMap::new();
    for signatures in membership.values() {
        for signature in signatures {
            *degree.entry(signature.clone()).or_insert(0) += 1;
        }
    }
    let hub_excluded: Vec<Signature> = degree
        .iter()
        .filter(|(_, hits)| **hits > limits.hub_degree_max)
        .map(|(signature, _)| signature.clone())
        .collect();
    let hubs: BTreeSet<Signature> = hub_excluded.iter().cloned().collect();

    let mut union_find = UnionFind::default();
    for (memory_id, signatures) in &membership {
        let memory_node = format!("m:{memory_id}");
        for signature in signatures.iter().filter(|s| !hubs.contains(*s)) {
            union_find.union(&memory_node, &format!("s:{}", signature.render()));
        }
    }

    let mut groups: BTreeMap<String, Component> = BTreeMap::new();
    for (memory_id, signatures) in &membership {
        let kept: Vec<&Signature> = signatures.iter().filter(|s| !hubs.contains(*s)).collect();
        if kept.is_empty() {
            continue;
        }
        let root = union_find.find(&format!("m:{memory_id}"));
        let component = groups.entry(root).or_default();
        component.members.push(memory_id.clone());
        for signature in kept {
            if !component.signatures.contains(signature) {
                component.signatures.push(signature.clone());
            }
        }
    }

    let mut components = Vec::new();
    let mut capped = Vec::new();
    let mut singletons = 0usize;
    for mut component in groups.into_values() {
        component.signatures.sort();
        if let Some(anchor) = component.signatures.first() {
            component.anchor = anchor.clone();
        }
        if component.members.len() < limits.min_cluster_size {
            singletons += 1;
            continue;
        }
        if component.members.len() > limits.max_members {
            capped.push(component);
            continue;
        }
        components.push(component);
    }
    components.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.anchor.cmp(&b.anchor))
    });
    let overflow = if components.len() > limits.max_clusters_per_repo {
        components.split_off(limits.max_clusters_per_repo)
    } else {
        Vec::new()
    };

    ComponentSet {
        components,
        hub_excluded,
        capped,
        overflow,
        singletons,
    }
}

// ---------------------------------------------------------------------------
// §4.4/§4.5 Deterministic identity, confidence, fallback summary
// ---------------------------------------------------------------------------

/// Deterministic observation row id: `memory:⟨obs_<signature_hash>⟩`.
pub fn observation_id(signature_hash: &str) -> String {
    ibis_db::ids::format_record_id(TABLE_MEMORY, &format!("obs_{signature_hash}"))
}

/// `min(0.95, 0.5 + 0.5*(1 - 1/member_count))`, rounded to 2 decimals.
pub fn observation_confidence(member_count: usize) -> f64 {
    if member_count == 0 {
        return 0.5;
    }
    let raw = 0.5 + 0.5 * (1.0 - 1.0 / member_count as f64);
    let capped = raw.min(0.95);
    (capped * 100.0).round() / 100.0
}

/// Rune-bounded truncation (never splits a char).
pub fn truncate_runes(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn member_title(member: &EvidenceMember) -> String {
    let title = member.title.trim();
    if !title.is_empty() {
        return truncate_runes(title, 80);
    }
    let first_line = member.content.lines().next().unwrap_or_default().trim();
    truncate_runes(first_line, 80)
}

/// The deterministic fallback summary (§4.8): valid without any LLM, and the
/// artifact the next synthesis pass upgrades.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FallbackSummary {
    pub name: String,
    pub summary: String,
}

/// `name = "<family_label>: <anchor_key>"` (cap 80) and
/// `summary = "<n> memories share <signature>. " + first 5 member titles`.
pub fn fallback_summary(anchor: &Signature, members: &[EvidenceMember]) -> FallbackSummary {
    let name = truncate_runes(
        &format!("{}: {}", family_label(&anchor.family), anchor.key),
        80,
    );
    let mut sorted: Vec<&EvidenceMember> = members.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let head = format!("{} memories share {}.", sorted.len(), anchor.render());
    let titles = sorted
        .iter()
        .take(5)
        .map(|member| member_title(member))
        .collect::<Vec<String>>();
    let summary = if titles.is_empty() {
        head
    } else {
        format!("{head} {}", titles.join(" · "))
    };
    FallbackSummary { name, summary }
}

/// Footer (§4.8): `summary + "\n\nMembers:\n" + "- <kind>: <title>"` for up
/// to 12 members, so the row is self-sufficient and vector matches cover the
/// member terms too.
pub fn render_observation_content(summary: &str, members: &[EvidenceMember]) -> String {
    let mut sorted: Vec<&EvidenceMember> = members.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = String::with_capacity(summary.len() + 128);
    out.push_str(summary);
    out.push_str("\n\nMembers:\n");
    let lines = sorted
        .iter()
        .take(12)
        .map(|member| format!("- {}: {}", member.kind.trim(), member_title(member)))
        .collect::<Vec<String>>();
    out.push_str(&lines.join("\n"));
    out
}

// ---------------------------------------------------------------------------
// §4.8 Summary validation (pure; the LLM call itself is step 5)
// ---------------------------------------------------------------------------

/// The record-id prefixes a model response may cite; anything else that
/// *looks* like a record id is a foreign id and rejects the response.
pub const RECORD_ID_PREFIXES: [&str; 6] = [
    "memory:",
    "commit:",
    "issue:",
    "source_file:",
    "file_chunk:",
    "symbol:",
];

/// Strips markdown fences (pattern `parse_consolidation_decision`).
pub fn strip_json_fences(text: &str) -> String {
    let mut text = text.trim().to_string();
    if let Some(stripped) = text.strip_prefix("```json") {
        text = stripped.to_string();
    }
    if let Some(stripped) = text.strip_prefix("```") {
        text = stripped.to_string();
    }
    if let Some(stripped) = text.strip_suffix("```") {
        text = stripped.to_string();
    }
    text.trim().to_string()
}

fn normalize_record_id(value: &str) -> String {
    value.trim().replace(['⟨', '⟩', '`'], "")
}

fn is_id_body_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '⟨' | '⟩')
}

/// True when every record-id-looking token in `text` is a mention of one of
/// the allowed ids (`members` + `evidence_refs`). A token that continues an
/// allowed id with more id characters is foreign and rejects.
fn mentions_only_allowed_ids(text: &str, allowed: &[String]) -> bool {
    let normalized_text = normalize_record_id(text);
    for (index, _) in normalized_text.char_indices() {
        if index > 0 {
            let previous = normalized_text[..index].chars().next_back().unwrap_or(' ');
            if is_id_body_char(previous) {
                continue;
            }
        }
        let rest = &normalized_text[index..];
        if !RECORD_ID_PREFIXES
            .iter()
            .any(|prefix| rest.starts_with(prefix))
        {
            continue;
        }
        let valid = allowed.iter().any(|id| {
            let normalized_id = normalize_record_id(id);
            match rest.strip_prefix(normalized_id.as_str()) {
                Some(tail) => !tail.chars().next().is_some_and(is_id_body_char),
                None => false,
            }
        });
        if !valid {
            return false;
        }
    }
    true
}

/// A validated synthesis response (name ≤ 80 runes, summary ≤ 1200 runes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSummary {
    pub name: String,
    pub summary: String,
}

/// Validates the model response (§4.8): fence strip, non-empty name/summary,
/// truncation (never an error) and the "only real members" id check. `None`
/// means the caller falls back to the deterministic summary.
pub fn validate_observation_summary(
    name: &str,
    summary: &str,
    members: &[String],
    evidence_refs: &[String],
) -> Option<ValidatedSummary> {
    let name = strip_json_fences(name);
    let summary = strip_json_fences(summary);
    if name.trim().is_empty() || summary.trim().is_empty() {
        return None;
    }
    let mut allowed: Vec<String> = Vec::with_capacity(members.len() + evidence_refs.len());
    allowed.extend(members.iter().cloned());
    allowed.extend(evidence_refs.iter().cloned());
    if !mentions_only_allowed_ids(&format!("{name} {summary}"), &allowed) {
        return None;
    }
    Some(ValidatedSummary {
        name: truncate_runes(name.trim(), 80),
        summary: truncate_runes(summary.trim(), 1200),
    })
}

// ---------------------------------------------------------------------------
// Scope + extraction (§4.2)
// ---------------------------------------------------------------------------

/// The reconcile scope: `team` or `personal:<owner>` (§4.3: the hash scope is
/// `team` | `personal:` + sanitize_id(owner)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObsScope {
    Team,
    Personal(String),
}

impl ObsScope {
    /// Parses `team` | `personal:<owner>`; an empty personal owner parses and
    /// is skipped by the caller (§4.4 edge case).
    pub fn parse(scope: &str) -> Result<Self> {
        let scope = scope.trim();
        if scope.eq_ignore_ascii_case("team") {
            return Ok(ObsScope::Team);
        }
        if let Some(owner) = scope.strip_prefix("personal:") {
            return Ok(ObsScope::Personal(owner.trim().to_string()));
        }
        Err(SearchError::other(format!(
            "observation scope must be `team` or `personal:<owner>`, got {scope:?}"
        )))
    }

    /// Wire label (`team` | `personal:<owner>`).
    pub fn label(&self) -> String {
        match self {
            ObsScope::Team => "team".to_string(),
            ObsScope::Personal(owner) => format!("personal:{owner}"),
        }
    }

    /// The `scope` component of `signature_hash`.
    pub fn hash_scope(&self) -> String {
        match self {
            ObsScope::Team => "team".to_string(),
            ObsScope::Personal(owner) => {
                format!("personal:{}", ibis_db::ids::sanitize_id(owner))
            }
        }
    }

    fn sql_predicate(&self) -> (String, Bindings) {
        match self {
            ObsScope::Team => ("visibility = 'team'".to_string(), Vec::new()),
            ObsScope::Personal(owner) => (
                "visibility = 'personal' AND owner = $owner".to_string(),
                vec![("owner".into(), Value::from_t(owner.as_str()))],
            ),
        }
    }

    fn visibility(&self) -> &'static str {
        match self {
            ObsScope::Team => "team",
            ObsScope::Personal(_) => "personal",
        }
    }

    fn owner(&self) -> &str {
        match self {
            ObsScope::Team => OBSERVATION_AGENT,
            ObsScope::Personal(owner) => owner,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ObsMemoryRow {
    id: String,
    kind: String,
    title: String,
    content: String,
    details: serde_json::Value,
    root_cause: String,
    source_path: String,
    introduced_in: String,
}

#[derive(Debug, Clone, Default)]
struct ObsFileEdge {
    memory_id: String,
    /// Decoded `source_file:<key>` string (map key, evidence ref).
    file_id: String,
    /// Raw `out` record value from Q2. `source_file` ids use array keys
    /// (`source_file:[repo:…, 'path']`); re-parsing them through
    /// `record_id_value` would flatten the key into a `String` that never
    /// matches `file IN $files`, so Q6 binds this raw value instead.
    file_record: Value,
    rel_path: String,
}

#[derive(Debug, Clone, Default)]
struct ObsCommitEdge {
    memory_id: String,
    commit_id: String,
}

#[derive(Debug, Clone, Default)]
struct ObsIssueEdge {
    commit_id: String,
    issue_id: String,
}

#[derive(Debug, Clone, Default)]
struct ObsIssueRow {
    provider: String,
    external_key: String,
}

#[derive(Debug, Clone, Default)]
struct ObsSymbolRow {
    symbol_name: String,
}

/// One existing derived observation as projected by Q7 (the diff side).
#[derive(Debug, Clone, Default)]
pub struct ExistingObservation {
    pub id: String,
    pub status: String,
    pub signature_hash: String,
    pub evidence_hash: String,
    pub members: Vec<String>,
    pub family: String,
    pub generated_by: String,
    pub has_embedding: bool,
    pub title: String,
    pub content: String,
    pub confidence: f64,
    pub owner: String,
    pub visibility: String,
}

#[derive(Debug, Clone, Default)]
struct Extraction {
    memories: Vec<ObsMemoryRow>,
    file_edges: Vec<ObsFileEdge>,
    commit_edges: Vec<ObsCommitEdge>,
    issue_edges: Vec<ObsIssueEdge>,
    issues: BTreeMap<String, ObsIssueRow>,
    symbols: BTreeMap<String, Vec<ObsSymbolRow>>,
    existing: Vec<ExistingObservation>,
    /// Q1 returned more than `max_members_examined` decodable rows: the
    /// window is incomplete, so no row may be archived/superseded.
    truncated: bool,
    /// One of Q2/Q3/Q6/Q7 hit `MAX_GRAPH_ROWS`: the evidence graph is
    /// incomplete, so no row may be archived/superseded.
    graph_truncated: bool,
}

impl Extraction {
    /// True when any bounded window was truncated (Q1 or the graph queries).
    fn is_truncated(&self) -> bool {
        self.truncated || self.graph_truncated
    }
}

fn object_rows(outcome: &ibis_db::executor::QueryOutcome) -> Vec<Object> {
    rows::normalize_rows(outcome)
        .map(|values| {
            rows::object_rows(&values)
                .into_iter()
                .cloned()
                .collect::<Vec<Object>>()
        })
        .unwrap_or_default()
}

fn string_array(values: impl IntoIterator<Item = String>) -> Value {
    Value::from_t(
        values
            .into_iter()
            .map(Value::from_t)
            .collect::<Vec<Value>>(),
    )
}

/// Parses a decoded `table:key` record id into a bindable record value.
/// SurrealDB does not coerce strings in `field IN $array` against record
/// fields (live-verified on 4.0), so the bounded arrays bind as records;
/// numeric keys keep the `issue` number-key exception (`format_record_id`).
fn record_id_value(plain_id: &str) -> Option<Value> {
    let (table, key) = plain_id.trim().split_once(':')?;
    let table = table.trim();
    let key = key.trim().trim_matches(|c| c == '⟨' || c == '⟩');
    if table.is_empty() || key.is_empty() {
        return None;
    }
    let key = if table == "issue" && key.bytes().all(|b| b.is_ascii_digit()) {
        RecordIdKey::Number(key.parse::<i64>().ok()?)
    } else {
        RecordIdKey::String(key.to_string())
    };
    Some(Value::RecordId(RecordId::new(table, key)))
}

/// Binds a bounded id set as record values (skipping unparsable ids).
fn record_array(values: impl IntoIterator<Item = String>) -> Value {
    Value::from_t(
        values
            .into_iter()
            .filter_map(|value| record_id_value(&value))
            .collect::<Vec<Value>>(),
    )
}

/// Binds the Q6 `$files` set from the Q2 edges. The raw `out` record value is
/// preserved because `source_file` ids use array keys
/// (`source_file:[repo:…, 'path']`) that `record_id_value` would flatten into
/// a `RecordIdKey::String` — a value that never matches `file IN $files`.
/// Dedup is by the decoded string id; rows without a raw record (fake
/// executors returning plain strings) fall back to `record_id_value`.
fn file_record_values(edges: &[ObsFileEdge]) -> Value {
    let mut seen: BTreeMap<String, Value> = BTreeMap::new();
    for edge in edges {
        if edge.file_id.is_empty() || seen.contains_key(&edge.file_id) {
            continue;
        }
        let value = match &edge.file_record {
            Value::RecordId(_) => edge.file_record.clone(),
            _ => match record_id_value(&edge.file_id) {
                Some(value) => value,
                None => continue,
            },
        };
        seen.insert(edge.file_id.clone(), value);
    }
    Value::from_t(seen.into_values().collect::<Vec<Value>>())
}

/// True for keys SurrealQL parses as a bare record-id identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`). Numeric keys are handled separately.
fn is_bare_record_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Renders a `derived_from` edge target as a SurrealQL record identifier.
///
/// `is_safe_record_id` is not sufficient for interpolation: `issue:IBIS-42`
/// passes it, but SurrealQL parses the hyphen as subtraction. Ordinary string
/// keys are therefore quoted through `format_record_id`
/// (`issue:⟨IBIS-42⟩`, `commit:⟨abc-def⟩`); bare identifier keys
/// (`memory:mem_a`), array keys (`source_file:[…]`, `file_chunk:[…]`),
/// already-wrapped keys (`table:⟨…⟩` / ``table:`…` ``) and numeric keys are
/// valid as-is. `None` means the id is unsafe and the caller must skip it.
fn edge_target_identifier(target: &str) -> Option<String> {
    let target = target.trim();
    if !ibis_db::ids::is_safe_record_id(target) {
        return None;
    }
    let (table, key) = target.split_once(':')?;
    let table = table.trim();
    let key = key.trim();
    if table.is_empty() || key.is_empty() {
        return None;
    }
    let literal = (key.starts_with('[') && key.ends_with(']'))
        || (key.starts_with('⟨') && key.ends_with('⟩'))
        || (key.starts_with('`') && key.ends_with('`'));
    if literal || key.bytes().all(|b| b.is_ascii_digit()) || is_bare_record_key(key) {
        return Some(target.to_string());
    }
    Some(ibis_db::ids::format_record_id(table, key))
}

/// Decoded record ids are plain (`memory:obs_x`); ids the engine builds are
/// wrapped (`memory:⟨obs_x⟩`) — comparisons normalize both sides.
fn plain_observation_id(id: &str) -> String {
    id.trim().replace(['⟨', '⟩'], "")
}

fn scalar(value: &Value) -> String {
    rows::scalar_string(value).trim().to_string()
}

impl SearchService {
    /// Extracts the bounded evidence graph (§4.2): Q1 (memory window) always
    /// runs, Q7 (existing derived rows) always runs, Q2–Q6 run only when
    /// their bound array is non-empty. Any query failure aborts with `Err`.
    async fn extract_observation_graph(
        &self,
        db: &std::sync::Arc<dyn Executor>,
        project: &str,
        scope: &ObsScope,
        limits: &ObservationLimits,
    ) -> Result<Extraction> {
        let repo_id = repo_record_id(project);
        let (scope_sql, scope_bindings) = scope.sql_predicate();
        let examined = limits
            .max_members_examined
            .clamp(1, OBSERVATION_MAX_EXAMINED);
        // One extra row tells whether the window was truncated.
        let q1_limit = examined + 1;
        let graph_limit = MAX_GRAPH_ROWS + 1;
        let mut graph_truncated = false;

        // Q1: eligible memories (one row per memory, C7 details + anchors).
        let q1_sql = format!(
            "SELECT id, kind, title, content, confidence, visibility, owner, details, details.root_cause AS root_cause, provenance.source_path AS source_path, provenance.introduced_in_commit AS introduced_in FROM memory WHERE project = {repo_id} AND status = 'current' AND kind IN $kinds AND (store = NONE OR store != 'derived') AND ({scope_sql}) ORDER BY id LIMIT {q1_limit};"
        );
        let mut q1_vars: Vars = bind(scope_bindings.clone());
        q1_vars.insert(
            "kinds".into(),
            string_array(OBSERVATION_KINDS.iter().map(|k| k.to_string())),
        );
        let q1_outcome = db
            .smart_query(&q1_sql, q1_vars)
            .await
            .map_err(|e| SearchError::other(format!("observation extract q1: {e}")))?;
        let mut memories: Vec<ObsMemoryRow> = Vec::new();
        for row in object_rows(&q1_outcome) {
            let id = rows::record_id(rows::field(&row, "id"));
            if !id.starts_with("memory:") {
                continue;
            }
            let details = details::details_json_from_value(rows::field(&row, "details"));
            let mut root_cause = scalar(rows::field(&row, "root_cause"));
            if root_cause.is_empty() {
                root_cause = details
                    .get("root_cause")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
            }
            memories.push(ObsMemoryRow {
                id,
                kind: scalar(rows::field(&row, "kind")).to_lowercase(),
                title: rows::scalar_string(rows::field(&row, "title")),
                content: rows::scalar_string(rows::field(&row, "content")),
                details,
                root_cause,
                source_path: scalar(rows::field(&row, "source_path")),
                introduced_in: rows::record_id(rows::field(&row, "introduced_in")),
            });
        }
        let mut truncated = false;
        if memories.len() > examined {
            memories.truncate(examined);
            truncated = true;
        }
        let memory_ids: Vec<String> = {
            let unique: BTreeSet<String> = memories.iter().map(|m| m.id.clone()).collect();
            unique.into_iter().collect()
        };

        // Q2: memory -> source_file edges (path-discriminated, prefix
        // re-checked in Rust).
        let mut file_edges: Vec<ObsFileEdge> = Vec::new();
        if !memory_ids.is_empty() {
            let q2_sql = format!(
                "SELECT in AS memory_id, out AS file_id, out.rel_path AS rel_path, out.path AS path FROM derived_from WHERE in IN $memory_ids AND out.path != NONE ORDER BY memory_id, rel_path LIMIT {graph_limit};"
            );
            let outcome = db
                .smart_query(
                    &q2_sql,
                    bind(vec![(
                        "memory_ids".into(),
                        record_array(memory_ids.clone()),
                    )]),
                )
                .await
                .map_err(|e| SearchError::other(format!("observation extract q2: {e}")))?;
            for (index, row) in object_rows(&outcome).into_iter().enumerate() {
                if index >= MAX_GRAPH_ROWS {
                    graph_truncated = true;
                    break;
                }
                let memory_id = rows::record_id(rows::field(&row, "memory_id"));
                let file_id = rows::record_id(rows::field(&row, "file_id"));
                if !file_id.starts_with("source_file:") {
                    continue;
                }
                let mut rel_path = scalar(rows::field(&row, "rel_path"));
                if rel_path.is_empty() {
                    rel_path = scalar(rows::field(&row, "path"));
                }
                if rel_path.is_empty() || memory_id.is_empty() {
                    continue;
                }
                file_edges.push(ObsFileEdge {
                    memory_id,
                    file_id,
                    file_record: rows::field(&row, "file_id").clone(),
                    rel_path,
                });
            }
        }

        // Q3: memory -> commit edges (date-discriminated union member).
        let mut commit_edges: Vec<ObsCommitEdge> = Vec::new();
        if !memory_ids.is_empty() {
            let q3_sql = format!(
                "SELECT in AS memory_id, out AS commit_id, out.date AS commit_date FROM derived_from WHERE in IN $memory_ids AND out.date != NONE ORDER BY memory_id, commit_id LIMIT {graph_limit};"
            );
            let outcome = db
                .smart_query(
                    &q3_sql,
                    bind(vec![(
                        "memory_ids".into(),
                        record_array(memory_ids.clone()),
                    )]),
                )
                .await
                .map_err(|e| SearchError::other(format!("observation extract q3: {e}")))?;
            for (index, row) in object_rows(&outcome).into_iter().enumerate() {
                if index >= MAX_GRAPH_ROWS {
                    graph_truncated = true;
                    break;
                }
                let memory_id = rows::record_id(rows::field(&row, "memory_id"));
                let commit_id = rows::record_id(rows::field(&row, "commit_id"));
                if memory_id.is_empty() || !commit_id.starts_with("commit:") {
                    continue;
                }
                commit_edges.push(ObsCommitEdge {
                    memory_id,
                    commit_id,
                });
            }
        }
        let commits: Vec<String> = {
            let unique: BTreeSet<String> = commit_edges
                .iter()
                .map(|edge| edge.commit_id.clone())
                .collect();
            unique.into_iter().collect()
        };

        // Q4: commit -> issue (implements).
        let mut issue_edges: Vec<ObsIssueEdge> = Vec::new();
        if !commits.is_empty() {
            let q4_sql = "SELECT in AS commit_id, out AS issue_id FROM implements WHERE in IN $commits ORDER BY commit_id, issue_id;";
            let outcome = db
                .smart_query(
                    q4_sql,
                    bind(vec![("commits".into(), record_array(commits.clone()))]),
                )
                .await
                .map_err(|e| SearchError::other(format!("observation extract q4: {e}")))?;
            for row in object_rows(&outcome) {
                let commit_id = rows::record_id(rows::field(&row, "commit_id"));
                let issue_id = rows::record_id(rows::field(&row, "issue_id"));
                if commit_id.is_empty() || issue_id.is_empty() {
                    continue;
                }
                issue_edges.push(ObsIssueEdge {
                    commit_id,
                    issue_id,
                });
            }
        }
        let issues: Vec<String> = {
            let unique: BTreeSet<String> = issue_edges
                .iter()
                .map(|edge| edge.issue_id.clone())
                .collect();
            unique.into_iter().collect()
        };

        // Q5: canonical issue keys.
        let mut issue_rows: BTreeMap<String, ObsIssueRow> = BTreeMap::new();
        if !issues.is_empty() {
            let q5_sql = "SELECT id, provider, external_key FROM issue WHERE id IN $issues;";
            let outcome = db
                .smart_query(
                    q5_sql,
                    bind(vec![("issues".into(), record_array(issues.clone()))]),
                )
                .await
                .map_err(|e| SearchError::other(format!("observation extract q5: {e}")))?;
            for row in object_rows(&outcome) {
                let id = rows::record_id(rows::field(&row, "id"));
                if id.is_empty() {
                    continue;
                }
                issue_rows.insert(
                    id,
                    ObsIssueRow {
                        provider: scalar(rows::field(&row, "provider")),
                        external_key: scalar(rows::field(&row, "external_key")),
                    },
                );
            }
        }

        // Q6: symbols per file (file_chunk.symbol_name — NOT the `symbol`
        // table, whose `kind` field is undeclared). `$files` binds the raw Q2
        // record values: `source_file` array keys cannot be rebuilt from
        // their string form.
        let mut symbols: BTreeMap<String, Vec<ObsSymbolRow>> = BTreeMap::new();
        let files = file_record_values(&file_edges);
        let has_files = matches!(&files, Value::Array(items) if !items.is_empty());
        if has_files {
            let q6_sql = format!(
                "SELECT file AS file_id, symbol_name, start_line, end_line FROM file_chunk WHERE file IN $files AND symbol_name != NONE AND symbol_name != '' ORDER BY file_id, start_line LIMIT {graph_limit};"
            );
            let outcome = db
                .smart_query(&q6_sql, bind(vec![("files".into(), files)]))
                .await
                .map_err(|e| SearchError::other(format!("observation extract q6: {e}")))?;
            for (index, row) in object_rows(&outcome).into_iter().enumerate() {
                if index >= MAX_GRAPH_ROWS {
                    graph_truncated = true;
                    break;
                }
                let file_id = rows::record_id(rows::field(&row, "file_id"));
                let symbol_name = scalar(rows::field(&row, "symbol_name"));
                if file_id.is_empty() || symbol_name.is_empty() {
                    continue;
                }
                symbols
                    .entry(file_id)
                    .or_default()
                    .push(ObsSymbolRow { symbol_name });
            }
        }

        // Q7: existing derived observations (reconciliation diff).
        let q7_sql = format!(
            "SELECT id, status, title, content, details.signature_hash AS signature_hash, details.evidence_hash AS evidence_hash, details.members AS members, details.family AS family, details.generated_by AS generated_by, embedding IS NOT NONE AS has_embedding, confidence, owner, visibility FROM memory WHERE project = {repo_id} AND kind = 'observation' AND store = 'derived' AND ({scope_sql}) ORDER BY id LIMIT {graph_limit};"
        );
        let q7_outcome = db
            .smart_query(&q7_sql, bind(scope_bindings))
            .await
            .map_err(|e| SearchError::other(format!("observation extract q7: {e}")))?;
        let mut existing: Vec<ExistingObservation> = Vec::new();
        for (index, row) in object_rows(&q7_outcome).into_iter().enumerate() {
            if index >= MAX_GRAPH_ROWS {
                graph_truncated = true;
                break;
            }
            let id = rows::record_id(rows::field(&row, "id"));
            if !id.starts_with("memory:") {
                continue;
            }
            existing.push(ExistingObservation {
                id,
                status: scalar(rows::field(&row, "status")),
                signature_hash: scalar(rows::field(&row, "signature_hash")),
                evidence_hash: scalar(rows::field(&row, "evidence_hash")),
                members: rows::string_slice(rows::field(&row, "members")),
                family: scalar(rows::field(&row, "family")),
                generated_by: scalar(rows::field(&row, "generated_by")),
                has_embedding: matches!(rows::field(&row, "has_embedding"), Value::Bool(true)),
                title: rows::scalar_string(rows::field(&row, "title")),
                content: rows::scalar_string(rows::field(&row, "content")),
                confidence: rows::numeric_float(rows::field(&row, "confidence")),
                owner: scalar(rows::field(&row, "owner")),
                visibility: scalar(rows::field(&row, "visibility")),
            });
        }

        Ok(Extraction {
            memories,
            file_edges,
            commit_edges,
            issue_edges,
            issues: issue_rows,
            symbols,
            existing,
            truncated,
            graph_truncated,
        })
    }

    /// Live successors of every superseded observation (§4.7 "superseded with
    /// a live successor → no-op"): `in` rows whose status is still `current`.
    /// One bounded query, skipped when no superseded row exists.
    async fn load_live_successors(
        &self,
        db: &std::sync::Arc<dyn Executor>,
        existing: &[ExistingObservation],
    ) -> Result<HashMap<String, usize>> {
        let superseded: Vec<String> = existing
            .iter()
            .filter(|row| row.status == "superseded")
            .map(|row| row.id.clone())
            .collect();
        if superseded.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = "SELECT in AS successor_id, out AS old_id FROM supersedes WHERE out IN $obs_ids;";
        let outcome = db
            .smart_query(
                sql,
                bind(vec![("obs_ids".into(), record_array(superseded))]),
            )
            .await
            .map_err(|e| SearchError::other(format!("observation successors: {e}")))?;
        let status_by_id: HashMap<String, &str> = existing
            .iter()
            .map(|row| (plain_observation_id(&row.id), row.status.as_str()))
            .collect();
        let mut live: HashMap<String, usize> = HashMap::new();
        for row in object_rows(&outcome) {
            let successor_id =
                plain_observation_id(&rows::record_id(rows::field(&row, "successor_id")));
            let old_id = plain_observation_id(&rows::record_id(rows::field(&row, "old_id")));
            if successor_id.is_empty() || old_id.is_empty() {
                continue;
            }
            let successor_current = status_by_id
                .get(successor_id.as_str())
                .is_some_and(|status| *status == "current");
            if successor_current {
                *live.entry(old_id).or_insert(0) += 1;
            }
        }
        Ok(live)
    }
}

// ---------------------------------------------------------------------------
// §4.3–§4.5 Component computation + diff
// ---------------------------------------------------------------------------

struct SignatureInputs {
    inputs: Vec<ObservationInput>,
    members: BTreeMap<String, EvidenceMember>,
    refs: BTreeMap<String, BTreeSet<String>>,
}

fn build_signature_inputs(graph: &Extraction, families: &[String]) -> SignatureInputs {
    let allow = |family: &str| families.iter().any(|allowed| allowed == family);
    let mut files_by_memory: BTreeMap<&str, Vec<&ObsFileEdge>> = BTreeMap::new();
    for edge in &graph.file_edges {
        files_by_memory
            .entry(edge.memory_id.as_str())
            .or_default()
            .push(edge);
    }
    let mut commits_by_memory: BTreeMap<&str, Vec<&ObsCommitEdge>> = BTreeMap::new();
    for edge in &graph.commit_edges {
        commits_by_memory
            .entry(edge.memory_id.as_str())
            .or_default()
            .push(edge);
    }
    let mut issues_by_commit: BTreeMap<&str, Vec<&ObsIssueEdge>> = BTreeMap::new();
    for edge in &graph.issue_edges {
        issues_by_commit
            .entry(edge.commit_id.as_str())
            .or_default()
            .push(edge);
    }

    let mut inputs: Vec<ObservationInput> = Vec::with_capacity(graph.memories.len());
    let mut members: BTreeMap<String, EvidenceMember> = BTreeMap::new();
    let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for memory in &graph.memories {
        let mut signatures: BTreeSet<Signature> = BTreeSet::new();
        let mut member_refs: BTreeSet<String> = BTreeSet::new();

        if allow(FAMILY_FIX_ROOT_CAUSE) && memory.kind == "fix" {
            if let Some(signature) = fix_root_cause_signature(&memory.root_cause) {
                signatures.insert(signature);
            }
        }

        if let Some(edges) = files_by_memory.get(memory.id.as_str()) {
            for edge in edges {
                if allow(FAMILY_FILE) {
                    if let Some(signature) = file_signature(&edge.rel_path) {
                        signatures.insert(signature);
                    }
                }
                member_refs.insert(edge.file_id.clone());
                if allow(FAMILY_SYMBOL) {
                    if let Some(symbols) = graph.symbols.get(&edge.file_id) {
                        let text = format!("{} {}", memory.title, memory.content);
                        for symbol in symbols {
                            if symbol_tokens_match(&text, &symbol.symbol_name) {
                                if let Some(signature) =
                                    symbol_signature(&edge.rel_path, &symbol.symbol_name)
                                {
                                    signatures.insert(signature);
                                }
                            }
                        }
                    }
                }
            }
        }
        if allow(FAMILY_FILE) {
            if let Some(signature) = file_signature(&memory.source_path) {
                signatures.insert(signature);
            }
        }

        let mut commits: BTreeSet<String> = BTreeSet::new();
        if let Some(edges) = commits_by_memory.get(memory.id.as_str()) {
            for edge in edges {
                commits.insert(edge.commit_id.clone());
                member_refs.insert(edge.commit_id.clone());
            }
        }
        if memory.introduced_in.starts_with("commit:") {
            commits.insert(memory.introduced_in.clone());
            member_refs.insert(memory.introduced_in.clone());
        }
        if allow(FAMILY_ISSUE) {
            for commit in &commits {
                if let Some(edges) = issues_by_commit.get(commit.as_str()) {
                    for edge in edges {
                        if let Some(issue) = graph.issues.get(&edge.issue_id) {
                            if let Some(signature) =
                                issue_signature(&issue.provider, &issue.external_key)
                            {
                                signatures.insert(signature);
                                member_refs.insert(edge.issue_id.clone());
                            }
                        }
                    }
                }
            }
        }

        members.insert(
            memory.id.clone(),
            EvidenceMember {
                id: memory.id.clone(),
                kind: memory.kind.clone(),
                status: "current".to_string(),
                content: memory.content.clone(),
                title: memory.title.clone(),
                details: memory.details.clone(),
            },
        );
        refs.insert(memory.id.clone(), member_refs);
        inputs.push(ObservationInput {
            memory_id: memory.id.clone(),
            signatures: signatures.into_iter().collect(),
        });
    }

    SignatureInputs {
        inputs,
        members,
        refs,
    }
}

/// A component with every value the persistence transaction needs.
struct ComputedComponent {
    id: String,
    signature_hash: String,
    evidence_hash: String,
    signature: String,
    anchor: Signature,
    signatures: Vec<String>,
    family: String,
    confidence: f64,
    visibility: String,
    owner: String,
    members: Vec<String>,
    member_evidence: Vec<EvidenceMember>,
    evidence_refs: Vec<String>,
}

fn compute_component(
    project_key: &str,
    scope: &ObsScope,
    component: &Component,
    members: &BTreeMap<String, EvidenceMember>,
    refs: &BTreeMap<String, BTreeSet<String>>,
) -> ComputedComponent {
    let member_evidence: Vec<EvidenceMember> = component
        .members
        .iter()
        .filter_map(|member| members.get(member))
        .cloned()
        .collect();
    let mut evidence_refs: BTreeSet<String> = BTreeSet::new();
    for member in &component.members {
        if let Some(member_refs) = refs.get(member) {
            evidence_refs.extend(member_refs.iter().cloned());
        }
    }
    let evidence_refs: Vec<String> = evidence_refs.into_iter().collect();
    let anchor = component.anchor.clone();
    let signature_hash = signature_hash(
        project_key,
        &scope.hash_scope(),
        &anchor.family,
        &anchor.key,
    );
    let id = observation_id(&signature_hash);
    let evidence_hash = evidence_hash(&member_evidence, &component.signatures, &evidence_refs);
    ComputedComponent {
        id,
        signature_hash,
        evidence_hash,
        signature: anchor.render(),
        anchor,
        signatures: component.signatures.iter().map(Signature::render).collect(),
        family: component.anchor.family.clone(),
        confidence: observation_confidence(component.members.len()),
        visibility: scope.visibility().to_string(),
        owner: scope.owner().to_string(),
        members: component.members.clone(),
        member_evidence,
        evidence_refs,
    }
}

/// One planned reconcile action (§4.7 idempotency table).
#[derive(Debug)]
enum PlanAction {
    /// Only `Full`: create the row with the fallback summary + embedding.
    Create(usize),
    /// Regenerate content/title/embedding, status ends `current`.
    Refresh(usize),
    /// Only `InvalidateOnly`: structural update + `status='stale'`.
    Stale(usize),
    /// Archived / superseded-without-live-successor row matchable again.
    Reactivate {
        index: usize,
        regenerate: bool,
        stale: bool,
    },
    /// `current` + `llm` + `evidence_hash == E` + no embedding: retry only
    /// the embedding (zero LLM).
    EmbeddingRetry(usize),
    /// Vanished row, no shared core with any new component.
    Archive(String),
    /// Vanished row replaced by a new component with a shared core ≥ 2.
    Supersede { index: usize, old: String },
    /// Row withheld from archive/supersede/reactivation because the
    /// extraction is incomplete (truncated window) or its signature is
    /// protected (overflow component beyond `max_clusters_per_repo`).
    /// Counted, never written.
    Skip(String),
}

fn plan_reconcile(
    computed: &[ComputedComponent],
    existing: &[ExistingObservation],
    mode: ReconcileMode,
    live_successors: &HashMap<String, usize>,
    protected: &BTreeSet<String>,
    truncated: bool,
) -> Vec<PlanAction> {
    let full = mode == ReconcileMode::Full;
    let by_hash: HashMap<&str, &ExistingObservation> = existing
        .iter()
        .map(|row| (row.signature_hash.as_str(), row))
        .collect();
    let computed_hashes: BTreeSet<&str> = computed
        .iter()
        .map(|component| component.signature_hash.as_str())
        .collect();

    let mut actions = Vec::new();
    for (index, component) in computed.iter().enumerate() {
        match by_hash.get(component.signature_hash.as_str()) {
            None => {
                if full {
                    actions.push(PlanAction::Create(index));
                }
            }
            Some(row) => match row.status.as_str() {
                "current" => {
                    if row.evidence_hash == component.evidence_hash {
                        let fallback =
                            row.generated_by.trim().is_empty() || row.generated_by == "fallback";
                        if fallback {
                            if full {
                                actions.push(PlanAction::Refresh(index));
                            }
                        } else if !row.has_embedding && full {
                            actions.push(PlanAction::EmbeddingRetry(index));
                        }
                    } else if full {
                        actions.push(PlanAction::Refresh(index));
                    } else {
                        actions.push(PlanAction::Stale(index));
                    }
                }
                "stale" => {
                    if full {
                        actions.push(PlanAction::Refresh(index));
                    }
                }
                "archived" => {
                    if truncated {
                        // A truncated window cannot prove the row is really
                        // back: reactivating could duplicate a current row.
                        actions.push(PlanAction::Skip(row.id.clone()));
                    } else {
                        actions.push(PlanAction::Reactivate {
                            index,
                            regenerate: full,
                            stale: !full && row.evidence_hash != component.evidence_hash,
                        });
                    }
                }
                "superseded" => {
                    let live = live_successors
                        .get(&plain_observation_id(&row.id))
                        .copied()
                        .unwrap_or(0);
                    if live == 0 {
                        if truncated {
                            // A truncated Q7 may hide the live successor.
                            actions.push(PlanAction::Skip(row.id.clone()));
                        } else {
                            actions.push(PlanAction::Reactivate {
                                index,
                                regenerate: full,
                                stale: !full && row.evidence_hash != component.evidence_hash,
                            });
                        }
                    }
                }
                _ => {}
            },
        }
    }

    for row in existing {
        if computed_hashes.contains(row.signature_hash.as_str()) {
            continue;
        }
        if row.status != "current" && row.status != "stale" {
            continue;
        }
        // Protected signatures (overflow components) and truncated windows
        // are incomplete evidence: never archive/supersede what they may not
        // have seen.
        if truncated || protected.contains(row.signature_hash.as_str()) {
            actions.push(PlanAction::Skip(row.id.clone()));
            continue;
        }
        if full {
            let mut superseded = false;
            for (index, component) in computed.iter().enumerate() {
                let shared = component
                    .members
                    .iter()
                    .filter(|member| row.members.contains(member))
                    .count();
                if shared >= 2 {
                    actions.push(PlanAction::Supersede {
                        index,
                        old: row.id.clone(),
                    });
                    superseded = true;
                }
            }
            if !superseded {
                actions.push(PlanAction::Archive(row.id.clone()));
            }
        } else {
            actions.push(PlanAction::Archive(row.id.clone()));
        }
    }
    actions
}

// ---------------------------------------------------------------------------
// §4.5 Persistence
// ---------------------------------------------------------------------------

struct ObservationWrite<'a> {
    component: &'a ComputedComponent,
    title: &'a str,
    content: &'a str,
    status: &'a str,
    generated_by: &'a str,
    embedding: Option<&'a Vec<f64>>,
    clear_incoming_supersedes: bool,
}

fn details_bindings(component: &ComputedComponent) -> Bindings {
    vec![
        (
            "signature".into(),
            Value::from_t(component.signature.as_str()),
        ),
        (
            "signature_hash".into(),
            Value::from_t(component.signature_hash.as_str()),
        ),
        (
            "anchor_signature".into(),
            Value::from_t(component.signature.as_str()),
        ),
        (
            "signatures".into(),
            string_array(component.signatures.iter().cloned()),
        ),
        (
            "evidence_hash".into(),
            Value::from_t(component.evidence_hash.as_str()),
        ),
        ("family".into(), Value::from_t(component.family.as_str())),
        (
            "members".into(),
            string_array(component.members.iter().cloned()),
        ),
        (
            "member_count".into(),
            Value::from_t(component.members.len() as i64),
        ),
        (
            "evidence_refs".into(),
            string_array(component.evidence_refs.iter().cloned()),
        ),
    ]
}

impl SearchService {
    /// Full UPSERT of the observation row (§4.5 skeleton): all `details`
    /// keys, `store='derived'`, deterministic id, edge rewrite (members +
    /// evidence refs) and the single `has_memory` edge.
    async fn persist_observation_row(
        &self,
        db: &std::sync::Arc<dyn Executor>,
        project: &str,
        write: &ObservationWrite<'_>,
    ) -> Result<()> {
        let component = write.component;
        let repo_id = repo_record_id(project);
        let observation_id = component.id.as_str();
        let mut bindings: Bindings = vec![
            ("repo_name".into(), Value::from_t(project)),
            ("title".into(), Value::from_t(write.title)),
            ("content".into(), Value::from_t(write.content)),
            ("status".into(), Value::from_t(write.status)),
            ("confidence".into(), Value::from_t(component.confidence)),
            (
                "visibility".into(),
                Value::from_t(component.visibility.as_str()),
            ),
            ("owner".into(), Value::from_t(component.owner.as_str())),
            (
                "tags".into(),
                string_array(vec![component.family.clone(), "observation".to_string()]),
            ),
            ("generated_by".into(), Value::from_t(write.generated_by)),
            (
                "embedding".into(),
                write
                    .embedding
                    .map(|vector| crate::commit_meaning::f64_vec_value(vector))
                    .unwrap_or(Value::None),
            ),
        ];
        bindings.extend(details_bindings(component));

        let mut sql = String::from("BEGIN TRANSACTION;\n");
        sql.push_str(&format!("UPSERT {repo_id} SET name = $repo_name;\n"));
        sql.push_str(&format!(
            "UPSERT {observation_id} SET kind = 'observation', title = $title, content = $content, status = $status, confidence = $confidence, visibility = $visibility, owner = $owner, tags = $tags, project = {repo_id}, store = 'derived', policy_applied = '{OBSERVATION_POLICY}', provenance.agent = '{OBSERVATION_AGENT}', provenance.tool = '{OBSERVATION_TOOL}', details.signature = $signature, details.signature_hash = $signature_hash, details.anchor_signature = $anchor_signature, details.signatures = $signatures, details.evidence_hash = $evidence_hash, details.family = $family, details.members = $members, details.member_count = $member_count, details.evidence_refs = $evidence_refs, details.rubric = '{OBSERVATION_RUBRIC}', details.generated_by = $generated_by, embedding = $embedding, valid_until = NONE, updated_at = time::now();\n"
        ));
        sql.push_str(&format!(
            "DELETE {ES_HAS_MEMORY} WHERE in = {repo_id} AND out = {observation_id};\n"
        ));
        sql.push_str(&format!(
            "RELATE {repo_id}->{ES_HAS_MEMORY}->{observation_id};\n"
        ));
        sql.push_str(&format!(
            "DELETE {ES_DERIVED_FROM} WHERE in = {observation_id};\n"
        ));
        for target in component
            .members
            .iter()
            .chain(component.evidence_refs.iter())
        {
            if let Some(target) = edge_target_identifier(target) {
                sql.push_str(&format!(
                    "RELATE {observation_id}->{ES_DERIVED_FROM}->{target};\n"
                ));
            }
        }
        if write.clear_incoming_supersedes {
            sql.push_str(&format!(
                "DELETE {ES_SUPERSEDES} WHERE out = {observation_id};\n"
            ));
        }
        sql.push_str("COMMIT TRANSACTION;");

        db.smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("observation persist: {e}")))?;
        Ok(())
    }

    /// Structural (no content/title/embedding) update + status + edge rewrite;
    /// used by `InvalidateOnly` (stale marks) and by reactivations that do
    /// not synthesize.
    async fn persist_observation_structural(
        &self,
        db: &std::sync::Arc<dyn Executor>,
        component: &ComputedComponent,
        status: &str,
        clear_incoming_supersedes: bool,
    ) -> Result<()> {
        let observation_id = component.id.as_str();
        let mut bindings: Bindings = vec![
            ("status".into(), Value::from_t(status)),
            ("confidence".into(), Value::from_t(component.confidence)),
        ];
        bindings.extend(details_bindings(component));

        let mut sql = String::from("BEGIN TRANSACTION;\n");
        sql.push_str(&format!(
            "UPDATE {observation_id} SET details.signature = $signature, details.signature_hash = $signature_hash, details.anchor_signature = $anchor_signature, details.signatures = $signatures, details.evidence_hash = $evidence_hash, details.family = $family, details.members = $members, details.member_count = $member_count, details.evidence_refs = $evidence_refs, confidence = $confidence, status = $status, valid_until = NONE, updated_at = time::now();\n"
        ));
        sql.push_str(&format!(
            "DELETE {ES_DERIVED_FROM} WHERE in = {observation_id};\n"
        ));
        for target in component
            .members
            .iter()
            .chain(component.evidence_refs.iter())
        {
            if let Some(target) = edge_target_identifier(target) {
                sql.push_str(&format!(
                    "RELATE {observation_id}->{ES_DERIVED_FROM}->{target};\n"
                ));
            }
        }
        if clear_incoming_supersedes {
            sql.push_str(&format!(
                "DELETE {ES_SUPERSEDES} WHERE out = {observation_id};\n"
            ));
        }
        sql.push_str("COMMIT TRANSACTION;");

        db.smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("observation persist: {e}")))?;
        Ok(())
    }

    async fn persist_observation_embedding(
        &self,
        db: &std::sync::Arc<dyn Executor>,
        observation_id: &str,
        embedding: &[f64],
    ) -> Result<()> {
        if !ibis_db::ids::is_safe_record_id(observation_id) {
            return Err(SearchError::other(format!(
                "unsafe observation id {observation_id}"
            )));
        }
        let sql = format!(
            "UPDATE {observation_id} SET embedding = $embedding, updated_at = time::now();"
        );
        db.smart_query(
            &sql,
            bind(vec![(
                "embedding".into(),
                crate::commit_meaning::f64_vec_value(embedding),
            )]),
        )
        .await
        .map_err(|e| SearchError::other(format!("observation embedding: {e}")))?;
        Ok(())
    }

    /// The synthesis of one component. Steps 1–4 have no LLM: the
    /// deterministic fallback (§4.8) is the produced artifact
    /// (`generated_by=fallback`); step 5 replaces it with the tier-M call.
    fn synthesize_observation(&self, component: &ComputedComponent) -> (String, String) {
        let fallback = fallback_summary(&component.anchor, &component.member_evidence);
        let content = render_observation_content(&fallback.summary, &component.member_evidence);
        (fallback.name, content)
    }

    /// Embeds at synthesis time through the existing `ai.embed_text` plumbing
    /// (`memory_embedding`); `None` means the row persists visible to list /
    /// lexical channels but invisible to `memory_search` until a later pass.
    async fn observation_embedding(&self, content: &str) -> Option<Vec<f64>> {
        self.ai.as_ref()?;
        match self.memory_embedding(content, None).await {
            Ok(vector) => Some(vector),
            Err(e) => {
                tracing::warn!("observation synthesis: embedding failed: {e}");
                None
            }
        }
    }

    /// Deterministic reconcile (§4.7): extract → signatures → hub filter →
    /// union-find → diff vs Q7 → actions per mode. `InvalidateOnly` writes
    /// only stale marks / archives / reactivations (zero LLM, zero embedding,
    /// zero CREATE); `Full` also creates and synthesizes.
    pub async fn reconcile_observations(
        &self,
        project_name: &str,
        scope: &str,
        mode: ReconcileMode,
    ) -> Result<ReconcileReport> {
        let scope = ObsScope::parse(scope)?;
        let settings = ObservationSettings::default();
        self.reconcile_observations_inner(
            project_name,
            &scope,
            mode,
            &settings.limits,
            &settings.families,
        )
        .await
    }

    /// Reconcile with the resolved `ai.observations` settings (C8 step 6):
    /// limits + family whitelist from one source.
    pub async fn reconcile_observations_with_settings(
        &self,
        project_name: &str,
        scope: &ObsScope,
        mode: ReconcileMode,
        settings: &ObservationSettings,
    ) -> Result<ReconcileReport> {
        self.reconcile_observations_inner(
            project_name,
            scope,
            mode,
            &settings.limits,
            &settings.families,
        )
        .await
    }

    async fn reconcile_observations_inner(
        &self,
        project_name: &str,
        scope: &ObsScope,
        mode: ReconcileMode,
        limits: &ObservationLimits,
        families: &[String],
    ) -> Result<ReconcileReport> {
        let db = self.require_db()?;
        let project = project_name.trim();
        if project.is_empty() {
            return Err(SearchError::other("project_name is required"));
        }
        let mut report = ReconcileReport {
            scope: scope.label(),
            ..Default::default()
        };
        if let ObsScope::Personal(owner) = scope {
            if owner.is_empty() {
                tracing::warn!(
                    "reconcile_observations: personal scope with empty owner skipped (not identifiable)"
                );
                return Ok(report);
            }
        }
        let project_key = ibis_db::ids::sanitize_id(project);

        let graph = self
            .extract_observation_graph(db, project, scope, limits)
            .await?;
        let signature_inputs = build_signature_inputs(&graph, families);
        let component_set = build_components(&signature_inputs.inputs, limits);

        for signature in &component_set.hub_excluded {
            tracing::info!(
                target: "observation_hub_excluded",
                signature = %signature.render(),
                hub_degree_max = limits.hub_degree_max,
                "observation_hub_excluded"
            );
        }
        for component in &component_set.capped {
            tracing::warn!(
                target: "observation_component_capped",
                anchor = %component.anchor_key(),
                member_count = component.member_count(),
                max_members = limits.max_members,
                "observation_component_capped"
            );
        }
        if !component_set.overflow.is_empty() {
            tracing::info!(
                "reconcile_observations: {} components over max_clusters_per_repo skipped (signatures protected)",
                component_set.overflow.len()
            );
        }
        report.hub_excluded = component_set.hub_excluded.len();
        report.capped = component_set.capped.len();
        report.overflow = component_set.overflow.len();

        // Signatures of overflow components must never be archived/superseded:
        // they were computed but not persisted, so an existing row with that
        // hash is still legitimate evidence.
        let protected: BTreeSet<String> = component_set
            .overflow
            .iter()
            .map(|component| {
                signature_hash(
                    &project_key,
                    &scope.hash_scope(),
                    &component.anchor.family,
                    &component.anchor.key,
                )
            })
            .collect();
        let truncated = graph.is_truncated();
        if truncated {
            tracing::warn!(
                target: "observation_extraction_truncated",
                q1_truncated = graph.truncated,
                graph_truncated = graph.graph_truncated,
                "observation_extraction_truncated"
            );
        }

        let computed: Vec<ComputedComponent> = component_set
            .components
            .iter()
            .map(|component| {
                compute_component(
                    &project_key,
                    scope,
                    component,
                    &signature_inputs.members,
                    &signature_inputs.refs,
                )
            })
            .collect();
        report.components = computed.len();

        let live_successors = self.load_live_successors(db, &graph.existing).await?;
        let existing_by_id: HashMap<String, &ExistingObservation> = graph
            .existing
            .iter()
            .map(|row| (plain_observation_id(&row.id), row))
            .collect();
        let actions = plan_reconcile(
            &computed,
            &graph.existing,
            mode,
            &live_successors,
            &protected,
            truncated,
        );

        for action in actions {
            match action {
                PlanAction::Create(index) => {
                    let component = &computed[index];
                    let (title, content) = self.synthesize_observation(component);
                    let embedding = self.observation_embedding(&content).await;
                    self.persist_observation_row(
                        db,
                        project,
                        &ObservationWrite {
                            component,
                            title: &title,
                            content: &content,
                            status: "current",
                            generated_by: "fallback",
                            embedding: embedding.as_ref(),
                            clear_incoming_supersedes: false,
                        },
                    )
                    .await?;
                    report.create += 1;
                }
                PlanAction::Refresh(index) => {
                    let component = &computed[index];
                    let row = existing_by_id.get(&plain_observation_id(&component.id));
                    let (title, content) = self.synthesize_observation(component);
                    let evidence_same =
                        row.is_some_and(|r| r.evidence_hash == component.evidence_hash);
                    let content_same =
                        row.is_some_and(|r| r.title == title && r.content == content);
                    let status_current = row.is_some_and(|r| r.status == "current");
                    let has_embedding = row.is_some_and(|r| r.has_embedding);
                    if evidence_same && content_same && status_current {
                        if has_embedding {
                            report.skipped += 1;
                            continue;
                        }
                        let embedding = self.observation_embedding(&content).await;
                        if let Some(embedding) = embedding {
                            self.persist_observation_embedding(db, &component.id, &embedding)
                                .await?;
                            report.refresh += 1;
                        } else {
                            report.skipped += 1;
                        }
                        continue;
                    }
                    let embedding = self.observation_embedding(&content).await;
                    self.persist_observation_row(
                        db,
                        project,
                        &ObservationWrite {
                            component,
                            title: &title,
                            content: &content,
                            status: "current",
                            generated_by: "fallback",
                            embedding: embedding.as_ref(),
                            clear_incoming_supersedes: false,
                        },
                    )
                    .await?;
                    report.refresh += 1;
                }
                PlanAction::Stale(index) => {
                    let component = &computed[index];
                    self.persist_observation_structural(db, component, "stale", false)
                        .await?;
                    report.stale += 1;
                }
                PlanAction::Reactivate {
                    index,
                    regenerate,
                    stale,
                } => {
                    let component = &computed[index];
                    if regenerate {
                        let (title, content) = self.synthesize_observation(component);
                        let embedding = self.observation_embedding(&content).await;
                        self.persist_observation_row(
                            db,
                            project,
                            &ObservationWrite {
                                component,
                                title: &title,
                                content: &content,
                                status: "current",
                                generated_by: "fallback",
                                embedding: embedding.as_ref(),
                                clear_incoming_supersedes: true,
                            },
                        )
                        .await?;
                    } else {
                        let status = if stale { "stale" } else { "current" };
                        self.persist_observation_structural(db, component, status, true)
                            .await?;
                    }
                    tracing::info!(
                        target: "observation_reactivated",
                        observation = %component.id,
                        "observation_reactivated"
                    );
                    report.refresh += 1;
                }
                PlanAction::EmbeddingRetry(index) => {
                    let component = &computed[index];
                    // An `llm` row already owns its synthesized content: embed
                    // the stored text, not the deterministic fallback. The
                    // fallback only covers a missing/empty stored content.
                    let stored = existing_by_id
                        .get(&plain_observation_id(&component.id))
                        .map(|row| row.content.trim())
                        .filter(|content| !content.is_empty())
                        .map(str::to_string);
                    let content = match stored {
                        Some(content) => content,
                        None => self.synthesize_observation(component).1,
                    };
                    let embedding = self.observation_embedding(&content).await;
                    if let Some(embedding) = embedding {
                        self.persist_observation_embedding(db, &component.id, &embedding)
                            .await?;
                        report.refresh += 1;
                    } else {
                        report.skipped += 1;
                    }
                }
                PlanAction::Skip(old) => {
                    tracing::info!(
                        target: "observation_archive_skipped",
                        observation = %old,
                        truncated,
                        "observation_archive_skipped"
                    );
                    report.skipped += 1;
                }
                PlanAction::Archive(old) => {
                    if !ibis_db::ids::is_safe_record_id(&old) {
                        return Err(SearchError::other(format!("unsafe observation id {old}")));
                    }
                    let sql = format!(
                        "UPDATE {old} SET status = 'archived', valid_until = time::now(), updated_at = time::now();"
                    );
                    db.execute(&sql)
                        .await
                        .map_err(|e| SearchError::other(format!("observation archive: {e}")))?;
                    report.archive += 1;
                }
                PlanAction::Supersede { index, old } => {
                    let new_id = &computed[index].id;
                    if !ibis_db::ids::is_safe_record_id(&old) {
                        return Err(SearchError::other(format!("unsafe observation id {old}")));
                    }
                    let sql = format!(
                        "BEGIN TRANSACTION;\nRELATE {new_id}->{ES_SUPERSEDES}->{old};\nUPDATE {old} SET status = 'superseded', valid_until = time::now(), updated_at = time::now();\nCOMMIT TRANSACTION;"
                    );
                    db.execute(&sql)
                        .await
                        .map_err(|e| SearchError::other(format!("observation supersede: {e}")))?;
                    report.supersede += 1;
                }
            }
        }

        Ok(report)
    }

    /// C8 step 6 hook: reconcile every scope of one project in `mode`
    /// (`InvalidateOnly` from the ingest hook; the synthesis pass drives
    /// `Full` per scope itself). Returns one report per scope.
    pub async fn reconcile_observations_for_project(
        &self,
        project_name: &str,
        mode: ReconcileMode,
        settings: &ObservationSettings,
    ) -> Result<Vec<ReconcileReport>> {
        let db = self.require_db()?;
        let scopes = observation_scopes_for_project(
            db.as_ref(),
            project_name,
            settings.max_scopes_per_project,
        )
        .await?;
        let mut reports = Vec::with_capacity(scopes.len());
        for scope in &scopes {
            reports.push(
                self.reconcile_observations_with_settings(project_name, scope, mode, settings)
                    .await?,
            );
        }
        Ok(reports)
    }
}

/// Discovers the reconcile scopes of one project: `team` first, then every
/// distinct personal owner holding eligible evidence (empty owners skipped),
/// capped at `max_scopes` total. Shared by the ingest hook and the synthesis
/// pass so both walk the exact same scope set.
pub async fn observation_scopes_for_project(
    db: &dyn Executor,
    project_name: &str,
    max_scopes: usize,
) -> Result<Vec<ObsScope>> {
    let project = project_name.trim();
    if project.is_empty() {
        return Err(SearchError::other(
            "observation scopes require a project name",
        ));
    }
    let repo_id = repo_record_id(project);
    let mut vars = Vars::new();
    let kinds: Vec<Value> = OBSERVATION_KINDS
        .iter()
        .map(|kind| Value::from_t(*kind))
        .collect();
    vars.insert("kinds".into(), Value::from_t(kinds));
    let query = format!(
        "SELECT owner FROM {TABLE_MEMORY} \
         WHERE project = {repo_id} AND status = 'current' AND kind IN $kinds \
         AND (store = NONE OR store != 'derived') AND visibility = 'personal' \
         GROUP BY owner;"
    );
    let outcome = db
        .smart_query(&query, vars)
        .await
        .map_err(|e| SearchError::other(format!("observation scopes: {e}")))?;
    let rows = rows::normalize_rows(&outcome)
        .ok_or_else(|| SearchError::other("observation scopes: unexpected result shape"))?;
    let mut owners: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for row in rows::object_rows(&rows) {
        let owner = rows::scalar_string(rows::field(row, "owner"))
            .trim()
            .to_string();
        // An owner-less personal scope is not identifiable (L3 edge case).
        if owner.is_empty() || !seen.insert(owner.clone()) {
            continue;
        }
        owners.push(owner);
    }
    owners.sort();
    let max_scopes = max_scopes.max(1);
    let mut scopes = vec![ObsScope::Team];
    for owner in owners {
        if scopes.len() >= max_scopes {
            tracing::warn!(
                target: "observation_scopes_truncated",
                project = project,
                cap = max_scopes,
                "observation scope cap reached; remaining personal scopes deferred"
            );
            break;
        }
        scopes.push(ObsScope::Personal(owner));
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use ibis_ai::types::{Candidate, Content, GenerationConfig};
    use ibis_ai::{AIProvider, AiError};
    use ibis_db::executor::QueryOutcome;
    use ibis_db::value::ToSql;

    use crate::memory::add::AddMemoryParams;
    use crate::memory::export::MemoryExportParams;
    use crate::memory::{
        is_derived_observation, Memory, MemoryListParams, MemorySearchParams, MemoryUpdateParams,
        STORE_DERIVED,
    };
    use crate::test_support::{fake_ai, obj, rows_of, FakeDb};

    const PROJ: &str = "obs_repo";
    const ROOT_CAUSE: &str = "empty discovery root";
    const TITLE_A: &str = "Empty discovery root crashes setup";
    const CONTENT_A: &str = "The wizard crashes when discovery_root is empty.";
    const TITLE_B: &str = "Discovery root guard missing";
    const CONTENT_B: &str = "Setup aborts when discovery_root is empty.";

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    fn fix_row(key: &str, title: &str, content: &str, root_cause: &str) -> Value {
        obj(vec![
            ("id", Value::from_t(format!("memory:{key}"))),
            ("kind", Value::from_t("fix")),
            ("title", Value::from_t(title)),
            ("content", Value::from_t(content)),
            (
                "details",
                obj(vec![("root_cause", Value::from_t(root_cause))]),
            ),
            ("root_cause", Value::from_t(root_cause)),
            ("source_path", Value::from_t("")),
            ("introduced_in", Value::from_t("")),
        ])
    }

    fn member(key: &str, title: &str, content: &str, root_cause: &str) -> EvidenceMember {
        EvidenceMember {
            id: format!("memory:{key}"),
            kind: "fix".to_string(),
            status: "current".to_string(),
            content: content.to_string(),
            title: title.to_string(),
            details: serde_json::json!({ "root_cause": root_cause }),
        }
    }

    struct TwoFix {
        a: Value,
        b: Value,
        signature_hash: String,
        id: String,
        evidence_hash: String,
    }

    fn two_fix() -> TwoFix {
        let signature = fix_root_cause_signature(ROOT_CAUSE).expect("fixture signature");
        let signature_hash = signature_hash(PROJ, "team", &signature.family, &signature.key);
        let id = observation_id(&signature_hash);
        let evidence_hash = evidence_hash(
            &[
                member("mem_a", TITLE_A, CONTENT_A, ROOT_CAUSE),
                member("mem_b", TITLE_B, CONTENT_B, ROOT_CAUSE),
            ],
            std::slice::from_ref(&signature),
            &[],
        );
        TwoFix {
            a: fix_row("mem_a", TITLE_A, CONTENT_A, ROOT_CAUSE),
            b: fix_row("mem_b", TITLE_B, CONTENT_B, ROOT_CAUSE),
            signature_hash,
            id,
            evidence_hash,
        }
    }

    #[derive(Clone)]
    struct Q7Row {
        id: String,
        status: String,
        signature_hash: String,
        evidence_hash: String,
        members: Vec<String>,
        generated_by: String,
        has_embedding: bool,
        title: String,
        content: String,
    }

    impl Default for Q7Row {
        fn default() -> Self {
            Q7Row {
                id: String::new(),
                status: "current".to_string(),
                signature_hash: String::new(),
                evidence_hash: String::new(),
                members: Vec::new(),
                generated_by: "llm".to_string(),
                has_embedding: true,
                title: String::new(),
                content: String::new(),
            }
        }
    }

    impl Q7Row {
        fn value(&self) -> Value {
            obj(vec![
                ("id", Value::from_t(self.id.clone())),
                ("status", Value::from_t(self.status.clone())),
                ("signature_hash", Value::from_t(self.signature_hash.clone())),
                ("evidence_hash", Value::from_t(self.evidence_hash.clone())),
                (
                    "members",
                    Value::from_t(
                        self.members
                            .iter()
                            .map(|member| Value::from_t(member.clone()))
                            .collect::<Vec<Value>>(),
                    ),
                ),
                ("family", Value::from_t(FAMILY_FIX_ROOT_CAUSE)),
                ("generated_by", Value::from_t(self.generated_by.clone())),
                ("has_embedding", Value::Bool(self.has_embedding)),
                ("title", Value::from_t(self.title.clone())),
                ("content", Value::from_t(self.content.clone())),
                ("confidence", Value::from_t(0.75f64)),
                ("owner", Value::from_t(OBSERVATION_AGENT)),
                ("visibility", Value::from_t("team")),
            ])
        }
    }

    fn var_str(value: &Value) -> String {
        value.as_string().cloned().unwrap_or_else(|| value.to_sql())
    }

    fn fake_db_error(fragment: &str) -> ibis_db::DbError {
        ibis_db::DbError::Query {
            message: format!("fake failure on {fragment}"),
            duration_ms: 0,
        }
    }

    /// Fake executor for the reconcile pass (ConsolidateDb pattern): routes
    /// by SQL substring, records `(sql, vars)` pairs, optionally fails one
    /// query.
    #[derive(Default)]
    struct ObservationDb {
        q1: Vec<Value>,
        q2: Vec<Value>,
        q3: Vec<Value>,
        q4: Vec<Value>,
        q5: Vec<Value>,
        q6: Vec<Value>,
        q7: Vec<Value>,
        supersedes: Vec<Value>,
        fail_fragment: Option<String>,
        calls: Mutex<Vec<(String, Vars)>>,
    }

    impl ObservationDb {
        fn with_q1(mut self, rows: Vec<Value>) -> Self {
            self.q1 = rows;
            self
        }

        fn with_q7(mut self, rows: Vec<Value>) -> Self {
            self.q7 = rows;
            self
        }

        fn with_supersedes(mut self, rows: Vec<Value>) -> Self {
            self.supersedes = rows;
            self
        }

        fn with_fail(mut self, fragment: &str) -> Self {
            self.fail_fragment = Some(fragment.to_string());
            self
        }

        fn calls_containing(&self, fragment: &str) -> Vec<(String, Vars)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(sql, _)| sql.contains(fragment))
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl Executor for ObservationDb {
        async fn execute(&self, sql: &str) -> std::result::Result<QueryOutcome, ibis_db::DbError> {
            self.calls
                .lock()
                .unwrap()
                .push((sql.to_string(), Vars::new()));
            if let Some(fragment) = &self.fail_fragment {
                if sql.contains(fragment.as_str()) {
                    return Err(fake_db_error(fragment));
                }
            }
            Ok(QueryOutcome::Single(Value::None))
        }

        async fn smart_query(
            &self,
            sql: &str,
            vars: Vars,
        ) -> std::result::Result<QueryOutcome, ibis_db::DbError> {
            self.calls.lock().unwrap().push((sql.to_string(), vars));
            if let Some(fragment) = &self.fail_fragment {
                if sql.contains(fragment.as_str()) {
                    return Err(fake_db_error(fragment));
                }
            }
            let rows = if sql.contains("kind IN $kinds") {
                self.q1.clone()
            } else if sql.contains("out.path != NONE") {
                self.q2.clone()
            } else if sql.contains("out.date != NONE") {
                self.q3.clone()
            } else if sql.contains("FROM implements") {
                self.q4.clone()
            } else if sql.contains("FROM issue") {
                self.q5.clone()
            } else if sql.contains("FROM file_chunk") {
                self.q6.clone()
            } else if sql.contains("kind = 'observation'") {
                self.q7.clone()
            } else if sql.contains("FROM supersedes") {
                self.supersedes.clone()
            } else {
                return Ok(QueryOutcome::Single(Value::None));
            };
            Ok(QueryOutcome::Single(Value::from_t(rows)))
        }
    }

    /// AI provider counting LLM vs embedding calls (the reconcile must make
    /// zero LLM calls; embedding only at synthesis) and recording every
    /// embedded text so tests can assert *what* was embedded.
    struct CountingAi {
        embed_calls: Mutex<usize>,
        generate_calls: Mutex<usize>,
        embedded_texts: Mutex<Vec<String>>,
    }

    impl Default for CountingAi {
        fn default() -> Self {
            CountingAi {
                embed_calls: Mutex::new(0),
                generate_calls: Mutex::new(0),
                embedded_texts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AIProvider for CountingAi {
        fn name(&self) -> &str {
            "counting-ai"
        }

        async fn is_functional(&self) -> bool {
            true
        }

        async fn embed_text(&self, text: &str) -> std::result::Result<Vec<f32>, AiError> {
            *self.embed_calls.lock().unwrap() += 1;
            self.embedded_texts.lock().unwrap().push(text.to_string());
            Ok(vec![0.25; 768])
        }

        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> std::result::Result<Candidate, AiError> {
            *self.generate_calls.lock().unwrap() += 1;
            Ok(Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![ibis_ai::Part {
                        text: String::new(),
                    }],
                },
                ..Default::default()
            })
        }
    }

    fn service(db: Arc<ObservationDb>, ai: Option<Arc<dyn AIProvider>>) -> SearchService {
        SearchService::new(Some(db as Arc<dyn Executor>), ai)
    }

    // ------------------------------------------------------------------
    // Unit — pure functions (§Test "Unit — funzioni pure")
    // ------------------------------------------------------------------

    #[test]
    fn normalize_key_matrix() {
        assert_eq!(
            normalize_key("  Empty  DISCOVERY   Root!! "),
            "empty discovery root"
        );
        // CamelCase survives (matching is token-level, not key-level).
        assert_eq!(normalize_key("DiscoveryRoot"), "discoveryroot");
        // Paths stay readable — no separator replacement here.
        assert_eq!(normalize_key("src/utils/helper.rs"), "src/utils/helper.rs");
        assert_eq!(normalize_key("ends with:  ;"), "ends with");
        assert_eq!(normalize_key("Café  ÉTÉ!!!"), "café été");
        assert_eq!(normalize_key("what?!,"), "what");
        // Cap 200 runes.
        let long = "x".repeat(250);
        assert_eq!(normalize_key(&long).chars().count(), 200);
        let capped = format!("{}!", "y".repeat(250));
        assert_eq!(normalize_key(&capped).chars().count(), 200);
    }

    #[test]
    fn signature_fix_root_cause() {
        let signature = fix_root_cause_signature("Empty Discovery Root.").expect("signature");
        assert_eq!(signature.family, FAMILY_FIX_ROOT_CAUSE);
        assert_eq!(signature.key, "empty discovery root");
        assert_eq!(signature.render(), "fix_root_cause|empty discovery root");
        for stop in [
            "", "none", "N/A", "unknown", "TBD", "misc", "various", " na ",
        ] {
            assert!(
                fix_root_cause_signature(stop).is_none(),
                "stoplist value {stop:?} must yield no signature"
            );
        }
    }

    #[test]
    fn signature_file_from_edge_and_from_source_path() {
        let from_edge = file_signature("src\\Wizard\\Discovery.rs").expect("edge path");
        let from_source_path = file_signature("./src/wizard/discovery.rs").expect("source path");
        assert_eq!(from_edge, from_source_path);
        assert_eq!(from_edge.key, "src/wizard/discovery.rs");
        assert!(file_signature("   ").is_none());
    }

    #[test]
    fn signature_issue_canonical_key() {
        let signature = issue_signature("GitHub", " IBIS-42 ").expect("issue signature");
        assert_eq!(signature.render(), "issue|github#IBIS-42");
        assert!(issue_signature("GitHub", "  ").is_none());
    }

    #[test]
    fn signature_symbol_token_match() {
        assert!(symbol_tokens_match(
            "The DiscoveryRoot crashes when empty",
            "DiscoveryRoot"
        ));
        // No verbatim token → no match (parts alone are not enough).
        assert!(!symbol_tokens_match(
            "the discovery root crashes",
            "DiscoveryRoot"
        ));
        // Substrings never match.
        assert!(!symbol_tokens_match(
            "The DiscoveryRootExtra crashes",
            "DiscoveryRoot"
        ));
        assert!(!symbol_tokens_match("anything", ""));
        let signature = symbol_signature("src/A.rs", "Foo").expect("symbol signature");
        assert_eq!(signature.render(), "symbol|src/a.rs#foo");
    }

    #[test]
    fn signature_hash_is_stable_and_scope_partitioned() {
        let base = signature_hash("ibis", "team", FAMILY_FIX_ROOT_CAUSE, "k");
        assert_eq!(base.len(), 32);
        assert!(base.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            base,
            signature_hash("ibis", "team", FAMILY_FIX_ROOT_CAUSE, "k")
        );
        assert_ne!(
            base,
            signature_hash("other", "team", FAMILY_FIX_ROOT_CAUSE, "k")
        );
        assert_ne!(
            base,
            signature_hash("ibis", "personal:alice", FAMILY_FIX_ROOT_CAUSE, "k")
        );
        assert_ne!(base, signature_hash("ibis", "team", FAMILY_FILE, "k"));
    }

    #[test]
    fn canonical_json_is_key_sorted() {
        let one = serde_json::json!({"b": 1, "a": {"z": true, "y": [1, "two"]}});
        let two = serde_json::json!({"a": {"y": [1, "two"], "z": true}, "b": 1});
        assert_eq!(canonical_json(&one), canonical_json(&two));
        assert_eq!(
            canonical_json(&one),
            r#"{"a":{"y":[1,"two"],"z":true},"b":1}"#
        );
    }

    #[test]
    fn evidence_hash_tracks_semantic_fields_and_ignores_input_order() {
        let signature = fix_root_cause_signature("rc").expect("signature");
        let signatures = std::slice::from_ref(&signature);
        let base = member("mem_a", "Title", "content", "rc");
        // The title feeds the fallback summary/footer, so it is semantic and
        // must invalidate the observation when edited.
        let mut retitled = base.clone();
        retitled.title = "Another title".to_string();
        assert_ne!(
            evidence_hash(std::slice::from_ref(&base), signatures, &[]),
            evidence_hash(&[retitled], signatures, &[])
        );
        // Content, status and details change the semantic evidence.
        let mut edited = base.clone();
        edited.content = "changed".to_string();
        assert_ne!(
            evidence_hash(std::slice::from_ref(&base), signatures, &[]),
            evidence_hash(&[edited], signatures, &[])
        );
        let mut restated = base.clone();
        restated.status = "stale".to_string();
        assert_ne!(
            evidence_hash(std::slice::from_ref(&base), signatures, &[]),
            evidence_hash(&[restated], signatures, &[])
        );
        let mut redetailed = base.clone();
        redetailed.details = serde_json::json!({"root_cause": "other"});
        assert_ne!(
            evidence_hash(std::slice::from_ref(&base), signatures, &[]),
            evidence_hash(&[redetailed], signatures, &[])
        );
        // Input order is irrelevant (members sorted by id inside).
        let second = member("mem_b", "Title", "content 2", "rc");
        assert_eq!(
            evidence_hash(&[base.clone(), second.clone()], signatures, &[]),
            evidence_hash(&[second, base], signatures, &[])
        );
    }

    #[test]
    fn union_find_merges_shared_memory() {
        let x = Signature::new(FAMILY_FILE, "src/a.rs");
        let y = Signature::new(FAMILY_FILE, "src/b.rs");
        let inputs = vec![
            ObservationInput {
                memory_id: "memory:mem_a".into(),
                signatures: vec![x.clone()],
            },
            ObservationInput {
                memory_id: "memory:mem_b".into(),
                signatures: vec![x.clone(), y.clone()],
            },
            ObservationInput {
                memory_id: "memory:mem_c".into(),
                signatures: vec![y.clone()],
            },
        ];
        let set = build_components(&inputs, &ObservationLimits::default());
        assert_eq!(set.components.len(), 1);
        assert_eq!(
            set.components[0].members,
            vec!["memory:mem_a", "memory:mem_b", "memory:mem_c"]
        );
        assert_eq!(set.components[0].signatures, vec![x, y]);
    }

    #[test]
    fn union_find_is_order_independent() {
        let x = Signature::new(FAMILY_FILE, "src/a.rs");
        let y = Signature::new(FAMILY_FIX_ROOT_CAUSE, "cause");
        let inputs = vec![
            ObservationInput {
                memory_id: "memory:mem_a".into(),
                signatures: vec![x.clone()],
            },
            ObservationInput {
                memory_id: "memory:mem_b".into(),
                signatures: vec![x.clone(), y.clone()],
            },
            ObservationInput {
                memory_id: "memory:mem_c".into(),
                signatures: vec![y.clone()],
            },
        ];
        let limits = ObservationLimits::default();
        let mut reversed = inputs.clone();
        reversed.reverse();
        assert_eq!(
            build_components(&inputs, &limits).components,
            build_components(&reversed, &limits).components
        );
        let mut rotated = inputs.clone();
        rotated.rotate_left(1);
        assert_eq!(
            build_components(&inputs, &limits).components,
            build_components(&rotated, &limits).components
        );
    }

    #[test]
    fn hub_signature_is_excluded() {
        let hub = Signature::new(FAMILY_FILE, "cargo.toml");
        let pair = Signature::new(FAMILY_FIX_ROOT_CAUSE, "bounded cause");
        let mut inputs: Vec<ObservationInput> = (0..11)
            .map(|i| ObservationInput {
                memory_id: format!("memory:mem_hub_{i:02}"),
                signatures: vec![hub.clone()],
            })
            .collect();
        inputs.extend((0..2).map(|i| ObservationInput {
            memory_id: format!("memory:mem_pair_{i:02}"),
            signatures: vec![pair.clone()],
        }));
        let set = build_components(&inputs, &ObservationLimits::default());
        assert_eq!(set.hub_excluded, vec![hub.clone()]);
        assert_eq!(set.components.len(), 1, "hub memories drop out entirely");
        assert_eq!(set.components[0].members.len(), 2);

        // Degree exactly at the cap is kept.
        let at_cap: Vec<ObservationInput> = (0..10)
            .map(|i| ObservationInput {
                memory_id: format!("memory:mem_cap_{i:02}"),
                signatures: vec![hub.clone()],
            })
            .collect();
        let set = build_components(&at_cap, &ObservationLimits::default());
        assert!(set.hub_excluded.is_empty());
        assert_eq!(set.components.len(), 1);
        assert_eq!(set.components[0].members.len(), 10);
    }

    #[test]
    fn singleton_is_dropped() {
        let inputs = vec![ObservationInput {
            memory_id: "memory:mem_alone".into(),
            signatures: vec![Signature::new(FAMILY_FIX_ROOT_CAUSE, "lonely")],
        }];
        let set = build_components(&inputs, &ObservationLimits::default());
        assert!(set.components.is_empty());
        assert_eq!(set.singletons, 1);
    }

    #[test]
    fn oversized_component_is_dropped_not_truncated() {
        // A 13-member component whose signatures each stay at/below the hub
        // degree (a chain): members 0-9 share `chain`, members 9-12 share
        // `tail`.
        let chain = Signature::new(FAMILY_FIX_ROOT_CAUSE, "chain");
        let tail = Signature::new(FAMILY_FILE, "src/tail.rs");
        let inputs: Vec<ObservationInput> = (0..13)
            .map(|i| {
                let mut signatures = Vec::new();
                if i <= 9 {
                    signatures.push(chain.clone());
                }
                if i >= 9 {
                    signatures.push(tail.clone());
                }
                ObservationInput {
                    memory_id: format!("memory:mem_wide_{i:02}"),
                    signatures,
                }
            })
            .collect();
        let set = build_components(&inputs, &ObservationLimits::default());
        assert!(
            set.hub_excluded.is_empty(),
            "each signature stays within the hub cap"
        );
        assert!(
            set.components.is_empty(),
            "oversized components are dropped"
        );
        assert_eq!(set.capped.len(), 1);
        assert_eq!(set.capped[0].member_count(), 13);
    }

    #[test]
    fn file_record_values_preserve_raw_records_and_dedup() {
        let raw = Value::RecordId(RecordId::new(
            "source_file",
            RecordIdKey::String("src/a.rs".to_string()),
        ));
        // The decoded string of an array-key id is NOT round-trippable through
        // `record_id_value`; only the raw value matches `file IN $files`.
        let array_key_id = "source_file:[repo:⟨obs_repo⟩, 'src/a.rs']".to_string();
        let edges = vec![
            ObsFileEdge {
                memory_id: "memory:mem_a".into(),
                file_id: array_key_id.clone(),
                file_record: raw.clone(),
                rel_path: "src/a.rs".into(),
            },
            // Same decoded id from a second memory: bound exactly once.
            ObsFileEdge {
                memory_id: "memory:mem_b".into(),
                file_id: array_key_id,
                file_record: raw.clone(),
                rel_path: "src/a.rs".into(),
            },
            // Fake-executor shape: plain string id, no raw record value.
            ObsFileEdge {
                memory_id: "memory:mem_c".into(),
                file_id: "source_file:src/b.rs".into(),
                file_record: Value::None,
                rel_path: "src/b.rs".into(),
            },
        ];
        let bound = file_record_values(&edges);
        let Value::Array(items) = bound else {
            panic!("$files must bind as an array");
        };
        assert_eq!(items.len(), 2, "dedup by decoded string id");
        assert!(
            items.contains(&raw),
            "the raw array-key record value must be preserved: {items:?}"
        );
        assert!(
            items.contains(&Value::RecordId(RecordId::new(
                "source_file",
                RecordIdKey::String("src/b.rs".to_string())
            ))),
            "plain string ids still bind as records: {items:?}"
        );
    }

    #[test]
    fn edge_target_identifier_quotes_expression_prone_keys() {
        // Hyphenated keys parse as subtraction if interpolated bare.
        assert_eq!(
            edge_target_identifier("issue:IBIS-42").as_deref(),
            Some("issue:⟨IBIS-42⟩")
        );
        assert_eq!(
            edge_target_identifier("commit:abc-def").as_deref(),
            Some("commit:⟨abc-def⟩")
        );
        assert_eq!(
            edge_target_identifier("memory:mem-a").as_deref(),
            Some("memory:⟨mem-a⟩")
        );
        // Bare identifiers are valid SurrealQL record keys already.
        assert_eq!(
            edge_target_identifier("memory:mem_a").as_deref(),
            Some("memory:mem_a")
        );
        // Array keys must never be re-wrapped as a single string key.
        let array_key = "source_file:[repo:⟨obs_repo⟩, 'src/a-b.rs']";
        assert_eq!(
            edge_target_identifier(array_key).as_deref(),
            Some(array_key)
        );
        // Already-wrapped keys stay unchanged (no double wrapping).
        assert_eq!(
            edge_target_identifier("memory:⟨obs_abc⟩").as_deref(),
            Some("memory:⟨obs_abc⟩")
        );
        assert_eq!(
            edge_target_identifier("memory:`obs_abc`").as_deref(),
            Some("memory:`obs_abc`")
        );
        // Numeric issue keys stay numeric.
        assert_eq!(
            edge_target_identifier("issue:42").as_deref(),
            Some("issue:42")
        );
        // Unsafe ids are skipped, never interpolated.
        assert!(edge_target_identifier("memory:bad id").is_none());
        assert!(edge_target_identifier("not-a-record").is_none());
    }

    #[test]
    fn overflow_components_are_returned_not_just_counted() {
        let inputs: Vec<ObservationInput> = (0..3)
            .flat_map(|cluster| {
                (0..2).map(move |member| ObservationInput {
                    memory_id: format!("memory:mem_{cluster}_{member}"),
                    signatures: vec![Signature::new(
                        FAMILY_FIX_ROOT_CAUSE,
                        format!("cause {cluster}"),
                    )],
                })
            })
            .collect();
        let limits = ObservationLimits {
            max_clusters_per_repo: 2,
            ..ObservationLimits::default()
        };
        let set = build_components(&inputs, &limits);
        assert_eq!(set.components.len(), 2);
        assert_eq!(set.overflow.len(), 1);
        assert_eq!(set.overflow[0].anchor.key, "cause 2");
    }

    #[test]
    fn anchor_priority_is_fix_then_issue_then_symbol_then_file() {
        assert_eq!(family_priority(FAMILY_FIX_ROOT_CAUSE), 0);
        assert_eq!(family_priority(FAMILY_ISSUE), 1);
        assert_eq!(family_priority(FAMILY_SYMBOL), 2);
        assert_eq!(family_priority(FAMILY_FILE), 3);

        let fix = fix_root_cause_signature("cause").expect("fix");
        let issue = issue_signature("prov", "KEY").expect("issue");
        let symbol = symbol_signature("src/a.rs", "Foo").expect("symbol");
        let file = file_signature("src/a.rs").expect("file");
        let limits = ObservationLimits::default();
        let component_for = |signatures: Vec<Signature>| {
            let inputs = vec![
                ObservationInput {
                    memory_id: "memory:mem_a".into(),
                    signatures: signatures.clone(),
                },
                ObservationInput {
                    memory_id: "memory:mem_b".into(),
                    signatures,
                },
            ];
            build_components(&inputs, &limits)
                .components
                .remove(0)
                .anchor
        };
        assert_eq!(
            component_for(vec![
                file.clone(),
                symbol.clone(),
                issue.clone(),
                fix.clone()
            ]),
            fix
        );
        assert_eq!(
            component_for(vec![file.clone(), symbol.clone(), issue.clone()]),
            issue
        );
        assert_eq!(component_for(vec![file.clone(), symbol.clone()]), symbol);
        assert_eq!(component_for(vec![file.clone()]), file);
    }

    #[test]
    fn observation_id_is_deterministic() {
        assert_eq!(
            observation_id("0123456789abcdef0123456789abcdef"),
            "memory:⟨obs_0123456789abcdef0123456789abcdef⟩"
        );
        assert_eq!(observation_id("abc"), observation_id("abc"));
    }

    #[test]
    fn confidence_formula() {
        assert_eq!(observation_confidence(2), 0.75);
        assert_eq!(observation_confidence(3), 0.83);
        assert_eq!(observation_confidence(6), 0.92);
        assert_eq!(observation_confidence(12), 0.95);
    }

    #[test]
    fn fallback_summary_is_deterministic() {
        let anchor = fix_root_cause_signature(ROOT_CAUSE).expect("anchor");
        let members = vec![
            member("mem_a", TITLE_A, CONTENT_A, ROOT_CAUSE),
            member("mem_b", TITLE_B, CONTENT_B, ROOT_CAUSE),
        ];
        let first = fallback_summary(&anchor, &members);
        assert_eq!(first, fallback_summary(&anchor, &members));
        assert_eq!(first.name, "Fix root cause: empty discovery root");
        assert!(first
            .summary
            .starts_with("2 memories share fix_root_cause|empty discovery root."));
        assert!(first.summary.contains(TITLE_A));
        assert!(first.summary.contains(TITLE_B));

        // An untitled member falls back to its first content line.
        let untitled = EvidenceMember {
            id: "memory:mem_c".into(),
            kind: "fix".into(),
            status: "current".into(),
            content: "First line of the body\nmore".into(),
            title: String::new(),
            details: serde_json::Value::Null,
        };
        let summary = fallback_summary(&anchor, &[untitled]);
        assert!(
            summary.summary.contains("First line of the body"),
            "{summary:?}"
        );

        let content = render_observation_content(&first.summary, &members);
        assert!(content.contains("\n\nMembers:\n- fix: "));
        assert!(content.contains(TITLE_A));
        assert!(content.contains(TITLE_B));
    }

    #[test]
    fn summary_validation_rejects_foreign_ids() {
        let members = vec!["memory:⟨mem_a⟩".to_string()];
        let foreign = "The pattern links memory:⟨mem_a⟩ and memory:⟨altro⟩.";
        assert!(validate_observation_summary("Name", foreign, &members, &[]).is_none());
        let ok = "The pattern links memory:⟨mem_a⟩ only.";
        let validated =
            validate_observation_summary("Name", ok, &members, &[]).expect("allowed mention");
        assert_eq!(validated.summary, ok);
        // The unwrapped form still matches the wrapped member id.
        assert!(validate_observation_summary("Name", "see memory:mem_a", &members, &[]).is_some());
        // A truncated id is not a mention.
        assert!(validate_observation_summary("Name", "see memory:mem_", &members, &[]).is_none());
        // Evidence refs count as allowed ids too.
        let refs = vec!["commit:abc".to_string()];
        assert!(validate_observation_summary("Name", "commit:abc", &members, &refs).is_some());
        assert!(validate_observation_summary("Name", "commit:def", &members, &refs).is_none());
    }

    #[test]
    fn summary_validation_truncates_long_text() {
        let long = "x".repeat(1500);
        let validated =
            validate_observation_summary("Name", &long, &[], &[]).expect("no ids, accepted");
        assert_eq!(validated.summary.chars().count(), 1200);
        let long_name = "n".repeat(100);
        let validated = validate_observation_summary(&long_name, "body", &[], &[]).expect("name");
        assert_eq!(validated.name.chars().count(), 80);
        assert!(validate_observation_summary("   ", "body", &[], &[]).is_none());
        assert!(validate_observation_summary("name", "   ", &[], &[]).is_none());
        // Fences are stripped before validation.
        let validated =
            validate_observation_summary("```json\nName\n```", "```\nBody\n```", &[], &[])
                .expect("fenced");
        assert_eq!(validated.name, "Name");
        assert_eq!(validated.summary, "Body");
    }

    #[test]
    fn read_only_predicate() {
        let derived = Memory {
            kind: "observation".into(),
            store: STORE_DERIVED.into(),
            ..Default::default()
        };
        assert!(is_derived_observation(&derived));
        let mut plain_derived = derived.clone();
        plain_derived.kind = "fix".into();
        assert!(!is_derived_observation(&plain_derived));
        let mut raw_observation = derived.clone();
        raw_observation.store = "surreal".into();
        assert!(!is_derived_observation(&raw_observation));
    }

    #[test]
    fn observation_details_rubric_is_the_c8_set() {
        assert_eq!(
            crate::memory::details::required_details_keys("observation"),
            &["signature_hash", "evidence_hash", "members"]
        );
    }

    // ------------------------------------------------------------------
    // Unit — reconcile with the fake executor (§Test step 3/4)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn reconcile_marks_stale_only_when_hash_changes() {
        let fixture = two_fix();
        let members = vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()];

        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_q7(vec![Q7Row {
                    id: fixture.id.clone(),
                    signature_hash: fixture.signature_hash.clone(),
                    evidence_hash: fixture.evidence_hash.clone(),
                    members: members.clone(),
                    ..Default::default()
                }
                .value()]),
        );
        let ai = Arc::new(CountingAi::default());
        let svc = service(db.clone(), Some(ai.clone()));
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("unchanged reconcile");
        assert_eq!(report.components, 1);
        assert_eq!(report.stale, 0);
        assert_eq!(report.create, 0);
        assert!(
            db.calls_containing("UPDATE memory:⟨obs_").is_empty(),
            "evidence unchanged must write nothing"
        );
        assert!(db.calls_containing("UPSERT memory:⟨obs_").is_empty());
        assert_eq!(*ai.generate_calls.lock().unwrap(), 0);
        assert_eq!(*ai.embed_calls.lock().unwrap(), 0);

        let changed = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_q7(vec![Q7Row {
                    id: fixture.id.clone(),
                    signature_hash: fixture.signature_hash.clone(),
                    evidence_hash: "00000000000000000000000000000000".to_string(),
                    members: members.clone(),
                    ..Default::default()
                }
                .value()]),
        );
        let ai = Arc::new(CountingAi::default());
        let svc = service(changed.clone(), Some(ai.clone()));
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("changed reconcile");
        assert_eq!(report.stale, 1);
        let updates = changed.calls_containing("UPDATE memory:⟨obs_");
        assert_eq!(updates.len(), 1, "one stale mark");
        assert!(updates[0].0.contains("status = $status"));
        assert!(
            updates[0].0.contains("valid_until = NONE"),
            "structural updates must clear a past valid_until: {}",
            updates[0].0
        );
        assert!(updates[0].0.contains("confidence = $confidence"));
        assert_eq!(
            crate::rows::numeric_float(updates[0].1.get("confidence").expect("confidence binding")),
            0.75
        );
        assert_eq!(
            var_str(updates[0].1.get("status").expect("status binding")),
            "stale"
        );
        assert_eq!(*ai.generate_calls.lock().unwrap(), 0);
        assert_eq!(*ai.embed_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn reconcile_creates_fallback_row_without_ai() {
        let fixture = two_fix();
        let db =
            Arc::new(ObservationDb::default().with_q1(vec![fixture.a.clone(), fixture.b.clone()]));
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::Full)
            .await
            .expect("full reconcile");
        assert_eq!(report.components, 1);
        assert_eq!(report.create, 1);
        let creates = db.calls_containing("UPSERT memory:⟨obs_");
        assert_eq!(creates.len(), 1, "one deterministic create");
        let (sql, vars) = &creates[0];
        assert!(sql.contains(&format!("UPSERT {}", fixture.id)));
        assert!(sql.contains("store = 'derived'"));
        assert!(sql.contains("policy_applied = 'observation.v1'"));
        assert!(sql.contains("details.rubric = 'observation.v1'"));
        assert!(sql.contains("provenance.agent = 'ibis.observation_engine'"));
        assert!(sql.contains("provenance.tool = 'memory_observations'"));
        assert!(sql.contains("valid_until = NONE"));
        assert!(sql.contains(&format!(
            "RELATE repo:⟨obs_repo⟩->has_memory->{}",
            fixture.id
        )));
        assert!(sql.contains(&format!(
            "RELATE {0}->derived_from->memory:mem_a",
            fixture.id
        )));
        assert!(sql.contains(&format!(
            "RELATE {0}->derived_from->memory:mem_b",
            fixture.id
        )));
        assert_eq!(
            var_str(vars.get("generated_by").expect("generated_by")),
            "fallback"
        );
        assert_eq!(var_str(vars.get("status").expect("status")), "current");
        assert_eq!(
            var_str(vars.get("member_count").expect("member_count")),
            "2"
        );
        assert_eq!(
            var_str(vars.get("evidence_hash").expect("evidence_hash")),
            fixture.evidence_hash
        );
        assert_eq!(var_str(vars.get("visibility").expect("visibility")), "team");
        assert_eq!(
            var_str(vars.get("owner").expect("owner")),
            OBSERVATION_AGENT
        );
        assert_eq!(
            var_str(vars.get("embedding").expect("embedding")),
            "NONE",
            "no AI provider → embedding NONE"
        );
        // The InvalidateOnly mode would have written nothing.
        let dry =
            Arc::new(ObservationDb::default().with_q1(vec![fixture.a.clone(), fixture.b.clone()]));
        let svc = service(dry.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("dry reconcile");
        assert_eq!(report.create, 0);
        assert!(
            dry.calls_containing("UPSERT memory:⟨obs_").is_empty(),
            "InvalidateOnly never CREATEs"
        );
    }

    #[tokio::test]
    async fn reconcile_archives_vanished_signature() {
        let fixture = two_fix();
        let db = Arc::new(ObservationDb::default().with_q7(vec![Q7Row {
                id: fixture.id.clone(),
                signature_hash: fixture.signature_hash.clone(),
                evidence_hash: fixture.evidence_hash.clone(),
                members: vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()],
                ..Default::default()
            }
            .value()]));
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("vanished reconcile");
        assert_eq!(report.components, 0);
        assert_eq!(report.archive, 1);
        let archives = db.calls_containing("UPDATE memory:⟨obs_");
        assert_eq!(archives.len(), 1);
        assert!(archives[0].0.contains("status = 'archived'"));
        assert!(archives[0].0.contains("valid_until = time::now()"));
    }

    #[tokio::test]
    async fn overflow_component_existing_row_is_not_archived() {
        // 26 two-member clusters: 25 persist, one overflows the cap.
        let mut rows: Vec<Value> = Vec::new();
        for cluster in 0..26 {
            let cause = format!("cause {cluster:02}");
            for member in ["a", "b"] {
                rows.push(fix_row(
                    &format!("mem_{cluster:02}_{member}"),
                    &format!("title {cluster:02} {member}"),
                    "shared content",
                    &cause,
                ));
            }
        }
        let overflow_hash = signature_hash(PROJ, "team", FAMILY_FIX_ROOT_CAUSE, "cause 25");
        let overflow_id = observation_id(&overflow_hash);
        let db = Arc::new(ObservationDb::default().with_q1(rows).with_q7(vec![Q7Row {
                id: overflow_id.clone(),
                signature_hash: overflow_hash,
                evidence_hash: "overflow-evidence".to_string(),
                members: vec![
                    "memory:mem_25_a".to_string(),
                    "memory:mem_25_b".to_string(),
                ],
                ..Default::default()
            }
            .value()]));
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("overflow reconcile");
        assert_eq!(report.components, 25);
        assert_eq!(report.overflow, 1);
        assert_eq!(report.archive, 0, "overflow rows are not vanished evidence");
        assert_eq!(report.skipped, 1);
        assert!(
            db.calls_containing(&format!("UPDATE {overflow_id}"))
                .is_empty(),
            "the overflow component's existing row must never be archived"
        );
    }

    #[tokio::test]
    async fn truncated_extraction_does_not_archive_existing_row() {
        // 501 decodable memories > the 500-row window: extraction is
        // truncated, so the pass may not conclude that any row vanished.
        let rows: Vec<Value> = (0..=500)
            .map(|index| {
                fix_row(
                    &format!("mem_{index:03}"),
                    &format!("title {index:03}"),
                    "shared content",
                    "shared cause",
                )
            })
            .collect();
        let vanished_hash = signature_hash(PROJ, "team", FAMILY_FIX_ROOT_CAUSE, "shared cause");
        let vanished_id = observation_id(&vanished_hash);
        let db = Arc::new(ObservationDb::default().with_q1(rows).with_q7(vec![Q7Row {
                    id: vanished_id.clone(),
                    signature_hash: vanished_hash,
                    evidence_hash: "old-evidence".to_string(),
                    members: vec!["memory:mem_000".to_string(), "memory:mem_001".to_string()],
                    ..Default::default()
                }
                .value()]));
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("truncated reconcile");
        assert_eq!(report.archive, 0, "truncated windows never archive");
        assert_eq!(report.skipped, 1);
        assert!(db.calls_containing("status = 'archived'").is_empty());
        assert!(db
            .calls_containing(&format!("UPDATE {vanished_id}"))
            .is_empty());
    }

    #[tokio::test]
    async fn truncated_extraction_does_not_reactivate_archived_row() {
        // 501 rows: a two-member pair plus 499 singletons. Q1 truncates, but
        // the pair still computes; the archived row matching it must not be
        // reactivated from a partial window (that could duplicate a current
        // observation).
        let mut rows: Vec<Value> = vec![
            fix_row("mem_pair_a", "pair a", "shared content", "pair cause"),
            fix_row("mem_pair_b", "pair b", "shared content", "pair cause"),
        ];
        for index in 0..499 {
            rows.push(fix_row(
                &format!("mem_single_{index:03}"),
                &format!("single {index:03}"),
                "single content",
                &format!("single cause {index:03}"),
            ));
        }
        let pair_hash = signature_hash(PROJ, "team", FAMILY_FIX_ROOT_CAUSE, "pair cause");
        let archived_id = observation_id(&pair_hash);
        let db = Arc::new(ObservationDb::default().with_q1(rows).with_q7(vec![Q7Row {
                    id: archived_id.clone(),
                    status: "archived".to_string(),
                    signature_hash: pair_hash,
                    evidence_hash: "archived-evidence".to_string(),
                    members: vec![
                        "memory:mem_pair_a".to_string(),
                        "memory:mem_pair_b".to_string(),
                    ],
                    ..Default::default()
                }
                .value()]));
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("truncated reactivation reconcile");
        assert_eq!(report.components, 1);
        assert_eq!(report.refresh, 0, "no reactivation from a truncated window");
        assert_eq!(report.skipped, 1);
        assert!(
            db.calls_containing(&format!("UPDATE {archived_id}"))
                .is_empty(),
            "the archived row must stay untouched"
        );
        assert!(db
            .calls_containing("DELETE supersedes WHERE out = ")
            .is_empty());
    }

    #[tokio::test]
    async fn embedding_retry_embeds_the_stored_llm_content() {
        let fixture = two_fix();
        let stored = "Stored LLM synthesis about the discovery root.";
        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_q7(vec![Q7Row {
                    id: fixture.id.clone(),
                    signature_hash: fixture.signature_hash.clone(),
                    evidence_hash: fixture.evidence_hash.clone(),
                    members: vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()],
                    generated_by: "llm".to_string(),
                    has_embedding: false,
                    title: "Stored title".to_string(),
                    content: stored.to_string(),
                    ..Default::default()
                }
                .value()]),
        );
        let ai = Arc::new(CountingAi::default());
        let svc = service(db.clone(), Some(ai.clone()));
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::Full)
            .await
            .expect("embedding retry reconcile");
        assert_eq!(report.refresh, 1);
        assert_eq!(
            ai.embedded_texts.lock().unwrap().as_slice(),
            [stored.to_string()],
            "an llm row must embed its stored content, not the fallback"
        );
        assert_eq!(
            db.calls_containing("SET embedding = $embedding").len(),
            1,
            "embedding retry updates the embedding only"
        );
        assert!(db.calls_containing("UPSERT memory:⟨obs_").is_empty());
    }

    #[tokio::test]
    async fn split_supersedes_with_shared_core() {
        let a = fix_row("mem_a", TITLE_A, CONTENT_A, "alpha root");
        let b = fix_row("mem_b", TITLE_B, CONTENT_B, "alpha root");
        let c = fix_row("mem_c", "C title", "C content", "beta root");
        let d = fix_row("mem_d", "D title", "D content", "beta root");
        let old_hash = signature_hash(PROJ, "team", FAMILY_FIX_ROOT_CAUSE, "combined pattern");
        let old_id = observation_id(&old_hash);
        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![a, b, c, d])
                .with_q7(vec![Q7Row {
                    id: old_id.clone(),
                    signature_hash: old_hash,
                    evidence_hash: "old".to_string(),
                    members: ["mem_a", "mem_b", "mem_c", "mem_d"]
                        .iter()
                        .map(|key| format!("memory:{key}"))
                        .collect(),
                    ..Default::default()
                }
                .value()]),
        );
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::Full)
            .await
            .expect("split reconcile");
        assert_eq!(report.components, 2);
        assert_eq!(report.create, 2);
        assert_eq!(report.supersede, 2, "one incoming edge per new component");

        let alpha_id = observation_id(&signature_hash(
            PROJ,
            "team",
            FAMILY_FIX_ROOT_CAUSE,
            "alpha root",
        ));
        let beta_id = observation_id(&signature_hash(
            PROJ,
            "team",
            FAMILY_FIX_ROOT_CAUSE,
            "beta root",
        ));
        let supersedes = db.calls_containing("->supersedes->");
        assert_eq!(supersedes.len(), 2);
        for (sql, _) in &supersedes {
            assert!(
                sql.contains(&format!("UPDATE {old_id} SET status = 'superseded'")),
                "{sql}"
            );
            assert!(!sql.contains("invalidated_at_oid"), "{sql}");
        }
        assert!(supersedes
            .iter()
            .any(|(sql, _)| sql.contains(&format!("RELATE {alpha_id}->supersedes->{old_id}"))));
        assert!(supersedes
            .iter()
            .any(|(sql, _)| sql.contains(&format!("RELATE {beta_id}->supersedes->{old_id}"))));
    }

    #[tokio::test]
    async fn merge_supersedes_both_olds() {
        let rows: Vec<Value> = ["mem_a", "mem_b", "mem_c", "mem_d"]
            .iter()
            .map(|key| fix_row(key, key, "merged content", "merged pattern"))
            .collect();
        let old_one_hash = signature_hash(PROJ, "team", FAMILY_FIX_ROOT_CAUSE, "group one");
        let old_two_hash = signature_hash(PROJ, "team", FAMILY_FIX_ROOT_CAUSE, "group two");
        let old_one = observation_id(&old_one_hash);
        let old_two = observation_id(&old_two_hash);
        let db = Arc::new(ObservationDb::default().with_q1(rows).with_q7(vec![
                    Q7Row {
                        id: old_one.clone(),
                        signature_hash: old_one_hash,
                        evidence_hash: "old-one".to_string(),
                        members: vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()],
                        ..Default::default()
                    }
                    .value(),
                    Q7Row {
                        id: old_two.clone(),
                        signature_hash: old_two_hash,
                        evidence_hash: "old-two".to_string(),
                        members: vec!["memory:mem_c".to_string(), "memory:mem_d".to_string()],
                        ..Default::default()
                    }
                    .value(),
                ]));
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::Full)
            .await
            .expect("merge reconcile");
        assert_eq!(report.create, 1);
        assert_eq!(report.supersede, 2, "the new row supersedes both olds");
        let new_id = observation_id(&signature_hash(
            PROJ,
            "team",
            FAMILY_FIX_ROOT_CAUSE,
            "merged pattern",
        ));
        let supersedes = db.calls_containing("->supersedes->");
        assert!(supersedes
            .iter()
            .any(|(sql, _)| sql.contains(&format!("RELATE {new_id}->supersedes->{old_one}"))));
        assert!(supersedes
            .iter()
            .any(|(sql, _)| sql.contains(&format!("RELATE {new_id}->supersedes->{old_two}"))));
    }

    #[tokio::test]
    async fn reactivation_clears_incoming_supersedes() {
        let fixture = two_fix();
        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_q7(vec![Q7Row {
                    id: fixture.id.clone(),
                    status: "archived".to_string(),
                    signature_hash: fixture.signature_hash.clone(),
                    evidence_hash: fixture.evidence_hash.clone(),
                    members: vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()],
                    ..Default::default()
                }
                .value()]),
        );
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("reactivation reconcile");
        assert_eq!(report.refresh, 1);
        let reactivated = db.calls_containing("DELETE supersedes WHERE out = ");
        assert_eq!(reactivated.len(), 1);
        let (sql, vars) = &reactivated[0];
        assert!(sql.contains(&format!("DELETE supersedes WHERE out = {}", fixture.id)));
        assert!(
            sql.contains("valid_until = NONE"),
            "reactivation must clear the archived valid_until: {sql}"
        );
        assert!(sql.contains("confidence = $confidence"), "{sql}");
        assert_eq!(
            crate::rows::numeric_float(vars.get("confidence").expect("confidence binding")),
            0.75
        );
        assert_eq!(
            var_str(vars.get("status").expect("status binding")),
            "current"
        );
    }

    #[tokio::test]
    async fn superseded_without_live_successor_reactivates() {
        let fixture = two_fix();
        let successor_id = "memory:⟨obs_successor⟩";
        let supersedes_edges = vec![obj(vec![
            ("successor_id", Value::from_t(successor_id)),
            ("old_id", Value::from_t(fixture.id.clone())),
        ])];
        // The edge exists but the successor is not current → reactivation.
        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_supersedes(supersedes_edges.clone())
                .with_q7(vec![
                    Q7Row {
                        id: fixture.id.clone(),
                        status: "superseded".to_string(),
                        signature_hash: fixture.signature_hash.clone(),
                        evidence_hash: fixture.evidence_hash.clone(),
                        members: vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()],
                        ..Default::default()
                    }
                    .value(),
                    Q7Row {
                        id: successor_id.to_string(),
                        status: "archived".to_string(),
                        signature_hash: "successorhash".to_string(),
                        ..Default::default()
                    }
                    .value(),
                ]),
        );
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("superseded reconcile");
        assert_eq!(report.refresh, 1);
        assert!(db
            .calls_containing("DELETE supersedes WHERE out = ")
            .iter()
            .any(|(sql, _)| sql.contains(&fixture.id)));

        // With a live successor the row is a no-op.
        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_supersedes(supersedes_edges)
                .with_q7(vec![
                    Q7Row {
                        id: fixture.id.clone(),
                        status: "superseded".to_string(),
                        signature_hash: fixture.signature_hash.clone(),
                        evidence_hash: fixture.evidence_hash.clone(),
                        members: vec!["memory:mem_a".to_string(), "memory:mem_b".to_string()],
                        ..Default::default()
                    }
                    .value(),
                    Q7Row {
                        id: successor_id.to_string(),
                        status: "current".to_string(),
                        signature_hash: "successorhash".to_string(),
                        ..Default::default()
                    }
                    .value(),
                ]),
        );
        let svc = service(db.clone(), None);
        let report = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::InvalidateOnly)
            .await
            .expect("live successor reconcile");
        assert_eq!(report.refresh, 0);
        // The superseded row is untouched (the successor stays the truth);
        // the fixture's successor row itself is out of scope of this fixture
        // and may be archived as its own vanished signature.
        assert!(
            db.calls_containing(&fixture.id)
                .iter()
                .all(|(sql, _)| !sql.contains(&format!("UPDATE {}", fixture.id))),
            "the superseded row must not be reactivated while the successor is current"
        );
    }

    #[tokio::test]
    async fn upsert_does_not_duplicate_has_memory() {
        let fixture = two_fix();
        let db =
            Arc::new(ObservationDb::default().with_q1(vec![fixture.a.clone(), fixture.b.clone()]));
        let svc = service(db.clone(), None);
        for _pass in 0..3 {
            svc.reconcile_observations(PROJ, "team", ReconcileMode::Full)
                .await
                .expect("full pass");
        }
        let creates = db.calls_containing("UPSERT memory:⟨obs_");
        assert_eq!(creates.len(), 3);
        let relates = db.calls_containing("->has_memory->");
        assert_eq!(relates.len(), 3);
        for (sql, _) in &relates {
            assert_eq!(
                sql.matches("->has_memory->").count(),
                1,
                "one has_memory edge per transaction"
            );
            assert!(
                sql.contains("DELETE has_memory WHERE"),
                "delete+relate keeps the edge single"
            );
        }
    }

    #[tokio::test]
    async fn reconcile_aborts_on_query_failure() {
        let fixture = two_fix();
        let db = Arc::new(
            ObservationDb::default()
                .with_q1(vec![fixture.a.clone(), fixture.b.clone()])
                .with_fail("kind IN $kinds"),
        );
        let svc = service(db.clone(), None);
        let err = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::Full)
            .await
            .expect_err("extraction failure aborts");
        assert!(err.to_string().contains("observation extract q1"), "{err}");
        assert!(
            db.calls_containing("UPSERT memory:⟨obs_").is_empty(),
            "no partial extraction is interpreted as evidence"
        );
    }

    #[tokio::test]
    async fn reconcile_requires_a_connected_database() {
        let svc = SearchService::new(None, None);
        let err = svc
            .reconcile_observations(PROJ, "team", ReconcileMode::Full)
            .await
            .expect_err("degraded db");
        assert!(err.to_string().contains("database not connected"), "{err}");
    }

    // ------------------------------------------------------------------
    // Unit — read-only guards + read surface
    // ------------------------------------------------------------------

    fn derived_row() -> Value {
        obj(vec![
            ("id", Value::from_t("memory:mem_obs")),
            ("kind", Value::from_t("observation")),
            ("content", Value::from_t("summary")),
            ("status", Value::from_t("current")),
            ("visibility", Value::from_t("team")),
            ("owner", Value::from_t(OBSERVATION_AGENT)),
            ("store", Value::from_t(STORE_DERIVED)),
        ])
    }

    fn derived_service() -> (Arc<FakeDb>, SearchService) {
        let fake = Arc::new(FakeDb::new(vec![(
            "SELECT * FROM memory:mem_obs",
            rows_of(vec![derived_row()]),
        )]));
        (
            fake.clone(),
            SearchService::new(Some(fake), Some(fake_ai())),
        )
    }

    #[tokio::test]
    async fn update_memory_rejects_derived() {
        let (_fake, svc) = derived_service();
        let err = svc
            .update_memory(
                &crate::ActorContext::LOCAL,
                MemoryUpdateParams {
                    id: "memory:mem_obs".into(),
                    content: Some("edit".into()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("derived observation is read-only");
        assert!(
            err.to_string().contains("derived from its evidence"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn delete_memory_rejects_derived() {
        let (_fake, svc) = derived_service();
        let err = svc
            .delete_memory(&crate::ActorContext::LOCAL, "memory:mem_obs", "")
            .await
            .expect_err("derived observation cannot be archived");
        assert!(err.to_string().contains("cannot be archived"), "{err}");
    }

    #[tokio::test]
    async fn share_rejects_derived() {
        let (_fake, svc) = derived_service();
        let err = svc
            .share_memory(&crate::ActorContext::LOCAL, "memory:mem_obs", "")
            .await
            .expect_err("derived observation cannot be shared");
        assert!(err.to_string().contains("cannot be shared"), "{err}");
    }

    #[tokio::test]
    async fn supersede_rejects_derived() {
        let (_fake, svc) = derived_service();
        let mut params = AddMemoryParams {
            project_name: "obs_repo".into(),
            content: "replacement".into(),
            supersedes: "memory:mem_obs".into(),
            ..Default::default()
        };
        let err = svc
            .add_memory(&crate::ActorContext::LOCAL, &mut params)
            .await
            .expect_err("derived observation supersession is engine-only");
        assert!(
            err.to_string()
                .contains("only the observation engine may supersede"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn hard_delete_derived_requires_trusted_actor() {
        let (fake, svc) = derived_service();
        let err = svc
            .memory_delete(
                &crate::ActorContext::remote_untrusted(),
                "memory:mem_obs",
                true,
                "someone",
            )
            .await
            .expect_err("remote/untrusted actor");
        assert!(err.to_string().contains("trusted local actor"), "{err}");

        let err = svc
            .memory_delete(&crate::ActorContext::LOCAL, "memory:mem_obs", true, "")
            .await
            .expect_err("anonymous actor");
        assert!(err.to_string().contains("trusted local actor"), "{err}");

        let outcome = svc
            .memory_delete(
                &crate::ActorContext::with_actor("maintainer"),
                "memory:mem_obs",
                true,
                "",
            )
            .await
            .expect("trusted local actor may hard-delete for maintenance");
        assert_eq!(outcome, "deleted");
        assert!(fake
            .execute_calls
            .lock()
            .unwrap()
            .iter()
            .any(|sql| sql.contains("DELETE memory:mem_obs")));
    }

    #[tokio::test]
    async fn export_excludes_derived() {
        let fake = Arc::new(FakeDb::new(vec![("FROM memory", rows_of(vec![]))]));
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        svc.memory_export(&crate::ActorContext::LOCAL, &MemoryExportParams::default())
            .await
            .expect("export");
        let sql = fake
            .smart_calls
            .lock()
            .unwrap()
            .iter()
            .find(|q| q.contains("FROM memory"))
            .cloned()
            .expect("export query");
        assert!(
            sql.contains("(store = NONE OR store != 'derived')"),
            "{sql}"
        );
    }

    #[tokio::test]
    async fn memory_list_filters_by_store() {
        let fake = Arc::new(FakeDb::new(vec![("FROM memory", rows_of(vec![]))]));
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        let last_list_sql = || {
            fake.smart_calls
                .lock()
                .unwrap()
                .iter()
                .rfind(|q| q.contains("FROM memory"))
                .cloned()
                .expect("list query")
        };

        svc.list_memories(
            &crate::ActorContext::LOCAL,
            MemoryListParams {
                store: "derived".into(),
                ..Default::default()
            },
        )
        .await
        .expect("derived list");
        assert!(last_list_sql().contains("store = 'derived'"));

        svc.list_memories(
            &crate::ActorContext::LOCAL,
            MemoryListParams {
                store: "git".into(),
                ..Default::default()
            },
        )
        .await
        .expect("git list");
        assert!(last_list_sql().contains("store = 'git'"));

        svc.list_memories(&crate::ActorContext::LOCAL, MemoryListParams::default())
            .await
            .expect("all list");
        assert!(!last_list_sql().contains("store ="), "{}", last_list_sql());

        let err = svc
            .list_memories(
                &crate::ActorContext::LOCAL,
                MemoryListParams {
                    store: "surreal".into(),
                    ..Default::default()
                },
            )
            .await
            .expect_err("surreal is not a filter value");
        assert!(err.to_string().contains("git, derived, or all"), "{err}");
    }

    #[tokio::test]
    async fn memory_search_projects_store() {
        let fake = Arc::new(FakeDb::new(vec![("FROM memory", rows_of(vec![]))]));
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        svc.search_memory(
            &crate::ActorContext::LOCAL,
            MemorySearchParams {
                query: "regression".into(),
                ..Default::default()
            },
        )
        .await
        .expect("search");
        let sql = fake
            .smart_calls
            .lock()
            .unwrap()
            .iter()
            .find(|q| q.contains("vector::distance::cosine"))
            .cloned()
            .expect("memory window");
        assert!(sql.contains("updated_at, store, details"), "{sql}");
    }

    // ------------------------------------------------------------------
    // C8 step 6 — ai.observations resolution + scope discovery
    // ------------------------------------------------------------------

    #[test]
    fn observation_settings_from_config_clamps_and_whitelists() {
        let dflt =
            ObservationSettings::from_config(&ibis_core::config::ObservationsConfig::default());
        assert_eq!(dflt, ObservationSettings::default());

        let resolved = ObservationSettings::from_config(&ibis_core::config::ObservationsConfig {
            enabled: true,
            min_cluster_size: 1,
            max_members: 999,
            hub_degree_max: 0,
            max_clusters_per_repo: 0,
            max_members_examined: 9999,
            max_scopes_per_project: 0,
            max_llm_summaries_per_pass: 0,
            families: vec!["file".into(), "bogus".into(), "file".into()],
            decide_shadow: true,
        });
        assert!(resolved.enabled);
        assert_eq!(resolved.limits.min_cluster_size, 2);
        assert_eq!(resolved.limits.max_members, 100);
        assert_eq!(resolved.limits.hub_degree_max, 1);
        assert_eq!(resolved.limits.max_clusters_per_repo, 1);
        assert_eq!(
            resolved.limits.max_members_examined,
            OBSERVATION_MAX_EXAMINED
        );
        assert_eq!(resolved.max_scopes_per_project, 1);
        assert_eq!(resolved.max_llm_summaries_per_pass, 1);
        assert_eq!(resolved.families, vec![FAMILY_FILE.to_string()]);
        assert!(resolved.family_allowed(FAMILY_FILE));
        assert!(!resolved.family_allowed(FAMILY_FIX_ROOT_CAUSE));
        assert!(resolved.decide_shadow);

        // An all-unknown whitelist falls back to every family (never an
        // accidental archive-everything configuration).
        let all_unknown =
            ObservationSettings::from_config(&ibis_core::config::ObservationsConfig {
                families: vec!["wat".into()],
                ..Default::default()
            });
        assert_eq!(all_unknown.families.len(), OBSERVATION_FAMILIES.len());
    }

    #[test]
    fn family_whitelist_restricts_signature_extraction() {
        let graph = Extraction {
            memories: vec![ObsMemoryRow {
                id: "memory:mem_a".to_string(),
                kind: "fix".to_string(),
                title: "Crash on empty discovery".to_string(),
                content: "The wizard crashes on an empty discovery root.".to_string(),
                details: serde_json::json!({ "root_cause": ROOT_CAUSE }),
                root_cause: ROOT_CAUSE.to_string(),
                source_path: "src/wizard.rs".to_string(),
                introduced_in: String::new(),
            }],
            ..Default::default()
        };
        let all = build_signature_inputs(
            &graph,
            &OBSERVATION_FAMILIES
                .iter()
                .map(|family| family.to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(all.inputs[0].signatures.len(), 2, "fix + file");
        let only_file = build_signature_inputs(&graph, &[FAMILY_FILE.to_string()]);
        assert_eq!(only_file.inputs[0].signatures.len(), 1);
        assert_eq!(only_file.inputs[0].signatures[0].family, FAMILY_FILE);
    }

    #[tokio::test]
    async fn observation_scopes_are_team_first_sorted_and_capped() {
        let fake = Arc::new(FakeDb::new(vec![(
            "SELECT owner FROM memory",
            rows_of(vec![
                obj(vec![("owner", Value::from_t("bob"))]),
                obj(vec![("owner", Value::from_t("alice"))]),
                obj(vec![("owner", Value::from_t(""))]),
                obj(vec![("owner", Value::from_t("alice"))]),
            ]),
        )]));
        let scopes = observation_scopes_for_project(fake.as_ref(), PROJ, 2)
            .await
            .expect("scopes");
        assert_eq!(
            scopes,
            vec![ObsScope::Team, ObsScope::Personal("alice".to_string())],
            "team first, owners sorted, empty owner skipped, cap respected"
        );
        let all = observation_scopes_for_project(fake.as_ref(), PROJ, 8)
            .await
            .expect("scopes");
        assert_eq!(
            all,
            vec![
                ObsScope::Team,
                ObsScope::Personal("alice".to_string()),
                ObsScope::Personal("bob".to_string())
            ]
        );
    }
}
