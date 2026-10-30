//! Multi-signal retrieval boost: keyword overlap +
//! entity-link re-ranking layered over vector/time scores.

use ibis_db::value::Value;
use regex::Regex;
use std::sync::OnceLock;

use crate::rows;

pub(crate) const KEYWORD_BOOST_WEIGHT: f64 = 0.15;
pub(crate) const ENTITY_BOOST_WEIGHT: f64 = 0.10;

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
/// the index into the caller's row list.
pub(crate) struct ScoredHit {
    pub text: String,
    pub path: String,
    pub tags: Vec<String>,
    pub score: f64,
    pub payload: usize,
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
    let ql = "SELECT name FROM symbol WHERE string::lowercase(name) IN $names LIMIT 50;";
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
                payload: 0,
            },
            ScoredHit {
                text: "unrelated".into(),
                path: String::new(),
                tags: vec![],
                score: 0.1,
                payload: 1,
            },
        ];
        let kws = vec!["auth".to_string()];
        apply_multi_signal_boost(&mut hits, &kws, &[], &[]);
        assert_eq!(hits[0].text, "auth login");
    }
}
