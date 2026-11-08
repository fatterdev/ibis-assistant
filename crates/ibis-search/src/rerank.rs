//! Post-fusion reranker (A3, ddofds fase 2 parte 2 — spec in
//! `docs/planning/2026-11-01-dailydoseofds-improvement-analysis-l3-punti-di-aggancio.md`).
//!
//! One optional semantic-scoring stage between the multi-signal boost and
//! the final 15-hit cut: the top hits of the fused board (bounded by the
//! candidate window, never widening it) go to a single local-first scoring
//! call (`route_hint = "bulk_local"`, temp 0.0, small output) that returns a
//! 0..1 relevance per excerpt. The final score blends the fused base with
//! the judged relevance (`base_norm*0.4 + llm*0.6` — a blend, not a
//! replacement: one hallucinated perfect score cannot fully erase the
//! ranking signals).
//!
//! Fail-open everywhere: an AI error (`ContextOnly` included), a
//! non-functional provider, a too-small board or an unparsable response
//! keep the pre-rerank order — the reranker can degrade the quality of the
//! order, never break the ask. The as_ref/at-ref path (`memory::askref`)
//! does NOT rerank in v1: its 15-hit cap carries a claim_file floor that a
//! reorder could violate.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use ibis_ai::{AIProvider, CostTracker};
use regex::Regex;

use crate::retrieval_boost::ScoredHit;

/// Reranker mode behind `search.rerank` (`off` | `local` — A3). Default
/// `off`: activation is guided by the A1 eval numbers, not rolled out on
/// faith.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RerankMode {
    /// No semantic rescoring stage (legacy order, zero AI cost).
    #[default]
    Off,
    /// One local-first scoring pass (`bulk_local` → local model, cloud
    /// fallback) over the top candidate-window hits after the fusion.
    Local,
}

impl RerankMode {
    /// Parses the `search.rerank` config value (`None` = unknown).
    pub fn parse(value: &str) -> Option<RerankMode> {
        match value.trim().to_lowercase().as_str() {
            "off" => Some(RerankMode::Off),
            "local" => Some(RerankMode::Local),
            _ => None,
        }
    }

    /// Resolves the config value; unknown non-empty values log a warning and
    /// fall back to the off default.
    pub fn resolve(value: &str) -> RerankMode {
        match RerankMode::parse(value) {
            Some(mode) => mode,
            None => {
                if !value.trim().is_empty() {
                    tracing::warn!("unknown search.rerank value {value:?}; using off");
                }
                RerankMode::Off
            }
        }
    }

    /// Config-facing name.
    pub fn as_str(&self) -> &'static str {
        match self {
            RerankMode::Off => "off",
            RerankMode::Local => "local",
        }
    }
}

/// Cap on the reranked subset (L3 A3: top-30 after the boost), further
/// bounded by the candidate window (`search.candidates` — the reranker
/// consumes the window, it never widens the per-channel fetch).
pub(crate) const RERANK_TOP_HITS: usize = 30;
/// Below this subset size the stage skips (L3 edge case: ≤5 hits).
const RERANK_MIN_HITS: usize = 5;
/// Excerpt length bound (~300 runes per hit, L3 step 1).
const RERANK_EXCERPT_RUNES: usize = 300;
/// `GenerationConfig` for the scoring call (L3 step 1: temp 0.0, ~512 out).
const RERANK_TEMPERATURE: f64 = 0.0;
const RERANK_MAX_OUTPUT_TOKENS: usize = 512;
/// L3 step 1: hybrid setups try the local model first, cloud stays the
/// fallback (never preferred).
const RERANK_ROUTE_HINT: &str = "bulk_local";
/// Blend weights (L3 step 2): the fused base keeps 40%, the judged
/// relevance carries 60%.
const BASE_WEIGHT: f64 = 0.4;
const LLM_WEIGHT: f64 = 0.6;
/// Cost record for the reranking turn (lane-C accounting).
pub(crate) const RERANK_COST_RECORD: &str = "system:search_rerank";

/// Byte-stable instruction block (D13 prompt-stability convention:
/// instructions first, variable context in the middle, query last — cloud
/// prompt caches key on the prefix).
pub(crate) const RERANK_INSTRUCTIONS: &str = "You are a retrieval relevance judge. Rate how relevant each excerpt is to the QUERY at the end.
Reply with ONLY a JSON array, one object per excerpt, exactly this shape:
[{\"index\": 0, \"score\": 0.9}, {\"index\": 1, \"score\": 0.1}]
- \"index\" is the bracketed excerpt number: use it unchanged.
- \"score\" is between 0.0 (irrelevant) and 1.0 (directly answers the query).
- Judge meaning, not keyword overlap; score every listed index exactly once.";

/// Salvage pattern for truncated responses: complete
/// `{"index": n, "score": s}` objects still count.
fn salvage_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\{\s*"index"\s*:\s*(\d+)\s*,\s*"score"\s*:\s*([-0-9.eE+]+)\s*\}"#)
            .expect("static regex")
    })
}

#[derive(serde::Deserialize)]
struct RerankScore {
    index: usize,
    score: f64,
}

/// Strips optional markdown fences.
fn strip_fences(text: &str) -> &str {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let inner = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if inner.is_empty() {
        trimmed
    } else {
        inner
    }
}

/// Parses the scores: full-array JSON first, per-object salvage on failure
/// (a truncated response still yields its complete objects — L3 edge case
/// "parse per-riga tollerante"). Out-of-range indexes drop, duplicate
/// indexes keep the FIRST entry, scores clamp to 0..1, garbage drops.
fn parse_rerank_scores(text: &str, len: usize) -> Vec<(usize, f64)> {
    let body = strip_fences(text);
    let mut out: Vec<(usize, f64)> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    let mut push = |index: usize, score: f64| {
        if index >= len || seen.contains(&index) || score.is_nan() {
            return;
        }
        seen.push(index);
        out.push((index, score.clamp(0.0, 1.0)));
    };
    match serde_json::from_str::<Vec<RerankScore>>(body) {
        Ok(scores) => {
            for s in scores {
                push(s.index, s.score);
            }
        }
        Err(_) => {
            for cap in salvage_pattern().captures_iter(body) {
                let (Ok(index), Ok(score)) = (cap[1].parse::<usize>(), cap[2].parse::<f64>())
                else {
                    continue;
                };
                push(index, score);
            }
        }
    }
    out
}

/// Collapses whitespace noise so multi-line chunks stay readable excerpts.
fn excerpt_text(text: &str) -> String {
    let mut flat = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            if !flat.is_empty() && !flat.ends_with(' ') {
                flat.push(' ');
            }
        } else {
            flat.push(c);
        }
    }
    flat.trim_end().chars().take(RERANK_EXCERPT_RUNES).collect()
}

/// Builds the scoring prompt (instructions → excerpts → query last, D13).
fn build_prompt(hits: &[ScoredHit], query: &str) -> String {
    let mut prompt = String::from(RERANK_INSTRUCTIONS);
    prompt.push_str("\n\nEXCERPTS:");
    for (i, hit) in hits.iter().enumerate() {
        let path = hit.path.trim();
        prompt.push_str(&format!(
            "\n[{i}] {}\n{}",
            if path.is_empty() { "(no path)" } else { path },
            excerpt_text(&hit.text)
        ));
    }
    prompt.push_str("\n\nQUERY: ");
    prompt.push_str(query.trim());
    prompt
}

/// The post-fusion reranker (L3 A3 steps 1–2). Sorts the top
/// `min(board, RERANK_TOP_HITS, candidate_window)` hits in place by
/// `base_norm*0.4 + llm*0.6` (ties break by id). Hits the model did not
/// score keep their pre-rerank relative order: their neutral relevance is
/// their own normalized base, so the subset shares one comparable scale.
/// Rows beyond the subset keep their positions after it. Any failure above
/// leaves `hits` untouched (fail-open).
pub(crate) async fn rerank_hits(
    hits: &mut [ScoredHit],
    query: &str,
    ai: &Arc<dyn AIProvider>,
    tracker: &CostTracker,
    candidate_window: usize,
) {
    let take = hits
        .len()
        .min(RERANK_TOP_HITS)
        .min(candidate_window.max(RERANK_MIN_HITS + 1));
    if take <= RERANK_MIN_HITS {
        tracing::debug!(
            "rerank: {} candidate hit(s), below the {RERANK_MIN_HITS} floor; skipped",
            take
        );
        return;
    }
    if !ai.is_functional().await {
        tracing::debug!("rerank: AI provider not functional; skipped");
        return;
    }
    let prompt = build_prompt(&hits[..take], query);
    let candidate = match ai
        .generate_content(
            &[ibis_ai::types::Content::text(prompt)],
            ibis_ai::types::GenerationConfig {
                temperature: RERANK_TEMPERATURE,
                max_output_tokens: RERANK_MAX_OUTPUT_TOKENS,
                route_hint: RERANK_ROUTE_HINT.to_string(),
                // D12: rerank scores are per-query; never response-cached.
                ..Default::default()
            },
        )
        .await
    {
        Ok(candidate) => candidate,
        Err(e) => {
            tracing::warn!("rerank: scoring call failed; keeping fused order: {e}");
            return;
        }
    };
    if let Some(usage) = &candidate.usage_metadata {
        tracker
            .record_usage(
                RERANK_COST_RECORD,
                usage.prompt_token_count.max(0) as u64,
                usage.candidates_token_count.max(0) as u64,
                usage.cached_tokens(),
            )
            .await;
    }
    let text: String = candidate
        .content
        .parts
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    let scores = parse_rerank_scores(&text, take);
    if scores.is_empty() {
        tracing::warn!("rerank: response yielded no parsable scores; keeping fused order");
        return;
    }
    let llm: HashMap<usize, f64> = scores.into_iter().collect();
    let subset = &mut hits[..take];
    let max = subset
        .iter()
        .map(|h| h.score)
        .fold(f64::NEG_INFINITY, f64::max)
        .max(0.0);
    for (i, hit) in subset.iter_mut().enumerate() {
        let base_norm = if max > 0.0 {
            (hit.score / max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        hit.score = match llm.get(&i) {
            Some(llm_score) => base_norm * BASE_WEIGHT + llm_score * LLM_WEIGHT,
            // Unscored: neutral relevance = own normalized base → the
            // pre-rerank relative order survives on the shared scale.
            None => base_norm,
        };
    }
    subset.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    tracing::debug!(
        "rerank: rescored {take} of {} hits ({} scored by the model)",
        hits.len(),
        llm.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ibis_ai::types::{Candidate, Content, GenerationConfig, Part};
    use ibis_ai::{AiError, CostStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn hit(id: &str, score: f64, text: &str) -> ScoredHit {
        ScoredHit {
            text: text.to_string(),
            path: format!("/src/{id}.rs"),
            tags: Vec::new(),
            score,
            id: id.to_string(),
            payload: 0,
            channel: String::new(),
        }
    }

    fn candidate_with_text(text: &str) -> Candidate {
        Candidate {
            content: Content {
                role: "model".into(),
                parts: vec![Part { text: text.into() }],
            },
            finish_reason: "STOP".into(),
            usage_metadata: None,
        }
    }

    /// Generate-call counting provider with a scripted single response.
    struct ScriptedAI {
        response: std::sync::Mutex<Option<Result<String, String>>>,
        calls: AtomicUsize,
        functional: bool,
    }

    impl ScriptedAI {
        fn responding(response: &str) -> Arc<Self> {
            Arc::new(ScriptedAI {
                response: std::sync::Mutex::new(Some(Ok(response.to_string()))),
                calls: AtomicUsize::new(0),
                functional: true,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(ScriptedAI {
                response: std::sync::Mutex::new(Some(Err("model exploded".to_string()))),
                calls: AtomicUsize::new(0),
                functional: true,
            })
        }

        fn nonfunctional() -> Arc<Self> {
            Arc::new(ScriptedAI {
                response: std::sync::Mutex::new(Some(Ok("[]".to_string()))),
                calls: AtomicUsize::new(0),
                functional: false,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl AIProvider for ScriptedAI {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn is_functional(&self) -> bool {
            self.functional
        }

        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.response.lock().unwrap().take() {
                Some(Ok(text)) => Ok(candidate_with_text(&text)),
                Some(Err(msg)) => Err(AiError::other(msg)),
                None => Err(AiError::other("script exhausted")),
            }
        }
    }

    struct FakeStore(std::sync::Mutex<Vec<(String, u64)>>);

    #[async_trait::async_trait]
    impl CostStore for FakeStore {
        async fn increment_tokens(&self, record_id: &str, total: u64) -> Result<(), AiError> {
            self.0.lock().unwrap().push((record_id.to_string(), total));
            Ok(())
        }
    }

    /// Provider returning one prepared candidate (usage metadata included).
    struct UsageAI(Option<Candidate>);

    #[async_trait::async_trait]
    impl AIProvider for UsageAI {
        fn name(&self) -> &str {
            "usage-ai"
        }
        async fn is_functional(&self) -> bool {
            true
        }
        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.0.clone().ok_or_else(|| AiError::other("no candidate"))
        }
    }

    fn noop_tracker() -> CostTracker {
        CostTracker::new(None)
    }

    fn ids_of(hits: &[ScoredHit]) -> Vec<&str> {
        hits.iter().map(|h| h.id.as_str()).collect()
    }

    fn ids_owned(hits: &[ScoredHit]) -> Vec<String> {
        hits.iter().map(|h| h.id.clone()).collect()
    }

    fn board_with_scores() -> Vec<ScoredHit> {
        // pre-rerank order: mediocre(0.9) > mid(0.7) > unscored-a(0.6) >
        // unscored-b(0.55) > star(0.5) > tail(0.2)
        vec![
            hit("mediocre", 0.9, "weak relevance"),
            hit("mid", 0.7, "partial"),
            hit("unscored-a", 0.6, "no score for me"),
            hit("unscored-b", 0.55, "nor me"),
            hit("star", 0.5, "direct answer"),
            hit("tail", 0.2, "tail"),
        ]
    }

    // --- mode ----------------------------------------------------------------

    #[test]
    fn rerank_mode_parses_config_values() {
        assert_eq!(RerankMode::parse("off"), Some(RerankMode::Off));
        assert_eq!(RerankMode::parse("local"), Some(RerankMode::Local));
        assert_eq!(RerankMode::parse("nonsense"), None);
        assert_eq!(
            RerankMode::resolve("nonsense"),
            RerankMode::Off,
            "unknown values fall back to the off default"
        );
        assert_eq!(RerankMode::default(), RerankMode::Off);
        assert_eq!(RerankMode::Off.as_str(), "off");
        assert_eq!(RerankMode::Local.as_str(), "local");
    }

    // --- parsing -------------------------------------------------------------

    #[test]
    fn parse_accepts_the_full_array() {
        let scores = parse_rerank_scores(
            r#"[{"index": 1, "score": 0.5}, {"index": 0, "score": 1.0}]"#,
            3,
        );
        assert_eq!(scores, vec![(1, 0.5), (0, 1.0)]);
    }

    #[test]
    fn parse_salvages_complete_objects_from_truncated_json() {
        let truncated =
            r#"[{"index": 0, "score": 0.8}, {"index": 1, "score": 0.3}, {"index": 2, "sc"#;
        let scores = parse_rerank_scores(truncated, 4);
        assert_eq!(
            scores,
            vec![(0, 0.8), (1, 0.3)],
            "valid rows survive the cut"
        );
    }

    #[test]
    fn parse_dedups_first_wins_and_drops_out_of_range() {
        let scores = parse_rerank_scores(
            r#"[{"index": 2, "score": 0.1}, {"index": 2, "score": 0.9}, {"index": 9, "score": 1.0}]"#,
            3,
        );
        assert_eq!(
            scores,
            vec![(2, 0.1)],
            "first duplicate wins; index 9 drops"
        );
    }

    #[test]
    fn parse_clamps_and_drops_garbage_scores() {
        let scores = parse_rerank_scores(
            r#"[{"index": 0, "score": 1.7}, {"index": 1, "score": -2}]"#,
            2,
        );
        assert_eq!(scores, vec![(0, 1.0), (1, 0.0)]);
        assert!(parse_rerank_scores("not json at all", 3).is_empty());
        assert!(parse_rerank_scores("", 3).is_empty());
    }

    #[test]
    fn parse_strips_markdown_fences() {
        let fenced = "```json\n[{\"index\": 0, \"score\": 0.4}]\n```";
        assert_eq!(parse_rerank_scores(fenced, 1), vec![(0, 0.4)]);
    }

    // --- excerpt + prompt ----------------------------------------------------

    #[test]
    fn excerpt_collapses_whitespace_and_truncates_by_runes() {
        let long = "a".repeat(500);
        let out = excerpt_text(&format!("line1\n\tline2  {long}"));
        assert_eq!(out.chars().count(), RERANK_EXCERPT_RUNES);
        assert!(!out.contains('\n'));
        assert!(out.starts_with("line1 line2 aaaa"));
    }

    /// D13 prompt stability for the new call-site: the instruction block is
    /// a byte-stable prefix regardless of context and query.
    #[test]
    fn prompt_keeps_the_instruction_prefix_byte_stable() {
        let hits_a = vec![hit("a", 0.9, "auth flow body")];
        let hits_b = vec![hit("b", 0.1, "totally different"), hit("c", 0.3, "x")];
        let pa = build_prompt(&hits_a, "how does auth work?");
        let pb = build_prompt(&hits_b, "unrelated");
        let n = RERANK_INSTRUCTIONS.len();
        assert_eq!(&pa[..n], RERANK_INSTRUCTIONS);
        assert_eq!(&pb[..n], RERANK_INSTRUCTIONS);
        // Context in the middle, query LAST.
        assert!(pa.ends_with("QUERY: how does auth work?"));
        assert!(pb.ends_with("QUERY: unrelated"));
        assert!(pa.contains("[0] /src/a.rs\nauth flow body"));
    }

    #[test]
    fn prompt_indexes_are_positions_not_ids() {
        let hits = vec![hit("zzz", 1.0, "one"), hit("aaa", 0.5, "two")];
        let prompt = build_prompt(&hits, "q");
        assert!(prompt.contains("[0] /src/zzz.rs\none"));
        assert!(prompt.contains("[1] /src/aaa.rs\ntwo"));
    }

    // --- blending ------------------------------------------------------------

    #[test]
    fn blend_weights_follow_the_spec() {
        // base_norm: a=1.0 (max), b=0.5. Scored: a → 0.4*1 + 0.6*1 = 1.0,
        // b → 0.4*0.5 + 0.6*0 = 0.2. Unscored keeps base_norm.
        let max = 0.8f64;
        let base_norm = |s: f64| (s / max).clamp(0.0, 1.0);
        let llm = HashMap::from([(0usize, 1.0f64), (1, 0.0f64)]);
        let final_of = |i: usize, score: f64| match llm.get(&i) {
            Some(l) => base_norm(score) * BASE_WEIGHT + l * LLM_WEIGHT,
            None => base_norm(score),
        };
        assert!((final_of(0, 0.8) - 1.0).abs() < 1e-9);
        assert!((final_of(1, 0.4) - 0.2).abs() < 1e-9);
    }

    // --- rerank_hits ---------------------------------------------------------

    #[tokio::test]
    async fn rescored_hits_reorder_and_unscored_keep_relative_order() {
        let mut hits = board_with_scores();
        // Subset max = 0.9 → base_norm: mediocre 1.0, mid 0.778,
        // unscored-a 0.667, unscored-b 0.611, star 0.556, tail 0.222.
        // Finals: mediocre llm 0.0 → 0.400; star llm 1.0 → 0.822;
        // mid llm 0.5 → 0.611 (= unscored-b's base → id tie-break mid first).
        let ai = ScriptedAI::responding(
            r#"[{"index": 0, "score": 0.0}, {"index": 4, "score": 1.0}, {"index": 1, "score": 0.5}]"#,
        );
        rerank_hits(
            &mut hits,
            "query",
            &(ai.clone() as Arc<dyn AIProvider>),
            &noop_tracker(),
            50,
        )
        .await;
        assert_eq!(ai.calls(), 1);
        // mid's blend (0.611…) and unscored-b's base (0.611…) are a float
        // tie — either order is acceptable, everything else is pinned.
        let ids = ids_of(&hits);
        assert_eq!(ids[0], "star");
        assert_eq!(ids[1], "unscored-a");
        assert!(
            ids[2..4] == ["mid", "unscored-b"][..] || ids[2..4] == ["unscored-b", "mid"][..],
            "tie pair in either order: {ids:?}"
        );
        assert_eq!(ids[4], "mediocre");
        assert_eq!(ids[5], "tail");
    }

    #[tokio::test]
    async fn candidate_window_caps_the_subset() {
        let mut hits: Vec<ScoredHit> = (0..10)
            .map(|i| hit(&format!("h{i}"), 1.0 - i as f64 * 0.1, "text"))
            .collect();
        let ai = ScriptedAI::responding(r#"[{"index": 7, "score": 1.0}]"#);
        // Window 8 → take 8 (window cap, not board length).
        rerank_hits(
            &mut hits,
            "q",
            &(ai.clone() as Arc<dyn AIProvider>),
            &noop_tracker(),
            8,
        )
        .await;
        assert_eq!(ai.calls(), 1);
        // h7 (llm 1.0) → 0.72, which lands it between h2 (0.8) and h3 (0.7).
        assert_eq!(ids_of(&hits[..4]), vec!["h0", "h1", "h2", "h7"]);
        assert_eq!(
            ids_of(&hits[8..]),
            vec!["h8", "h9"],
            "beyond-window rows keep their order"
        );
    }

    #[tokio::test]
    async fn small_boards_skip_without_an_ai_call() {
        for n in [0usize, 3, 5] {
            let mut hits: Vec<ScoredHit> =
                (0..n).map(|i| hit(&format!("h{i}"), 0.5, "t")).collect();
            let before = ids_owned(&hits);
            let ai = ScriptedAI::responding("[]");
            rerank_hits(
                &mut hits,
                "q",
                &(ai.clone() as Arc<dyn AIProvider>),
                &noop_tracker(),
                50,
            )
            .await;
            assert_eq!(ai.calls(), 0, "{n} hits ≤ {RERANK_MIN_HITS} must skip");
            assert_eq!(ids_owned(&hits), before, "order untouched");
        }
    }

    #[tokio::test]
    async fn nonfunctional_provider_skips() {
        let mut hits = board_with_scores();
        let before = ids_owned(&hits);
        let ai = ScriptedAI::nonfunctional();
        rerank_hits(
            &mut hits,
            "q",
            &(ai.clone() as Arc<dyn AIProvider>),
            &noop_tracker(),
            50,
        )
        .await;
        assert_eq!(ai.calls(), 0, "non-functional provider never generates");
        assert_eq!(ids_owned(&hits), before);
    }

    #[tokio::test]
    async fn ai_failure_fails_open() {
        let mut hits = board_with_scores();
        let before = ids_owned(&hits);
        let ai = ScriptedAI::failing();
        rerank_hits(
            &mut hits,
            "q",
            &(ai.clone() as Arc<dyn AIProvider>),
            &noop_tracker(),
            50,
        )
        .await;
        assert_eq!(ai.calls(), 1, "the attempt happens");
        assert_eq!(ids_owned(&hits), before, "order unchanged after the error");
    }

    #[tokio::test]
    async fn unparsable_response_fails_open() {
        let mut hits = board_with_scores();
        let before = ids_owned(&hits);
        let ai = ScriptedAI::responding("I think hit zero is great!");
        rerank_hits(
            &mut hits,
            "q",
            &(ai.clone() as Arc<dyn AIProvider>),
            &noop_tracker(),
            50,
        )
        .await;
        assert_eq!(ai.calls(), 1);
        assert_eq!(ids_owned(&hits), before);
    }

    #[tokio::test]
    async fn usage_is_recorded_on_the_rerank_record() {
        let store = Arc::new(FakeStore(std::sync::Mutex::new(Vec::new())));
        let tracker = CostTracker::new(Some(store.clone() as Arc<dyn CostStore>));
        let mut hits = board_with_scores();
        let mut candidate = candidate_with_text(r#"[{"index": 0, "score": 1.0}]"#);
        candidate.usage_metadata = Some(ibis_ai::types::UsageMetadata {
            prompt_token_count: 120,
            candidates_token_count: 30,
            total_token_count: 150,
            cached_content_token_count: None,
        });
        let ai: Arc<dyn AIProvider> = Arc::new(UsageAI(Some(candidate)));
        rerank_hits(&mut hits, "q", &ai, &tracker, 50).await;
        assert_eq!(
            store.0.lock().unwrap().clone(),
            vec![(RERANK_COST_RECORD.to_string(), 150u64)]
        );
        // Only the leader was scored (llm 1.0 reinforces it); everyone else
        // keeps base_norm → the pre-rerank order survives.
        assert_eq!(
            ids_of(&hits),
            vec![
                "mediocre",
                "mid",
                "unscored-a",
                "unscored-b",
                "star",
                "tail"
            ]
        );
    }

    #[tokio::test]
    async fn zero_base_scores_do_not_divide_by_zero() {
        let mut hits: Vec<ScoredHit> = (0..6).map(|i| hit(&format!("h{i}"), 0.0, "t")).collect();
        let ai =
            ScriptedAI::responding(r#"[{"index": 0, "score": 0.9}, {"index": 5, "score": 0.0}]"#);
        rerank_hits(
            &mut hits,
            "q",
            &(ai.clone() as Arc<dyn AIProvider>),
            &noop_tracker(),
            50,
        )
        .await;
        // max = 0 → base_norm 0 everywhere → finals are pure llm blends.
        // h0 wins (0.54); the zeros tie and fall back to id order.
        assert_eq!(ids_of(&hits), vec!["h0", "h1", "h2", "h3", "h4", "h5"]);
    }
}
