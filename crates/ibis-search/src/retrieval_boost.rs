//! Multi-signal retrieval boost: keyword overlap +
//! entity-link re-ranking layered over vector/time scores.

use ibis_db::value::Value;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::rows;

pub(crate) const KEYWORD_BOOST_WEIGHT: f64 = 0.15;
pub(crate) const ENTITY_BOOST_WEIGHT: f64 = 0.10;
/// Reciprocal Rank Fusion smoothing constant (A2, standard k=60).
pub(crate) const RRF_K: f64 = 60.0;
/// "is current" bump applied to the fused base in RRF mode (A2 step 4:
/// constant epsilon after fusion, no longer a fraction of the cosine score).
pub(crate) const CURRENT_EPSILON: f64 = 0.05;
/// BM25 saturation point of the bounded monotone calibration (F04/D2): a
/// lexical score `s` enters the additive blend as `s / (s + K)`, mapping
/// 0→0 and ∞→1 so raw BM25 units can never dominate cosine/decay scores
/// through their scale alone. Monotone, so within-channel ranking is
/// invariant to any constant rescaling of the raw scores. The value is the
/// eval-pinned one (section 9 of the remediation plan): the score of a
/// "strong" lexical hit saturates around half of the blend scale.
pub(crate) const BM25_SATURATION_K: f64 = 3.0;

/// Bounded monotone calibration for raw BM25/full-text scores (F04/D2).
/// Raw BM25 units are unbounded and entered the additive blend next to
/// calibrated cosine/decay scores; `s/(s+K)` removes the unit dependency
/// while preserving order (monotone) — a strong lexical match lands near
/// 1.0, a weak one near 0.
pub(crate) fn calibrate_lexical_score(raw: f64) -> f64 {
    if raw <= 0.0 {
        return 0.0;
    }
    raw / (raw + BM25_SATURATION_K)
}

/// Channel fusion mode behind `search.fusion` (`additive` | `rrf` — A2).
/// Default stays `additive` until the A1 eval validates RRF on the golden
/// dataset (safe rollout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FusionMode {
    /// Legacy additive blend of per-channel scores (pinned by unit tests).
    #[default]
    Additive,
    /// Reciprocal Rank Fusion over the per-channel ranks.
    Rrf,
}

impl FusionMode {
    /// Parses the `search.fusion` config value (`None` = unknown).
    pub fn parse(value: &str) -> Option<FusionMode> {
        match value.trim().to_lowercase().as_str() {
            "additive" => Some(FusionMode::Additive),
            "rrf" => Some(FusionMode::Rrf),
            _ => None,
        }
    }

    /// Resolves the config value; unknown non-empty values log a warning and
    /// fall back to the additive default.
    pub fn resolve(value: &str) -> FusionMode {
        match FusionMode::parse(value) {
            Some(mode) => mode,
            None => {
                if !value.trim().is_empty() {
                    tracing::warn!("unknown search.fusion value {value:?}; using additive");
                }
                FusionMode::Additive
            }
        }
    }

    /// Config-facing name.
    pub fn as_str(&self) -> &'static str {
        match self {
            FusionMode::Additive => "additive",
            FusionMode::Rrf => "rrf",
        }
    }
}

/// Query expansion mode behind `search.query_expansion` (`off` |
/// `symbol_variant` — A4). Default `symbol_variant`: the second embedding
/// fires only when the linked-symbol lookup resolved at least one symbol,
/// so symbol-free asks keep the single-embedding fast path (zero cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryExpansionMode {
    /// Single embedding of the literal query (legacy path).
    Off,
    /// Second embedding of the symbol-variant query when linked symbols
    /// resolve; vector channels keep per-row `score = max(passes)`.
    #[default]
    SymbolVariant,
}

impl QueryExpansionMode {
    /// Parses the `search.query_expansion` config value (`None` = unknown).
    pub fn parse(value: &str) -> Option<QueryExpansionMode> {
        match value.trim().to_lowercase().as_str() {
            "off" => Some(QueryExpansionMode::Off),
            "symbol_variant" => Some(QueryExpansionMode::SymbolVariant),
            _ => None,
        }
    }

    /// Resolves the config value; unknown non-empty values log a warning and
    /// fall back to the symbol-variant default.
    pub fn resolve(value: &str) -> QueryExpansionMode {
        match QueryExpansionMode::parse(value) {
            Some(mode) => mode,
            None => {
                if !value.trim().is_empty() {
                    tracing::warn!(
                        "unknown search.query_expansion value {value:?}; using symbol_variant"
                    );
                }
                QueryExpansionMode::SymbolVariant
            }
        }
    }

    /// Config-facing name.
    pub fn as_str(&self) -> &'static str {
        match self {
            QueryExpansionMode::Off => "off",
            QueryExpansionMode::SymbolVariant => "symbol_variant",
        }
    }
}

/// Cap on how many linked symbol names the expansion variant may append
/// (A4: deterministic, bounded prompt/vector size — mirrors the 20-name
/// bind limit of `resolve_linked_symbols`).
pub(crate) const MAX_EXPANSION_SYMBOLS: usize = 20;

/// Builds the symbol-variant query list (A4 step 1): `[originale]` or
/// `[originale, variante]` where the variant is the original query with the
/// CamelCase tokens reunited in identifier form plus the linked symbol names
/// appended (case-insensitive dedup). Single entry = fast path (no second
/// embedding): no linked symbols resolved or the variant collapses into the
/// original — the second embedding never fires without symbol signal
/// (L3 A4 edge cases).
pub(crate) fn expand_query(query: &str, linked_symbols: &[String]) -> Vec<String> {
    let primary = query.trim();
    let mut extras: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !linked_symbols.is_empty() {
        // CamelCase tokens of the query reunited: "AskProject" also enters
        // the variant as "askproject" so the embedder sees the identifier
        // as one unit next to its parts.
        for part in token_split_pattern().split(primary) {
            let part = part.trim();
            if part.is_empty() || !looks_like_identifier(part) {
                continue;
            }
            let parts = split_camel_case(part);
            if parts.len() > 1 {
                let reunited = parts.join("").to_lowercase();
                if reunited.len() >= 3 && seen.insert(reunited.clone()) {
                    extras.push(reunited);
                }
            }
        }
        for sym in linked_symbols.iter().take(MAX_EXPANSION_SYMBOLS) {
            let s = sym.trim().to_lowercase();
            if !s.is_empty() && seen.insert(s.clone()) {
                extras.push(s);
            }
        }
    }
    if extras.is_empty() {
        vec![primary.to_string()]
    } else {
        vec![
            primary.to_string(),
            format!("{primary} {}", extras.join(" ")),
        ]
    }
}

fn token_split_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^a-zA-Z0-9_/.\-]+").expect("static regex"))
}

fn path_like_token_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[./\\]").expect("static regex"))
}

/// Breaks identifiers like AskProject / HTTPServer into parts (
/// `splitCamelCase` — no lookaround needed).
pub fn split_camel_case(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let runes: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut start = 0usize;
    for i in 1..runes.len() {
        let prev = runes[i - 1];
        let cur = runes[i];
        let boundary = (prev.is_lowercase() && cur.is_uppercase())
            || (prev.is_uppercase()
                && cur.is_uppercase()
                && i + 1 < runes.len()
                && runes[i + 1].is_lowercase())
            || (prev.is_alphabetic() && cur.is_numeric())
            || (prev.is_numeric() && cur.is_alphabetic());
        if boundary {
            parts.push(runes[start..i].iter().collect());
            start = i;
        }
    }
    parts.push(runes[start..].iter().collect());
    parts
}

/// A generic re-rankable retrieval row; `payload` carries
/// the index into the caller's row list and `id` is the chunk record id
/// (deterministic tie-break in RRF mode). `channel` carries the fusion
/// channel id of the best contribution (F03/D1 diagnostics).
pub(crate) struct ScoredHit {
    pub text: String,
    pub path: String,
    pub tags: Vec<String>,
    pub score: f64,
    pub id: String,
    pub payload: usize,
    pub channel: String,
}

/// Extracts lowercase keyword tokens and light entity candidates
/// (identifiers, CamelCase parts, path-like fragments) from a free-text
/// query.
pub fn tokenize_query(query: &str) -> (Vec<String>, Vec<String>) {
    let mut keywords: Vec<String> = Vec::new();
    let mut entities: Vec<String> = Vec::new();
    let mut seen_kw = std::collections::HashSet::new();
    let mut seen_ent = std::collections::HashSet::new();

    let add = |target: &mut Vec<String>,
               seen: &mut std::collections::HashSet<String>,
               raw: &str,
               min_len: usize| {
        let tok = raw.trim().to_lowercase();
        if tok.len() < min_len {
            return;
        }
        if seen.insert(tok.clone()) {
            target.push(tok);
        }
    };

    for part in token_split_pattern().split(query) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        add(&mut keywords, &mut seen_kw, part, 2);
        if path_like_token_pattern().is_match(part) || looks_like_identifier(part) {
            add(&mut entities, &mut seen_ent, part, 2);
        }
        for camel in split_camel_case(part) {
            add(&mut keywords, &mut seen_kw, &camel, 2);
            if looks_like_identifier(&camel) {
                add(&mut entities, &mut seen_ent, &camel, 3);
            }
        }
    }
    (keywords, entities)
}

/// `looksLikeIdentifier.
pub fn looks_like_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut has_letter = false;
    for r in s.chars() {
        if r.is_alphabetic() {
            has_letter = true;
            continue;
        }
        if r.is_numeric() || matches!(r, '_' | '-' | '/' | '.' | '\\') {
            continue;
        }
        return false;
    }
    has_letter
}

/// Pragmatic BM25 stand-in: term frequency (capped) + coverage → 0..1.
pub fn keyword_overlap_score(keywords: &[String], text: &str) -> f64 {
    if keywords.is_empty() || text.trim().is_empty() {
        return 0.0;
    }
    let lower = text.to_lowercase();
    let mut matched = 0usize;
    let mut freq_sum = 0.0f64;
    for kw in keywords {
        let count = lower.matches(kw.as_str()).count();
        if count == 0 {
            continue;
        }
        matched += 1;
        // Cap per-term frequency so noisy repetition cannot dominate.
        freq_sum += 5.0f64.min(count as f64);
    }
    if matched == 0 {
        return 0.0;
    }
    let coverage = matched as f64 / keywords.len() as f64;
    let tf = freq_sum / (freq_sum + 3.0); // soft saturation
    0.6 * coverage + 0.4 * tf
}

/// 0..1 when query entity tokens (or resolved symbol names) appear in the
/// hit text/path/tags.
pub fn entity_link_score(
    entities: &[String],
    linked_symbols: &[String],
    text: &str,
    path: &str,
    tags: &[String],
) -> f64 {
    let mut candidates: Vec<String> = Vec::with_capacity(entities.len() + linked_symbols.len());
    let mut seen = std::collections::HashSet::new();
    for e in entities.iter().chain(linked_symbols.iter()) {
        let e = e.trim().to_lowercase();
        if e.is_empty() || !seen.insert(e.clone()) {
            continue;
        }
        candidates.push(e);
    }
    if candidates.is_empty() {
        return 0.0;
    }
    let haystack = format!("{} {} {}", text, path, tags.join(" ")).to_lowercase();
    let matched = candidates
        .iter()
        .filter(|e| haystack.contains(e.as_str()))
        .count();
    matched as f64 / candidates.len() as f64
}

/// Blends vector/time base with keyword and entity boosts.
pub(crate) fn combine_retrieval_score(base: f64, keyword: f64, entity: f64) -> f64 {
    let base = base.max(0.0);
    let mut remain = 1.0 - KEYWORD_BOOST_WEIGHT - ENTITY_BOOST_WEIGHT;
    if remain < 0.5 {
        remain = 0.5;
    }
    base * remain + keyword * KEYWORD_BOOST_WEIGHT + entity * ENTITY_BOOST_WEIGHT
}

/// Reciprocal Rank Fusion over per-channel `(payload, rank)` lists (1-based
/// ranks, k = [`RRF_K`]). A chunk present in more than one channel **sums**
/// its per-channel contributions (standard RRF); rank 0 entries are skipped.
pub fn rrf_fuse(channels: &[Vec<(usize, usize)>], k: f64) -> HashMap<usize, f64> {
    let mut fused: HashMap<usize, f64> = HashMap::new();
    for channel in channels {
        for &(payload, rank) in channel {
            if rank == 0 {
                continue;
            }
            *fused.entry(payload).or_insert(0.0) += 1.0 / (k + rank as f64);
        }
    }
    fused
}

/// Theoretical RRF maximum for a chunk ranked 1st in every channel:
/// `num_channels × 1/(k+1)` — the normalization denominator for
/// [`combine_rrf_score`].
pub(crate) fn rrf_max_score(num_channels: usize, k: f64) -> f64 {
    if num_channels == 0 {
        return 1.0;
    }
    num_channels as f64 / (k + 1.0)
}

/// Blends the normalized RRF base with the keyword/entity boosts
/// (`rrf_norm*0.75 + kw*0.15 + ent*0.10` — A2 step 3; the base is clamped to
/// 0..1 so the fused score stays on the same scale as the additive blend).
pub(crate) fn combine_rrf_score(rrf_norm: f64, keyword: f64, entity: f64) -> f64 {
    let base = rrf_norm.clamp(0.0, 1.0);
    base * (1.0 - KEYWORD_BOOST_WEIGHT - ENTITY_BOOST_WEIGHT)
        + keyword * KEYWORD_BOOST_WEIGHT
        + entity * ENTITY_BOOST_WEIGHT
}

/// Looks up symbol names matching query entity tokens (best-effort; entity
/// tokens are bound as `$names`, F-002). Failures return an empty list so
/// retrieval still works.
pub(crate) async fn resolve_linked_symbols(
    db: Option<&std::sync::Arc<dyn ibis_db::Executor>>,
    entities: &[String],
) -> Vec<String> {
    let Some(db) = db else {
        return Vec::new();
    };
    let mut names_bind: Vec<Value> = Vec::new();
    for e in entities {
        if e.len() < 3 {
            continue;
        }
        names_bind.push(Value::from_t(e.as_str()));
        if names_bind.len() >= 20 {
            break;
        }
    }
    if names_bind.is_empty() {
        return Vec::new();
    }
    let ql =
        "SELECT name FROM symbol WHERE string::lowercase(name) IN $names ORDER BY name LIMIT 50;";
    let mut vars = ibis_db::executor::Vars::new();
    vars.insert("names".into(), Value::from_t(names_bind));
    let Ok(outcome) = db.smart_query(ql, vars).await else {
        return Vec::new();
    };
    let Some(rows_values) = rows::normalize_rows(&outcome) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for row in rows::object_rows(&rows_values) {
        let name = rows::scalar_string(rows::field(row, "name"));
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            names.push(trimmed.to_lowercase());
        }
    }
    names
}

/// Applies keyword/entity boosts in place and sorts by descending score
/// (stable).
pub(crate) fn apply_multi_signal_boost(
    hits: &mut [ScoredHit],
    keywords: &[String],
    entities: &[String],
    linked_symbols: &[String],
) {
    for hit in hits.iter_mut() {
        let kw = keyword_overlap_score(keywords, &format!("{} {}", hit.text, hit.path));
        let ent = entity_link_score(entities, linked_symbols, &hit.text, &hit.path, &hit.tags);
        hit.score = combine_retrieval_score(hit.score, kw, ent);
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// RRF-mode ranking: `hits` carry the normalized fused base in `score`
/// (["CURRENT_EPSILON"] included); keyword/entity boosts apply on top and
/// the sort is descending with a deterministic id tie-break (A2).
pub(crate) fn apply_rrf_boost(
    hits: &mut [ScoredHit],
    keywords: &[String],
    entities: &[String],
    linked_symbols: &[String],
) {
    for hit in hits.iter_mut() {
        let kw = keyword_overlap_score(keywords, &format!("{} {}", hit.text, hit.path));
        let ent = entity_link_score(entities, linked_symbols, &hit.text, &hit.path, &hit.tags);
        hit.score = combine_rrf_score(hit.score, kw, ent);
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_camel_case_boundaries() {
        assert_eq!(split_camel_case("AskProject"), vec!["Ask", "Project"]);
        assert_eq!(split_camel_case("HTTPServer"), vec!["HTTP", "Server"]);
        assert_eq!(split_camel_case("get2Items"), vec!["get", "2", "Items"]);
        assert!(split_camel_case("").is_empty());
    }

    #[test]
    fn tokenize_query_extracts_keywords_and_entities() {
        let (keywords, entities) =
            tokenize_query("Search the auth_service.go handler for LoginFlow");
        assert!(keywords.contains(&"auth_service.go".to_string()));
        assert!(entities.contains(&"auth_service.go".to_string()));
        assert!(
            keywords.contains(&"login".to_string()) || keywords.contains(&"loginflow".to_string())
        );
        assert!(entities.contains(&"loginflow".to_string()));
    }

    #[test]
    fn keyword_overlap_caps_and_saturates() {
        let kws = vec!["auth".to_string()];
        assert_eq!(keyword_overlap_score(&kws, ""), 0.0);
        let hit = "auth auth auth auth auth auth auth".to_string();
        let score = keyword_overlap_score(&kws, &hit);
        assert!(score > 0.0 && score <= 1.0);
        let miss = keyword_overlap_score(&kws, "nothing here");
        assert_eq!(miss, 0.0);
    }

    #[test]
    fn entity_link_matches_text_path_tags() {
        let ents = vec!["parse".to_string()];
        assert!(entity_link_score(&ents, &[], "the parser runs", "", &[]) > 0.0);
        assert!(entity_link_score(&ents, &[], "nothing", "x", &[]) == 0.0);
        assert!(entity_link_score(&ents, &[], "", "src/parse.rs", &[]) > 0.0);
    }

    #[test]
    fn combine_keeps_base_share() {
        let base = 1.0;
        let blended = combine_retrieval_score(base, 0.0, 0.0);
        assert!((blended - 0.75).abs() < 1e-9);
        let full = combine_retrieval_score(1.0, 1.0, 1.0);
        assert!((full - 1.0).abs() < 1e-9);
    }

    #[test]
    fn boost_sorts_descending() {
        let mut hits = vec![
            ScoredHit {
                text: "auth login".into(),
                path: String::new(),
                tags: vec![],
                score: 0.4,
                id: "hit:1".into(),
                payload: 0,
                channel: String::new(),
            },
            ScoredHit {
                text: "unrelated".into(),
                path: String::new(),
                tags: vec![],
                score: 0.1,
                id: "hit:2".into(),
                payload: 1,
                channel: String::new(),
            },
        ];
        let kws = vec!["auth".to_string()];
        apply_multi_signal_boost(&mut hits, &kws, &[], &[]);
        assert_eq!(hits[0].text, "auth login");
    }

    #[test]
    fn rrf_fuse_sums_cross_channel_contributions() {
        let channels = vec![vec![(0usize, 1usize), (1, 2)], vec![(1, 1)]];
        let fused = rrf_fuse(&channels, RRF_K);
        assert!((fused[&0] - 1.0 / (RRF_K + 1.0)).abs() < 1e-12);
        assert!((fused[&1] - (1.0 / (RRF_K + 2.0) + 1.0 / (RRF_K + 1.0))).abs() < 1e-12);
        assert!(fused[&1] > fused[&0], "cross-channel sum outranks single");
    }

    #[test]
    fn rrf_fuse_skips_empty_and_zero_rank() {
        let channels = vec![Vec::new(), vec![(3usize, 0usize)], vec![(3, 2)]];
        let fused = rrf_fuse(&channels, RRF_K);
        assert_eq!(fused.len(), 1);
        assert!((fused[&3] - 1.0 / (RRF_K + 2.0)).abs() < 1e-12);
    }

    #[test]
    fn rrf_k_flattens_rank_gaps() {
        let one_channel = vec![vec![(0usize, 1usize), (1, 2)]];
        let gap = |k: f64| {
            let fused = rrf_fuse(&one_channel, k);
            fused[&0] - fused[&1]
        };
        assert!(gap(RRF_K) < gap(1.0), "larger k flattens rank differences");
    }

    #[test]
    fn rrf_max_score_theoretical() {
        assert!((rrf_max_score(4, RRF_K) - 4.0 / 61.0).abs() < 1e-12);
        assert!((rrf_max_score(0, RRF_K) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn combine_rrf_weights_and_clamp() {
        assert!((combine_rrf_score(1.0, 0.0, 0.0) - 0.75).abs() < 1e-9);
        assert!((combine_rrf_score(1.0, 1.0, 1.0) - 1.0).abs() < 1e-9);
        assert!(
            (combine_rrf_score(1.2, 0.0, 0.0) - 0.75).abs() < 1e-9,
            "base clamped to 1"
        );
        assert!(
            (combine_rrf_score(-0.1, 0.0, 0.0) - 0.0).abs() < 1e-9,
            "base clamped to 0"
        );
    }

    #[test]
    fn calibrate_lexical_is_bounded_monotone_and_scale_order_invariant() {
        // F04/D2 properties.
        // Bounded: no raw value can push past 1.0.
        for raw in [0.0f64, 0.3, 1.0, 3.0, 50.0, 1000.0] {
            let calibrated = calibrate_lexical_score(raw);
            assert!(
                (0.0..=1.0).contains(&calibrated),
                "calibrate({raw}) = {calibrated} out of bounds"
            );
        }
        assert_eq!(calibrate_lexical_score(0.0), 0.0);
        assert_eq!(calibrate_lexical_score(-5.0), 0.0);

        // Monotone: a stronger raw match never calibrates lower.
        let mut prev = -1.0;
        for raw in [0.01, 0.1, 1.0, 5.0, 20.0, 200.0] {
            let c = calibrate_lexical_score(raw);
            assert!(c > prev, "calibrate must be monotone in raw: {raw}");
            prev = c;
        }

        // Scale-order invariance: multiplying ALL raw scores of a channel by
        // a constant keeps their calibrated ORDER (the within-channel
        // ranking does not depend on the BM25 unit).
        let raws = [0.2f64, 1.0, 4.0, 30.0];
        let order = |factor: f64| -> Vec<usize> {
            let mut calibrated: Vec<(usize, f64)> = raws
                .iter()
                .enumerate()
                .map(|(i, r)| (i, calibrate_lexical_score(r * factor)))
                .collect();
            calibrated.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            calibrated.into_iter().map(|(i, _)| i).collect()
        };
        assert_eq!(order(1.0), order(7.5), "order invariant to constant scale");

        // A "strong" lexical hit saturates near the top of the blend scale;
        // a weak one stays low.
        assert!(calibrate_lexical_score(50.0) > 0.9);
        assert!(calibrate_lexical_score(0.3) < 0.2);
    }

    #[test]
    fn rrf_boost_sorts_with_id_tiebreak() {
        let mut hits = vec![
            ScoredHit {
                text: "beta".into(),
                path: String::new(),
                tags: vec![],
                score: 0.5,
                id: "hit:b".into(),
                payload: 0,
                channel: String::new(),
            },
            ScoredHit {
                text: "alpha".into(),
                path: String::new(),
                tags: vec![],
                score: 0.5,
                id: "hit:a".into(),
                payload: 1,
                channel: String::new(),
            },
        ];
        apply_rrf_boost(&mut hits, &[], &[], &[]);
        assert_eq!(hits[0].id, "hit:a", "equal score breaks tie by id");
    }

    #[test]
    fn rrf_boost_keyword_still_promotes() {
        let mut hits = vec![
            ScoredHit {
                text: "unrelated".into(),
                path: String::new(),
                tags: vec![],
                score: 0.5,
                id: "hit:a".into(),
                payload: 0,
                channel: String::new(),
            },
            ScoredHit {
                text: "auth flow".into(),
                path: String::new(),
                tags: vec![],
                score: 0.4,
                id: "hit:b".into(),
                payload: 1,
                channel: String::new(),
            },
        ];
        let kws = vec!["auth".to_string()];
        apply_rrf_boost(&mut hits, &kws, &[], &[]);
        assert_eq!(hits[0].id, "hit:b", "keyword boost promotes over base");
    }

    // --- A4: query expansion -------------------------------------------------

    #[test]
    fn expand_query_appends_reunited_camel_tokens_and_symbols() {
        let exp = expand_query(
            "how does the PriceCalculator work",
            &["pricecalculatorservice".to_string()],
        );
        assert_eq!(exp.len(), 2, "primary + variant");
        let secondary = &exp[1];
        assert!(
            secondary.starts_with("how does the PriceCalculator work"),
            "primary query stays literal: {secondary}"
        );
        // CamelCase tokens reunited in identifier form + linked symbols.
        assert!(secondary.contains("pricecalculator"));
        assert!(secondary.contains("pricecalculatorservice"));
    }

    #[test]
    fn expand_query_dedups_reunited_token_against_symbol() {
        // "AskProject" reunites to "askproject"; a linked symbol with the same
        // normalized name must not be appended twice.
        let exp = expand_query("AskProject flow", &["askproject".to_string()]);
        assert_eq!(exp.len(), 2, "variant present");
        assert_eq!(exp[1].matches("askproject").count(), 1, "dedup: {}", exp[1]);
    }

    #[test]
    fn expand_query_fast_path_without_symbols() {
        // L3 A4 edge case: empty linked symbols → no second embedding.
        let exp = expand_query("how does AskProject work", &[]);
        assert_eq!(exp.len(), 1, "single entry = fast path");
        assert_eq!(exp[0], "how does AskProject work");
    }

    #[test]
    fn expand_query_trims_primary_and_keeps_it_literal() {
        let exp = expand_query("  trim me  ", &["sym".to_string()]);
        assert_eq!(exp[0], "trim me");
        assert_eq!(exp.len(), 2, "variant present");
        assert!(exp[1].starts_with("trim me "), "{}", exp[1]);
    }

    #[test]
    fn expand_query_caps_appended_symbols() {
        let symbols: Vec<String> = (0..40).map(|i| format!("symbol{i}")).collect();
        let exp = expand_query("q", &symbols);
        assert_eq!(exp.len(), 2, "variant present");
        assert_eq!(
            exp[1].matches("symbol").count(),
            MAX_EXPANSION_SYMBOLS,
            "bounded by MAX_EXPANSION_SYMBOLS: {}",
            exp[1]
        );
    }

    #[test]
    fn query_expansion_mode_parses_config_values() {
        assert_eq!(
            QueryExpansionMode::parse("off"),
            Some(QueryExpansionMode::Off)
        );
        assert_eq!(
            QueryExpansionMode::parse("symbol_variant"),
            Some(QueryExpansionMode::SymbolVariant)
        );
        assert_eq!(QueryExpansionMode::parse("nonsense"), None);
        assert_eq!(
            QueryExpansionMode::resolve("nonsense"),
            QueryExpansionMode::SymbolVariant,
            "unknown values fall back to the default"
        );
        assert_eq!(
            QueryExpansionMode::default(),
            QueryExpansionMode::SymbolVariant
        );
        assert_eq!(QueryExpansionMode::Off.as_str(), "off");
        assert_eq!(QueryExpansionMode::SymbolVariant.as_str(), "symbol_variant");
    }
}
