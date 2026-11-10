//! E15 — deterministic citations & trust for the agentic ask (producer).
//!
//! Implements the shared E14/E15 contract (2026-11-09 planning doc, §0 and
//! §E15): a stable citation catalog (`S1`..`S20`, first-observation order),
//! bounded excerpts, the single claim-level coverage definition and the
//! base-1000 integer trust score. There are ZERO LLM calls here — the same
//! inputs always produce the same output.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::search::SearchResult;

/// Maximum number of citable sources (ids `S1`..`S20`); beyond the cap a
/// source stays in `sources` but is not citable (§0.6).
pub(crate) const MAX_CITATIONS: usize = 20;
/// Bounded excerpt length in runes for the citation `excerpt` field.
pub(crate) const CITATION_EXCERPT_RUNES: usize = 240;
/// Weak-context evidence threshold: sources with `score >= 0.30` count as
/// "above threshold" (inclusive).
pub(crate) const MIN_EVIDENCE_SCORE: f64 = 0.30;
/// Weak context requires at least this many observed sources *and* this
/// many sources above [`MIN_EVIDENCE_SCORE`].
pub(crate) const WEAK_CONTEXT_MIN_SOURCES: usize = 2;

/// E15.5 evidence-block caps for `capture_observation` (shared so the
/// numbers stay versioned in one place).
pub(crate) const CAPTURE_EVIDENCE_MAX_ENTRIES: usize = 20;
/// Excerpt length for the capture evidence block (runes, collapsed).
pub(crate) const CAPTURE_EVIDENCE_EXCERPT_RUNES: usize = 160;

/// Trust component weights (sum 1000, §E15.2).
const TRUST_W_COVERAGE: u64 = 400;
const TRUST_W_SOURCE_RATIO: u64 = 200;
const TRUST_W_TOP_SCORE: u64 = 150;
const TRUST_W_CURRENT: u64 = 100;
const TRUST_W_DIVERSITY: u64 = 100;
const TRUST_W_CITATION: u64 = 50;
/// Constant offset: after the final `/ 1000` an all-zero assessment is 0.
const TRUST_BASE_OFFSET: u64 = 500;

/// Stoplist of isolated abbreviations that are never claims (§0.5).
const CLAIM_STOPLIST: [&str; 6] = ["e.g.", "i.e.", "etc.", "vs.", "cf.", "et al."];

/// One citable evidence entry (§0.3 wire shape). Field-for-field projection
/// of the observed [`SearchResult`]; `path`/`commit_hash`/`excerpt` are
/// omitted from the wire when empty. There is no `line` field.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgenticCitation {
    #[serde(default)]
    pub citation_id: String,
    #[serde(default)]
    pub source_id: String,
    #[serde(default)]
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit_hash: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt: String,
}

/// Deterministic trust signal for an agentic answer (§0.4). Always
/// serialized, even in the honest "no evidence" default state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgenticTrust {
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub level: TrustLevel,
    #[serde(default)]
    pub coverage: f64,
    #[serde(default)]
    pub weak_context: bool,
    #[serde(default)]
    pub cited_sources: usize,
    #[serde(default)]
    pub observed_sources: usize,
}

/// Trust level buckets (`high` | `medium` | `low`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    High,
    Medium,
    #[default]
    Low,
}

impl Default for AgenticTrust {
    fn default() -> Self {
        Self {
            score: 0.0,
            level: TrustLevel::Low,
            coverage: 0.0,
            weak_context: true,
            cited_sources: 0,
            observed_sources: 0,
        }
    }
}

/// The outcome of observing one search result (§E15.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ObserveOutcome {
    /// In the catalog (new or already seen): the source is citable.
    Cited(String),
    /// Catalog cap reached: the source stays in `sources` and in the
    /// observation, but has no citation id.
    Uncited,
    /// Unidentifiable (id and fallback key all empty): dropped from both
    /// the catalog and `sources`.
    Skip,
}

/// One catalog record's non-wire metadata (kept out of [`AgenticCitation`]
/// so the payload shape stays exactly §0.3).
#[derive(Debug)]
struct CatalogMeta {
    key: String,
    is_current: bool,
}

/// Deterministic, ask-local citation catalog (§0.6): stable ids in
/// first-observation order, dedup, cap, first-snapshot-wins.
#[derive(Debug, Default)]
pub(crate) struct CitationCatalog {
    entries: Vec<AgenticCitation>,
    meta: Vec<CatalogMeta>,
}

impl CitationCatalog {
    /// Observes one search result: assigns or reuses the citation id,
    /// applies dedup and the cap. A repeated key never refreshes the entry
    /// (first snapshot wins).
    pub(crate) fn observe(&mut self, r: &SearchResult) -> ObserveOutcome {
        let Some(key) = citation_key(r) else {
            return ObserveOutcome::Skip;
        };
        if let Some(i) = self.meta.iter().position(|m| m.key == key) {
            return ObserveOutcome::Cited(self.entries[i].citation_id.clone());
        }
        if self.entries.len() >= MAX_CITATIONS {
            return ObserveOutcome::Uncited;
        }
        let citation_id = format!("S{}", self.entries.len() + 1);
        let source_kind = r.source_kind.trim();
        self.entries.push(AgenticCitation {
            citation_id: citation_id.clone(),
            source_id: r.id.clone(),
            source_kind: if source_kind.is_empty() {
                "unknown".to_string()
            } else {
                source_kind.to_string()
            },
            path: r.path.clone(),
            commit_hash: r.commit_hash.clone(),
            score: sanitize_score(r.score),
            excerpt: bounded_excerpt(&r.content, CITATION_EXCERPT_RUNES),
        });
        self.meta.push(CatalogMeta {
            key,
            is_current: r.is_current,
        });
        ObserveOutcome::Cited(citation_id)
    }

    /// The citable entries — the `citations` payload slice.
    pub(crate) fn entries(&self) -> &[AgenticCitation] {
        &self.entries
    }

    /// Catalog size after dedup and cap (the trust denominator).
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Valid citation ids (post-cap): any marker outside this set is
    /// invalid and never covers a claim.
    pub(crate) fn valid_ids(&self) -> HashSet<usize> {
        (1..=self.entries.len()).collect()
    }
}

/// The catalog dedup key (§0.6): `id` when non-empty, else
/// `source_kind|path|commit_hash`; `None` = unidentifiable (`Skip`).
pub(crate) fn citation_key(r: &SearchResult) -> Option<String> {
    if !r.id.trim().is_empty() {
        return Some(r.id.clone());
    }
    if r.source_kind.trim().is_empty()
        && r.path.trim().is_empty()
        && r.commit_hash.trim().is_empty()
    {
        return None;
    }
    Some(format!("{}|{}|{}", r.source_kind, r.path, r.commit_hash))
}

/// Collapses every whitespace run into a single space, trims, keeps the
/// first `max_runes` runes and appends `...` when truncated. Empty content
/// stays an empty excerpt.
pub(crate) fn bounded_excerpt(content: &str, max_runes: usize) -> String {
    let collapsed: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let runes: Vec<char> = collapsed.chars().collect();
    if runes.len() > max_runes {
        let mut out: String = runes[..max_runes].iter().collect();
        out.push_str("...");
        out
    } else {
        collapsed
    }
}

/// `NaN`/inf → `0.0`; the citation carries the sanitized retrieval value
/// (the `[0,1]` clamp happens only inside the trust score).
fn sanitize_score(score: f64) -> f64 {
    if score.is_finite() {
        score
    } else {
        0.0
    }
}

/// Base-1000 bucket of a value clamped to `[0,1]` (`bp(x)` of §E15.2).
fn bp(value: f64) -> u64 {
    (value.clamp(0.0, 1.0) * 1000.0).round() as u64
}

/// Level buckets, with the weak-context cap: `high` is downgraded to
/// `medium` with fewer than 2 sources above the evidence threshold.
pub(crate) fn trust_level(score_bp: u64, weak_context: bool) -> TrustLevel {
    let level = if score_bp >= 700 {
        TrustLevel::High
    } else if score_bp >= 450 {
        TrustLevel::Medium
    } else {
        TrustLevel::Low
    };
    if weak_context && level == TrustLevel::High {
        TrustLevel::Medium
    } else {
        level
    }
}

/// `weak_context = observed_sources < 2 || above_threshold < 2` (§E15.2).
fn is_weak_context(observed_sources: usize, above_threshold: usize) -> bool {
    observed_sources < WEAK_CONTEXT_MIN_SOURCES || above_threshold < WEAK_CONTEXT_MIN_SOURCES
}

/// The deterministic assessment of one candidate answer (§0.5/§E15.2):
/// trust plus the repair-facing counts E14 consumes.
#[derive(Debug, Clone)]
pub(crate) struct TrustAssessment {
    pub trust: AgenticTrust,
    /// Repair-facing counts: E15 publishes them, the E14 policy reads them
    /// (coverage threshold, repair observation, `invalid_citations` flag).
    pub total_claims: usize,
    pub covered_claims: usize,
    pub invalid_citations: usize,
    pub coverage_bp: u64,
}

/// The single implementer of coverage + trust (§E15.3): segments the claims
/// of `answer`, matches citation markers against the catalog and computes
/// every trust component in exact base-1000 integer math.
pub(crate) fn assess(catalog: &CitationCatalog, answer: &str) -> TrustAssessment {
    let visible = strip_code(answer);
    let valid = catalog.valid_ids();
    let mut total_claims = 0usize;
    let mut covered_claims = 0usize;
    let mut invalid_citations = 0usize;
    let mut cited_ids: HashSet<usize> = HashSet::new();

    for segment in segment_claims(&visible) {
        let trimmed = segment.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.ends_with(':') {
            continue;
        }
        let stripped = re_marker().replace_all(trimmed, " ");
        if !stripped.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        if CLAIM_STOPLIST.contains(&trimmed.to_lowercase().as_str()) {
            continue;
        }

        total_claims += 1;
        let mut covered = false;
        let mut claim_invalid: HashSet<usize> = HashSet::new();
        for id in marker_ids(trimmed) {
            if valid.contains(&id) {
                covered = true;
                cited_ids.insert(id);
            } else {
                claim_invalid.insert(id);
            }
        }
        if covered {
            covered_claims += 1;
        }
        invalid_citations += claim_invalid.len();
    }

    let observed_sources = catalog.len();
    let cited_sources = cited_ids.len();
    let coverage_bp = if total_claims == 0 {
        0
    } else {
        bp(covered_claims as f64 / total_claims as f64)
    };
    let source_ratio_bp = if observed_sources == 0 {
        0
    } else {
        bp(cited_sources as f64 / observed_sources as f64)
    };
    let top_bp = top_score_bp(&catalog.entries);

    let mut current_kind_sources = 0usize;
    let mut current_count = 0usize;
    let mut distinct_kinds: HashSet<&str> = HashSet::new();
    let mut above_threshold = 0usize;
    for (entry, meta) in catalog.entries.iter().zip(catalog.meta.iter()) {
        if !entry.source_kind.is_empty() {
            distinct_kinds.insert(entry.source_kind.as_str());
        }
        if entry.score >= MIN_EVIDENCE_SCORE {
            above_threshold += 1;
        }
        if entry.source_kind == "file_chunk" || entry.source_kind == "memory" {
            current_kind_sources += 1;
            if meta.is_current {
                current_count += 1;
            }
        }
    }
    // Neutral 500 for commit-only asks: no file_chunk/memory entry to judge.
    let current_bp = if observed_sources == 0 {
        0
    } else if current_kind_sources == 0 {
        500
    } else {
        bp(current_count as f64 / current_kind_sources as f64)
    };
    let diversity_bp = bp((distinct_kinds.len().min(3) as f64) / 3.0);
    let citation_bp = bp((cited_sources.min(3) as f64) / 3.0);
    let weak_context = is_weak_context(observed_sources, above_threshold);

    let score_bp = (TRUST_W_COVERAGE * coverage_bp
        + TRUST_W_SOURCE_RATIO * source_ratio_bp
        + TRUST_W_TOP_SCORE * top_bp
        + TRUST_W_CURRENT * current_bp
        + TRUST_W_DIVERSITY * diversity_bp
        + TRUST_W_CITATION * citation_bp
        + TRUST_BASE_OFFSET)
        / 1000;

    let trust = AgenticTrust {
        score: score_bp as f64 / 1000.0,
        level: trust_level(score_bp, weak_context),
        coverage: coverage_bp as f64 / 1000.0,
        weak_context,
        cited_sources,
        observed_sources,
    };
    TrustAssessment {
        trust,
        total_claims,
        covered_claims,
        invalid_citations,
        coverage_bp,
    }
}

/// Top retrieval score bucket; `0` for an empty catalog or an all-NaN one
/// (scores are sanitized on the way in, so the guard is defensive).
fn top_score_bp(entries: &[AgenticCitation]) -> u64 {
    let mut top = f64::NEG_INFINITY;
    for entry in entries {
        if entry.score.is_nan() {
            continue;
        }
        top = top.max(entry.score);
    }
    if top.is_finite() {
        bp(top)
    } else {
        0
    }
}

/// Removes fenced code blocks and inline code spans before segmentation:
/// code is evidence, not claim (§0.5).
fn strip_code(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '`' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if i + 2 < chars.len() && chars[i + 1] == '`' && chars[i + 2] == '`' {
            i += 3;
            while i < chars.len() {
                if chars[i] == '`'
                    && i + 2 < chars.len()
                    && chars[i + 1] == '`'
                    && chars[i + 2] == '`'
                {
                    i += 3;
                    break;
                }
                i += 1;
            }
        } else {
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
        }
    }
    out
}

/// Splits the code-stripped answer into candidate claim segments: list
/// lines are autonomous segments, every other line is split on sentence
/// boundaries (§0.5).
fn segment_claims(text: &str) -> Vec<String> {
    let mut segments = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_list_line(trimmed) {
            segments.push(trimmed.to_string());
            continue;
        }
        for sentence in split_sentences(trimmed) {
            let trimmed = sentence.trim();
            if !trimmed.is_empty() {
                segments.push(trimmed.to_string());
            }
        }
    }
    segments
}

/// `- `, `* ` and `N. ` prefixes mark an autonomous list segment.
fn is_list_line(trimmed: &str) -> bool {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && trimmed[digits..].starts_with(". ")
}

/// Splits one line on sentence boundaries: a terminator followed by
/// whitespace and an uppercase letter/digit/`[`/`(`/`"`, or by the end of
/// the text. Decimals, versions and URLs never split (the terminator must
/// be followed by whitespace).
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if is_sentence_terminator(chars[i]) {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let at_end = j >= chars.len();
            let boundary = at_end || {
                let had_whitespace = j > i + 1;
                let next = chars[j];
                had_whitespace
                    && (next.is_uppercase()
                        || next.is_ascii_digit()
                        || matches!(next, '[' | '(' | '"'))
            };
            if boundary {
                segments.push(chars[start..=i].iter().collect::<String>());
                start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if start < chars.len() {
        segments.push(chars[start..].iter().collect::<String>());
    }
    segments
}

fn is_sentence_terminator(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '。' | '！' | '？')
}

fn re_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\s*([Ss]\d+(?:\s*,\s*[Ss]\d+)*)\s*\]").expect("static regex"))
}

/// Normalized ids of every citation marker in `text` (`[S1]`, `[s2]`,
/// `[S1, S2]`); malformed markers are not recognized.
fn marker_ids(text: &str) -> Vec<usize> {
    let mut ids = Vec::new();
    for cap in re_marker().captures_iter(text) {
        let inner = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        for part in inner.split(',') {
            let part = part.trim();
            let digits = part
                .strip_prefix('S')
                .or_else(|| part.strip_prefix('s'))
                .unwrap_or("");
            if let Ok(n) = digits.parse::<usize>() {
                ids.push(n);
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sr(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            source_kind: "file_chunk".to_string(),
            ..Default::default()
        }
    }

    fn sr_full(
        id: &str,
        kind: &str,
        path: &str,
        commit: &str,
        score: f64,
        is_current: bool,
        content: &str,
    ) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            source_kind: kind.to_string(),
            path: path.to_string(),
            commit_hash: commit.to_string(),
            score,
            is_current,
            content: content.to_string(),
            ..Default::default()
        }
    }

    // --- catalog ---------------------------------------------------------

    #[test]
    fn citation_catalog_assigns_ids_in_first_observation_order() {
        let mut catalog = CitationCatalog::default();
        assert_eq!(
            catalog.observe(&sr("file_chunk:a")),
            ObserveOutcome::Cited("S1".to_string())
        );
        assert_eq!(
            catalog.observe(&sr("memory:m1")),
            ObserveOutcome::Cited("S2".to_string())
        );
        assert_eq!(
            catalog.observe(&sr("commit_chunk:c1")),
            ObserveOutcome::Cited("S3".to_string())
        );
        let ids: Vec<&str> = catalog
            .entries()
            .iter()
            .map(|e| e.citation_id.as_str())
            .collect();
        assert_eq!(ids, vec!["S1", "S2", "S3"]);
        let sources: Vec<&str> = catalog
            .entries()
            .iter()
            .map(|e| e.source_id.as_str())
            .collect();
        assert_eq!(
            sources,
            vec!["file_chunk:a", "memory:m1", "commit_chunk:c1"]
        );
        assert_eq!(catalog.len(), 3);
    }

    #[test]
    fn citation_catalog_trims_source_kind_and_keeps_unknown_for_empty() {
        let mut catalog = CitationCatalog::default();
        catalog.observe(&sr_full(
            "file_chunk:a",
            "  file_chunk  ",
            "src/a.rs",
            "",
            0.9,
            true,
            "content",
        ));
        catalog.observe(&sr_full(
            "file_chunk:b",
            "   ",
            "src/b.rs",
            "",
            0.8,
            true,
            "content",
        ));
        assert_eq!(catalog.entries()[0].source_kind, "file_chunk");
        assert_eq!(catalog.entries()[1].source_kind, "unknown");
    }

    #[test]
    fn citation_catalog_dedupes_repeated_source_id() {
        let mut catalog = CitationCatalog::default();
        assert_eq!(
            catalog.observe(&sr_full(
                "file_chunk:a",
                "file_chunk",
                "src/a.rs",
                "",
                0.9,
                true,
                "first"
            )),
            ObserveOutcome::Cited("S1".to_string())
        );
        assert_eq!(
            catalog.observe(&sr_full(
                "file_chunk:a",
                "file_chunk",
                "src/a.rs",
                "",
                0.1,
                false,
                "second"
            )),
            ObserveOutcome::Cited("S1".to_string())
        );
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.entries()[0].excerpt, "first");
        assert_eq!(catalog.entries()[0].score, 0.9);
    }

    #[test]
    fn citation_catalog_falls_back_to_kind_path_commit_for_empty_id() {
        let mut catalog = CitationCatalog::default();
        assert_eq!(
            catalog.observe(&sr_full(
                "",
                "file_chunk",
                "src/x.rs",
                "abc",
                0.5,
                true,
                "one"
            )),
            ObserveOutcome::Cited("S1".to_string())
        );
        // Same fallback key: dedup, first snapshot wins.
        assert_eq!(
            catalog.observe(&sr_full(
                "",
                "file_chunk",
                "src/x.rs",
                "abc",
                0.9,
                true,
                "two"
            )),
            ObserveOutcome::Cited("S1".to_string())
        );
        // Different commit: a new key.
        assert_eq!(
            catalog.observe(&sr_full(
                "",
                "file_chunk",
                "src/x.rs",
                "def",
                0.5,
                true,
                "three"
            )),
            ObserveOutcome::Cited("S2".to_string())
        );
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.entries()[0].source_id, "");
        assert_eq!(catalog.entries()[0].path, "src/x.rs");
        assert_eq!(catalog.entries()[0].commit_hash, "abc");
    }

    #[test]
    fn citation_catalog_skips_unidentifiable_hit() {
        let mut catalog = CitationCatalog::default();
        let unidentifiable = SearchResult::default();
        assert_eq!(catalog.observe(&unidentifiable), ObserveOutcome::Skip);
        assert_eq!(catalog.len(), 0);
        assert!(catalog.entries().is_empty());
    }

    #[test]
    fn citation_catalog_caps_at_20() {
        let mut catalog = CitationCatalog::default();
        for i in 1..=MAX_CITATIONS {
            assert_eq!(
                catalog.observe(&sr(&format!("chunk:{i}"))),
                ObserveOutcome::Cited(format!("S{i}"))
            );
        }
        for i in 21..=25 {
            assert_eq!(
                catalog.observe(&sr(&format!("chunk:{i}"))),
                ObserveOutcome::Uncited
            );
        }
        assert_eq!(catalog.len(), MAX_CITATIONS);
        assert_eq!(catalog.entries()[19].citation_id, "S20");
    }

    #[test]
    fn citation_catalog_keeps_first_snapshot() {
        let mut catalog = CitationCatalog::default();
        catalog.observe(&sr_full(
            "file_chunk:a",
            "file_chunk",
            "src/a.rs",
            "old",
            0.91,
            true,
            "the first snapshot",
        ));
        catalog.observe(&sr_full(
            "file_chunk:a",
            "file_chunk",
            "src/renamed.rs",
            "new",
            0.10,
            false,
            "a newer observation",
        ));
        let entry = &catalog.entries()[0];
        assert_eq!(entry.excerpt, "the first snapshot");
        assert_eq!(entry.score, 0.91);
        assert_eq!(entry.path, "src/a.rs");
        assert_eq!(entry.commit_hash, "old");
    }

    #[test]
    fn citation_excerpt_is_bounded_and_whitespace_collapsed() {
        let mut catalog = CitationCatalog::default();
        catalog.observe(&sr_full(
            "file_chunk:a",
            "file_chunk",
            "src/a.rs",
            "",
            0.5,
            true,
            "  hello\t\tworld\n\nsecond   line  ",
        ));
        assert_eq!(catalog.entries()[0].excerpt, "hello world second line");

        let mut long = CitationCatalog::default();
        long.observe(&sr_full(
            "file_chunk:b",
            "file_chunk",
            "src/b.rs",
            "",
            0.5,
            true,
            &"x".repeat(300),
        ));
        let excerpt = &long.entries()[0].excerpt;
        assert_eq!(excerpt.chars().count(), CITATION_EXCERPT_RUNES + 3);
        assert_eq!(
            excerpt,
            &format!("{}...", "x".repeat(CITATION_EXCERPT_RUNES))
        );
    }

    #[test]
    fn citation_empty_content_keeps_entry() {
        let mut catalog = CitationCatalog::default();
        assert_eq!(
            catalog.observe(&sr_full(
                "file_chunk:a",
                "file_chunk",
                "src/a.rs",
                "",
                0.5,
                true,
                ""
            )),
            ObserveOutcome::Cited("S1".to_string())
        );
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.entries()[0].excerpt, "");
        assert_eq!(catalog.entries()[0].path, "src/a.rs");
    }

    #[test]
    fn citation_score_is_sanitized() {
        let mut catalog = CitationCatalog::default();
        catalog.observe(&sr_full(
            "file_chunk:nan",
            "file_chunk",
            "",
            "",
            f64::NAN,
            true,
            "",
        ));
        catalog.observe(&sr_full(
            "file_chunk:inf",
            "file_chunk",
            "",
            "",
            f64::INFINITY,
            true,
            "",
        ));
        catalog.observe(&sr_full(
            "file_chunk:ninf",
            "file_chunk",
            "",
            "",
            f64::NEG_INFINITY,
            true,
            "",
        ));
        catalog.observe(&sr_full(
            "file_chunk:ok",
            "file_chunk",
            "",
            "",
            0.87,
            true,
            "",
        ));
        assert_eq!(catalog.entries()[0].score, 0.0);
        assert_eq!(catalog.entries()[1].score, 0.0);
        assert_eq!(catalog.entries()[2].score, 0.0);
        assert_eq!(catalog.entries()[3].score, 0.87);
    }

    // --- trust matrix ----------------------------------------------------

    struct Node {
        kind: &'static str,
        score: f64,
        is_current: bool,
    }

    fn catalog(entries: &[Node]) -> CitationCatalog {
        let mut out = CitationCatalog::default();
        for (i, node) in entries.iter().enumerate() {
            out.entries.push(AgenticCitation {
                citation_id: format!("S{}", i + 1),
                source_id: format!("src:{i}"),
                source_kind: node.kind.to_string(),
                path: String::new(),
                commit_hash: String::new(),
                score: node.score,
                excerpt: String::new(),
            });
            out.meta.push(CatalogMeta {
                key: format!("src:{i}"),
                is_current: node.is_current,
            });
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn check_case(
        name: &str,
        nodes: &[Node],
        answer: &str,
        score_bp: u64,
        coverage_norm: f64,
        level: TrustLevel,
        weak: bool,
        cited: usize,
    ) {
        let catalog = catalog(nodes);
        let assessment = assess(&catalog, answer);
        assert_eq!(
            (assessment.trust.coverage * 1000.0).round() as u64,
            (coverage_norm * 1000.0).round() as u64,
            "{name}: coverage"
        );
        assert_eq!(
            assessment.trust.observed_sources,
            catalog.len(),
            "{name}: observed_sources"
        );
        assert_eq!(
            assessment.trust.cited_sources, cited,
            "{name}: cited_sources"
        );
        assert_eq!(
            (assessment.trust.score * 1000.0).round() as u64,
            score_bp,
            "{name}: score_bp"
        );
        assert_eq!(assessment.trust.level, level, "{name}: level");
        assert_eq!(assessment.trust.weak_context, weak, "{name}: weak_context");
    }

    #[test]
    fn trust_matrix_matches_table() {
        // A — 4/4 claims, 3 observed, 3 cited, top 0.90, 2/2 current, 3 kinds.
        check_case(
            "A",
            &[
                Node {
                    kind: "file_chunk",
                    score: 0.90,
                    is_current: true,
                },
                Node {
                    kind: "commit_chunk",
                    score: 0.85,
                    is_current: false,
                },
                Node {
                    kind: "memory",
                    score: 0.70,
                    is_current: true,
                },
            ],
            "A [S1]. B [S2]. C [S3]. D [S1].",
            985,
            1.0,
            TrustLevel::High,
            false,
            3,
        );
        // B — 2/4 claims, 4 observed, 2 cited, top 0.60, 1/2 current, 2 kinds.
        check_case(
            "B",
            &[
                Node {
                    kind: "file_chunk",
                    score: 0.60,
                    is_current: true,
                },
                Node {
                    kind: "file_chunk",
                    score: 0.55,
                    is_current: false,
                },
                Node {
                    kind: "commit_chunk",
                    score: 0.50,
                    is_current: false,
                },
                Node {
                    kind: "commit_chunk",
                    score: 0.40,
                    is_current: false,
                },
            ],
            "A [S1]. B. C [S2]. D.",
            540,
            0.5,
            TrustLevel::Medium,
            false,
            2,
        );
        // C — 0/3 claims, 5 observed, 0 cited, top 0.20, none current, 1 kind.
        check_case(
            "C",
            &[
                Node {
                    kind: "file_chunk",
                    score: 0.20,
                    is_current: false,
                },
                Node {
                    kind: "file_chunk",
                    score: 0.15,
                    is_current: false,
                },
                Node {
                    kind: "file_chunk",
                    score: 0.10,
                    is_current: false,
                },
                Node {
                    kind: "file_chunk",
                    score: 0.05,
                    is_current: false,
                },
                Node {
                    kind: "file_chunk",
                    score: 0.01,
                    is_current: false,
                },
            ],
            "A. B. C.",
            63,
            0.0,
            TrustLevel::Low,
            true,
            0,
        );
        // D — empty catalog, no claims: the honest no-evidence state.
        check_case("D", &[], "", 0, 0.0, TrustLevel::Low, true, 0);
        // E — 3/4 claims, 3 observed, 3 cited, top 1.00, 2/2 current, 2 kinds.
        check_case(
            "E",
            &[
                Node {
                    kind: "file_chunk",
                    score: 1.00,
                    is_current: true,
                },
                Node {
                    kind: "file_chunk",
                    score: 0.90,
                    is_current: true,
                },
                Node {
                    kind: "commit_chunk",
                    score: 0.80,
                    is_current: false,
                },
            ],
            "A [S1]. B [S2]. C [S3]. D.",
            867,
            0.75,
            TrustLevel::High,
            false,
            3,
        );
        // F — 0/3 claims, 3 observed, 0 cited, top 0.80, all current, 1 kind.
        check_case(
            "F",
            &[
                Node {
                    kind: "memory",
                    score: 0.80,
                    is_current: true,
                },
                Node {
                    kind: "memory",
                    score: 0.75,
                    is_current: true,
                },
                Node {
                    kind: "memory",
                    score: 0.70,
                    is_current: true,
                },
            ],
            "A. B. C.",
            253,
            0.0,
            TrustLevel::Low,
            false,
            0,
        );
        // G — commit-only: neutral current ratio, 2/2 claims all cited.
        check_case(
            "G",
            &[
                Node {
                    kind: "commit_chunk",
                    score: 0.70,
                    is_current: false,
                },
                Node {
                    kind: "commit_chunk",
                    score: 0.65,
                    is_current: false,
                },
            ],
            "A [S1]. B [S2].",
            822,
            1.0,
            TrustLevel::High,
            false,
            2,
        );
        // H — single source: high score but weak-context capped to medium.
        check_case(
            "H",
            &[Node {
                kind: "file_chunk",
                score: 0.90,
                is_current: true,
            }],
            "A [S1].",
            885,
            1.0,
            TrustLevel::Medium,
            true,
            1,
        );
        // I — full catalog (cap 20 from 25 observations in the cap test).
        let mut nodes_i = Vec::new();
        for i in 0..5 {
            nodes_i.push(Node {
                kind: "file_chunk",
                score: if i == 0 { 0.90 } else { 0.50 },
                is_current: true,
            });
        }
        for _ in 0..5 {
            nodes_i.push(Node {
                kind: "memory",
                score: 0.50,
                is_current: true,
            });
        }
        for _ in 0..10 {
            nodes_i.push(Node {
                kind: "commit_chunk",
                score: 0.50,
                is_current: false,
            });
        }
        let answer_i: String = (1..=20)
            .map(|i| format!("C{i} [S{i}]."))
            .collect::<Vec<_>>()
            .join(" ");
        check_case(
            "I",
            &nodes_i,
            &answer_i,
            985,
            1.0,
            TrustLevel::High,
            false,
            20,
        );
        // J — all sources below threshold: weak cap but 2/2 grounds it.
        check_case(
            "J",
            &[
                Node {
                    kind: "file_chunk",
                    score: 0.20,
                    is_current: true,
                },
                Node {
                    kind: "memory",
                    score: 0.10,
                    is_current: true,
                },
            ],
            "A [S1]. B [S2].",
            830,
            1.0,
            TrustLevel::Medium,
            true,
            2,
        );
    }

    #[test]
    fn trust_level_boundaries() {
        let cases = [
            (700, false, TrustLevel::High),
            (699, false, TrustLevel::Medium),
            (450, false, TrustLevel::Medium),
            (449, false, TrustLevel::Low),
            (700, true, TrustLevel::Medium),
            (985, true, TrustLevel::Medium),
            (450, true, TrustLevel::Medium),
            (449, true, TrustLevel::Low),
        ];
        for (score_bp, weak, expected) in cases {
            assert_eq!(
                trust_level(score_bp, weak),
                expected,
                "score_bp={score_bp} weak={weak}"
            );
        }
    }

    #[test]
    fn weak_context_depends_on_observed_and_above_threshold() {
        assert!(is_weak_context(0, 0), "observed 0 → weak");
        assert!(is_weak_context(1, 1), "observed 1 → weak");
        assert!(is_weak_context(2, 1), "2 observed but only 1 above → weak");
        assert!(!is_weak_context(2, 2), "2 observed, 2 above → not weak");
    }

    #[test]
    fn trust_zero_sources_is_low_and_weak() {
        let catalog = CitationCatalog::default();
        let assessment = assess(&catalog, "The loop is bounded [S1].");
        assert_eq!(assessment.trust.score, 0.0);
        assert_eq!(assessment.trust.level, TrustLevel::Low);
        assert!(assessment.trust.weak_context);
        assert_eq!(assessment.trust.coverage, 0.0);
        assert_eq!(assessment.trust.cited_sources, 0);
        assert_eq!(assessment.trust.observed_sources, 0);
        assert_eq!(assessment.covered_claims, 0);
        assert_eq!(assessment.invalid_citations, 1);
    }

    #[test]
    fn trust_commit_only_uses_neutral_current_ratio() {
        let catalog = catalog(&[
            Node {
                kind: "commit_chunk",
                score: 0.70,
                is_current: false,
            },
            Node {
                kind: "commit_chunk",
                score: 0.65,
                is_current: false,
            },
        ]);
        let assessment = assess(&catalog, "A [S1]. B [S2].");
        assert_eq!((assessment.trust.score * 1000.0).round() as u64, 822);
        assert_eq!(assessment.trust.level, TrustLevel::High);
        assert!(!assessment.trust.weak_context);
    }

    #[test]
    fn trust_cap_uses_catalog_size() {
        let mut catalog = CitationCatalog::default();
        for i in 1..=25 {
            let _ = catalog.observe(&sr(&format!("chunk:{i}")));
        }
        assert_eq!(catalog.len(), MAX_CITATIONS);
        let assessment = assess(&catalog, "Answer [S1].");
        assert_eq!(assessment.trust.observed_sources, MAX_CITATIONS);
        assert_eq!(assessment.trust.cited_sources, 1);
        // Ids beyond the cap never cover and never resolve.
        let beyond = assess(&catalog, "Answer [S21].");
        assert_eq!(beyond.covered_claims, 0);
        assert_eq!(beyond.invalid_citations, 1);
    }

    #[test]
    fn trust_rounding_is_base_1000() {
        let catalog = catalog(&[
            Node {
                kind: "file_chunk",
                score: 0.50,
                is_current: true,
            },
            Node {
                kind: "commit_chunk",
                score: 0.50,
                is_current: false,
            },
            Node {
                kind: "commit_chunk",
                score: 0.50,
                is_current: false,
            },
        ]);
        let assessment = assess(&catalog, "A [S1]. B [S2]. C.");
        assert_eq!(assessment.coverage_bp, 667);
        assert_eq!((assessment.trust.coverage * 1000.0).round() as u64, 667);
        assert_eq!(assessment.trust.coverage, 667.0 / 1000.0);
        assert_eq!(
            (assessment.trust.score * 1000.0).round() as u64,
            675,
            "400·667 + 200·667 + 150·500 + 100·1000 + 100·667 + 50·667 + 500 → 675 bp"
        );
    }

    // --- coverage --------------------------------------------------------

    fn two_entry_catalog() -> CitationCatalog {
        catalog(&[
            Node {
                kind: "file_chunk",
                score: 0.50,
                is_current: true,
            },
            Node {
                kind: "file_chunk",
                score: 0.50,
                is_current: true,
            },
        ])
    }

    #[test]
    fn coverage_claim_heuristic_matches_table() {
        let catalog = two_entry_catalog();
        let rows: [(&str, usize, usize, f64, usize); 15] = [
            (
                "The retry loop is bounded by MAX_RETRIES [S1]. It uses exponential backoff [S2].",
                2,
                2,
                1.0,
                0,
            ),
            (
                "The retry loop is bounded by MAX_RETRIES. It uses exponential backoff [S2].",
                2,
                1,
                0.5,
                0,
            ),
            (
                "- The loop is bounded [S1].\n- The backoff starts at 100ms [S2].",
                2,
                2,
                1.0,
                0,
            ),
            ("## Summary\nThe loop is bounded [S1].", 1, 1, 1.0, 0),
            (
                "```\nlet x = [S9];\n```\nThe value is one [S1].",
                1,
                1,
                1.0,
                0,
            ),
            ("Yes [S1]. No.", 2, 1, 0.5, 0),
            ("The loop is bounded [S99].", 1, 0, 0.0, 1),
            ("[S1].", 0, 0, 0.0, 0),
            ("Version 3.14 is stable [S1].", 1, 1, 1.0, 0),
            ("e.g. this is a claim [S1].", 1, 1, 1.0, 0),
            ("Steps:\n- The loop is bounded [S1].", 1, 1, 1.0, 0),
            ("The loop is bounded [S1, S2].", 1, 1, 1.0, 0),
            (
                "The loop is bounded [S1] and the backoff [S1].",
                1,
                1,
                1.0,
                0,
            ),
            ("The loop is bounded [s1].", 1, 1, 1.0, 0),
            ("The loop is bounded `[S1]`.", 1, 0, 0.0, 0),
        ];
        for (answer, total, covered, coverage, invalid) in rows {
            let assessment = assess(&catalog, answer);
            assert_eq!(assessment.total_claims, total, "total: {answer:?}");
            assert_eq!(assessment.covered_claims, covered, "covered: {answer:?}");
            assert_eq!(
                (assessment.trust.coverage * 1000.0).round() as u64,
                (coverage * 1000.0).round() as u64,
                "coverage: {answer:?}"
            );
            assert_eq!(assessment.invalid_citations, invalid, "invalid: {answer:?}");
        }
    }

    #[test]
    fn coverage_ignores_code_fences_and_inline_code() {
        let catalog = two_entry_catalog();
        let answer =
            "```\n[S99] fenced [S1]\n```\nThe value is one [S1]. The token `[S99]` is code.";
        let assessment = assess(&catalog, answer);
        assert_eq!(assessment.total_claims, 2, "code is not a claim");
        assert_eq!(assessment.covered_claims, 1);
        assert_eq!(
            assessment.invalid_citations, 0,
            "markers in code never count"
        );
        assert_eq!(assessment.trust.cited_sources, 1);
    }

    #[test]
    fn coverage_counts_invalid_citations_once_per_claim() {
        let catalog = two_entry_catalog();
        let same_claim = assess(&catalog, "The loop is bounded [S99] and [S99].");
        assert_eq!(same_claim.total_claims, 1);
        assert_eq!(same_claim.invalid_citations, 1, "per claim, per id: once");

        let two_claims = assess(
            &catalog,
            "The loop is bounded [S99] and [S99]. Again [S99].",
        );
        assert_eq!(two_claims.total_claims, 2);
        assert_eq!(two_claims.invalid_citations, 2, "once per claim");
    }

    #[test]
    fn coverage_multi_marker_and_case_insensitive() {
        let catalog = two_entry_catalog();
        let assessment = assess(&catalog, "The loop is bounded [S1, s2]. Second claim [s1].");
        assert_eq!(assessment.total_claims, 2);
        assert_eq!(assessment.covered_claims, 2);
        assert_eq!(assessment.invalid_citations, 0);
        assert_eq!(assessment.trust.cited_sources, 2);
        assert_eq!(assessment.trust.coverage, 1.0);
    }

    #[test]
    fn assessment_exposes_counts_for_repair() {
        let catalog = two_entry_catalog();
        let assessment = assess(&catalog, "First [S1]. Second. Third [S99].");
        assert_eq!(assessment.total_claims, 3);
        assert_eq!(assessment.covered_claims, 1);
        assert_eq!(assessment.invalid_citations, 1);
        assert_eq!(assessment.coverage_bp, 333);
        assert_eq!(assessment.trust.coverage, 0.333);
    }
}
