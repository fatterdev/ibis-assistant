//! Agentic ReAct loop: multi-step SEARCH / FINAL ANSWER
//! protocol over the hybrid ask. Parsing layers are pinned by the original test
//! estate (quoted multiline extraction, line-based fallback, chatter
//! stripping, empty-query nudge, no-action nudge, search-failure
//! continuation).
//!
//! E15 (producer side): observed sources are collected in first-observation
//! order with stable citation ids from [`crate::agentic_trust`], the
//! observation prefixes citable lines with `[S<n>] `, and every Ok
//! completion path carries the deterministic citations + trust assessment.

use regex::Regex;
use std::sync::OnceLock;

use ibis_ai::types::{Candidate, Content, Part};

use crate::agentic_trust::{
    assess, citation_key, AgenticCitation, AgenticTrust, CitationCatalog, ObserveOutcome,
};
use crate::error::{Result, SearchError};
use crate::search::{SearchResult, SearchService};

/// The outcome of an agentic search.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgenticResult {
    pub answer: String,
    pub sources: Vec<SearchResult>,
    #[serde(rename = "reasoning_steps")]
    pub steps: Vec<String>,

    // E15 — producer.
    #[serde(default)]
    pub citations: Vec<AgenticCitation>,
    #[serde(default)]
    pub trust: AgenticTrust,

    // E14 — loop flags (defaults keep the payload backwards-compatible).
    #[serde(default)]
    pub budget_exhausted: bool,
    #[serde(default)]
    pub completion_checked: bool,
    #[serde(default)]
    pub repair_turns: u8,
    #[serde(default)]
    pub invalid_citations: usize,
    #[serde(default)]
    pub search_unavailable: bool,
    #[serde(default)]
    pub stop_reason: AgenticStopReason,
}

/// Why the ReAct loop stopped (E14 owns the policy; the default is the
/// plain accepted `FINAL ANSWER`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgenticStopReason {
    #[default]
    FinalAnswer,
    MaxSteps,
    BudgetExhausted,
    NoProgress,
    CompletionLowCoverage,
    AiError,
    ContextOnly,
    EmptyQuery,
}

fn re_final_answer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("(?i)FINAL ANSWER:").expect("static regex"))
}

fn re_search() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("(?i)SEARCH:").expect("static regex"))
}

/// The Senior Software Engineer agent system prompt (`SystemPrompt,
/// byte-identical contract).
pub const SYSTEM_PROMPT: &str = "You are a Senior Software Engineer Agent.
Your goal is to answer questions about the codebase using the provided SEARCH tool. You also have access to the codebase's underlying execution flow and structural dependencies via explicit graph traversal tools (like AnalyzeBlastRadius).

PROTOCOL:
1. THOUGHT: Explain your reasoning. What do you need to know?
2. ACTION: If you need information, output \"SEARCH: \\\"<query>\\\"\". Always use double quotes for the query. You can also explicitly state your desire to explore call graphs or dead code if you are looking at specific functions.
3. OBSERVATION: I will provide the search results. Note that the graph model can link across languages (e.g. JS -> C# -> SQL). Pay close attention to cross-language edges if you are investigating execution paths.
4. REPEAT: You can search multiple times if needed.
5. FINAL ANSWER: When you have enough information, output \"FINAL ANSWER: <your answer>\".

GUIDELINES:
- Be precise.
- If the search results are not relevant, try a different query.
- If you cannot find the answer, admit it.
- Cite the file paths in your answer.
- Trace execution flows logically when they span multiple languages.
- Temporal contract: file_chunk with is_current=true is HEAD (\"how the code is now\"). commit_chunk is historical change meaning (\"why/when it changed\") — never treat it as current source. memory hits are current team truth only; ignore superseded/stale if somehow labeled. Prefer commit_chunk for change forensics.";

/// E14.5 — citation instructions appended once after the byte-stable
/// [`SYSTEM_PROMPT`] prefix. The text is the L3 spec §E14.5 block verbatim;
/// the two constants together form the fixed D13 prompt prefix.
pub const CITATION_PROTOCOL: &str = "CITATION PROTOCOL:
- Every citable OBSERVATION line starts with its id in square brackets, e.g. [S1].
- In FINAL ANSWER, support each factual claim with one or more ids in square brackets, e.g. \"The retry loop is bounded by MAX_RETRIES [S2].\"
- Use only ids that appear in OBSERVATION. Never invent or renumber ids.
- A claim without a citation is treated as unsupported by the completion check.
- If no evidence supports a claim, say so explicitly instead of citing an unrelated source.";

const AGENTIC_MAX_STEPS: usize = 5;
/// D10 per-task routing: the ReAct ask is the heaviest local task (tier L);
/// one provider is pinned for the whole loop, so the model never switches
/// mid-task.
const AGENTIC_ROUTE_HINT: &str = "local:L";

/// D13/E14.5 prompt assembly. Order is contractual:
/// `SYSTEM_PROMPT` → `\n\n` → `CITATION_PROTOCOL` (the fixed, byte-stable
/// prefix) → optional `TOPOLOGICAL CONTEXT` (variable branch context) →
/// `\n\nQuestion: {query}` (always last). No timestamp or request id ever
/// enters the prefix.
fn build_agentic_prompt(query: &str, branch_context: &str) -> String {
    let mut prompt = String::with_capacity(
        SYSTEM_PROMPT.len() + CITATION_PROTOCOL.len() + branch_context.len() + query.len() + 128,
    );
    prompt.push_str(SYSTEM_PROMPT);
    prompt.push_str("\n\n");
    prompt.push_str(CITATION_PROTOCOL);
    if !branch_context.is_empty() {
        prompt.push_str(&format!(
            "\n\nTOPOLOGICAL CONTEXT:\nYou are currently operating on branch/commit: {branch_context}. Keep in mind that search results might come from different parallel branches. Use this context to provide cross-branch insights when useful."
        ));
    }
    prompt.push_str(&format!("\n\nQuestion: {query}"));
    prompt
}

impl SearchService {
    /// Performs the multi-step ReAct search.
    pub async fn ask_project_agentic(
        &self,
        query: &str,
        branch_context: &str,
    ) -> Result<AgenticResult> {
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::other("AI provider not configured"))?;
        let system_prompt = build_agentic_prompt(query, branch_context);

        let mut history: Vec<Content> = vec![Content {
            role: "user".to_string(),
            parts: vec![Part {
                text: system_prompt,
            }],
        }];

        let mut result = AgenticResult::default();
        let mut catalog = CitationCatalog::default();
        let mut observed: Vec<ObservedSource> = Vec::new();
        let mut observed_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        // E14.5/§E14.3: a FINAL ANSWER is the only normal loop exit; falling
        // out of the step budget is reported as `MaxSteps`.
        let mut final_answer_observed = false;

        for i in 0..AGENTIC_MAX_STEPS {
            let candidate: Candidate = ai
                .generate_content(
                    &history,
                    ibis_ai::types::GenerationConfig {
                        temperature: 0.2,
                        max_output_tokens: 2000,
                        route_hint: AGENTIC_ROUTE_HINT.to_string(),
                        // D12: agentic ask is never response-cached.
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| SearchError::other(format!("AI generation failed: {e}")))?;

            if let Some(usage) = &candidate.usage_metadata {
                if self.db.is_some() {
                    crate::search::cost_tracker(self)
                        .record_usage(
                            "system:search_agentic",
                            usage.prompt_token_count.max(0) as u64,
                            usage.candidates_token_count.max(0) as u64,
                            usage.cached_tokens(),
                        )
                        .await;
                }
            }

            let response: String = candidate
                .content
                .parts
                .iter()
                .map(|p| p.text.as_str())
                .collect();

            result.steps.push(response.clone());

            // Add model response to history (ensure role is model).
            let mut model_content = candidate.content.clone();
            if model_content.role.is_empty() {
                model_content.role = "model".to_string();
            }
            history.push(model_content);

            // Parse response (case-insensitive; priority to first occurrence).
            let loc_final = re_final_answer().find(&response);
            let loc_search = re_search().find(&response);

            let action = match (loc_final.as_ref(), loc_search.as_ref()) {
                (Some(f), Some(s)) => {
                    if f.start() < s.start() {
                        "final"
                    } else {
                        "search"
                    }
                }
                (Some(_), None) => "final",
                (None, Some(_)) => "search",
                (None, None) => "none",
            };

            if action == "final" {
                let answer_part = &response[loc_final.unwrap().end()..];
                result.answer = answer_part.trim().to_string();
                final_answer_observed = true;
                break;
            } else if action == "search" {
                let search_loc = loc_search.unwrap();
                let raw_rest = &response[search_loc.end()..];
                let trimmed_rest = raw_rest.trim();

                let mut search_query = String::new();
                let mut quoted_extracted = false;
                // 1. Quotes at the start (supports multiline).
                if trimmed_rest.starts_with('"') || trimmed_rest.starts_with('\'') {
                    let quote = &trimmed_rest[0..1];
                    if let Some(end_quote) = trimmed_rest[1..].find(quote) {
                        search_query = trimmed_rest[1..1 + end_quote].to_string();
                        quoted_extracted = true;
                    }
                }

                if !quoted_extracted && search_query.is_empty() {
                    // Fallback to line-based extraction when not quoted
                    // properly.
                    let raw_query = match raw_rest.find('\n') {
                        Some(line_end) => raw_rest[..line_end].trim(),
                        None => raw_rest.trim(),
                    };
                    // Stop at FINAL ANSWER if present in the same line.
                    let mut raw_query = raw_query.to_string();
                    if let Some(loc) = re_final_answer().find(&raw_query) {
                        raw_query = raw_query[..loc.start()].trim().to_string();
                    }
                    // Mismatched quote: strip the leading quote.
                    if raw_query.starts_with('"') || raw_query.starts_with('\'') {
                        let quote = &raw_query[0..1];
                        search_query = raw_query
                            .strip_prefix(quote)
                            .unwrap_or(&raw_query)
                            .to_string();
                    } else {
                        // Sentence boundary (. ) strips chatter.
                        let mut cleaned = raw_query.clone();
                        if let Some(idx) = cleaned.find(". ") {
                            cleaned = cleaned[..idx].to_string();
                        }
                        search_query = cleaned.trim_end_matches('.').to_string();
                    }
                }

                let search_query = search_query.trim().to_string();

                if search_query.is_empty() {
                    tracing::info!("Agentic Search Step {}: Skipping empty search query", i + 1);
                    history.push(Content {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: "OBSERVATION: Search query was empty. Please provide a valid search term."
                                .to_string(),
                        }],
                    });
                    continue;
                }

                tracing::info!(
                    "Agentic Search Step {}: Searching for '{}'",
                    i + 1,
                    search_query
                );

                // Execute search.
                match self
                    .ask_project(&crate::ActorContext::LOCAL, &search_query)
                    .await
                {
                    Ok(search_results) => {
                        let mut step_observed: Vec<ObservedSource> = Vec::new();
                        for r in search_results {
                            match catalog.observe(&r) {
                                ObserveOutcome::Skip => {}
                                ObserveOutcome::Uncited => {
                                    push_observed_once(
                                        &mut observed,
                                        &mut observed_keys,
                                        None,
                                        r,
                                        &mut step_observed,
                                    );
                                }
                                ObserveOutcome::Cited(id) => {
                                    push_observed_once(
                                        &mut observed,
                                        &mut observed_keys,
                                        Some(id),
                                        r,
                                        &mut step_observed,
                                    );
                                }
                            }
                        }
                        let observation = format_observation(&step_observed);
                        history.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part { text: observation }],
                        });
                    }
                    Err(err) => {
                        history.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part {
                                text: format!("OBSERVATION: Search failed: {err}"),
                            }],
                        });
                        continue;
                    }
                }
            } else {
                // No explicit action, nudge.
                history.push(Content {
                    role: "user".to_string(),
                    parts: vec![Part {
                        text: "OBSERVATION: Please continue. Output SEARCH or FINAL ANSWER."
                            .to_string(),
                    }],
                });
            }
        }

        // E14.5/§E14.3: the loop only ends early on FINAL ANSWER. Without
        // one, the step budget is what stopped the ask — report it instead of
        // pretending the fallback answer was a model final answer. The
        // fallback answer and its deterministic trust assessment are kept.
        result.stop_reason = if final_answer_observed {
            AgenticStopReason::FinalAnswer
        } else {
            AgenticStopReason::MaxSteps
        };

        // Flatten sources in first-observation order (deduped by the catalog
        // key): the `Vec<SearchResult>` shape is unchanged, only the order
        // becomes deterministic (E15 stable citation ids).
        result.sources = observed.into_iter().map(|o| o.result).collect();

        if result.answer.is_empty() {
            result.answer =
                "I could not find a definitive answer after multiple steps. Please try a more specific question."
                    .to_string();
        }

        // E15: deterministic assessment on every Ok completion path — the
        // accepted FINAL ANSWER and the max-steps fallback answer alike.
        let assessment = assess(&catalog, &result.answer);
        result.citations = catalog.entries().to_vec();
        result.trust = assessment.trust;
        result.invalid_citations = assessment.invalid_citations;

        Ok(result)
    }
}

/// One observed source inside the loop (E15): `citation_id` is `Some` for
/// citable sources, `None` beyond the citation cap. Unidentifiable hits
/// (`ObserveOutcome::Skip`) never become an `ObservedSource`.
struct ObservedSource {
    citation_id: Option<String>,
    result: SearchResult,
}

/// Records one observed source: the accumulated `observed` list keeps the
/// first snapshot per catalog key, while `step_observed` feeds this turn's
/// observation block (every Cited/Uncited hit, cap or not).
fn push_observed_once(
    observed: &mut Vec<ObservedSource>,
    observed_keys: &mut std::collections::HashSet<String>,
    citation_id: Option<String>,
    result: SearchResult,
    step_observed: &mut Vec<ObservedSource>,
) {
    if let Some(key) = citation_key(&result) {
        if observed_keys.insert(key) {
            observed.push(ObservedSource {
                citation_id: citation_id.clone(),
                result: result.clone(),
            });
        }
    }
    step_observed.push(ObservedSource {
        citation_id,
        result,
    });
}

/// Formats the OBSERVATION block from the sources observed in this step
/// (the inline `obs` builder). Citable lines are prefixed `[S<n>] `; content
/// is truncated to 2000 runes.
fn format_observation(results: &[ObservedSource]) -> String {
    let mut obs = String::from("OBSERVATION: Found the following evidence:\n");
    if results.is_empty() {
        obs.push_str("No results found.\n");
    }
    for o in results {
        let r = &o.result;
        let mut content_snippet = r.content.clone();
        let runes: Vec<char> = content_snippet.chars().collect();
        if runes.len() > 2000 {
            content_snippet = runes[..2000].iter().collect::<String>() + "...(truncated)";
        }
        let citation_prefix = match &o.citation_id {
            Some(id) => format!("[{id}] "),
            None => String::new(),
        };
        obs.push_str(&format!(
            "{citation_prefix}- {} (Score: {:.2}, source_kind={}, is_current={}",
            observation_label(r),
            r.score,
            empty_default(&r.source_kind, "unknown"),
            r.is_current,
        ));
        if !r.status.is_empty() {
            obs.push_str(&format!(", status={}", r.status));
        }
        if !r.commit_hash.is_empty() {
            obs.push_str(&format!(", commit={}", r.commit_hash));
        }
        obs.push_str(&format!(")\nContext: {content_snippet}\n"));

        if !r.context.related_issues.is_empty() {
            obs.push_str("  Related Issues:\n");
            for issue in &r.context.related_issues {
                obs.push_str(&format!(
                    "    - [{}] {} (Status: {})\n",
                    issue.id, issue.subject, issue.status
                ));
            }
        }
        if !r.context.commits.is_empty() {
            obs.push_str("  Recent Commits:\n");
            for commit in &r.context.commits {
                let hash_short: String = commit.hash.chars().take(7).collect();
                obs.push_str(&format!(
                    "    - [{}] {} (by {})\n",
                    hash_short, commit.message, commit.author
                ));
                if !commit.meaning.is_empty() {
                    obs.push_str(&format!(
                        "      Meaning: {}\n",
                        crate::commit_meaning::truncate_commit_meaning(&commit.meaning)
                    ));
                }
            }
        }
        if !r.context.expert_authors.is_empty() {
            obs.push_str(&format!(
                "  Experts: {}\n",
                r.context.expert_authors.join(", ")
            ));
        }
        obs.push('\n');
    }
    obs
}

/// `observationLabel.
pub(crate) fn observation_label(r: &SearchResult) -> String {
    match r.source_kind.as_str() {
        "commit_chunk" => {
            if !r.commit_hash.is_empty() {
                format!("Commit: {}", r.commit_hash)
            } else {
                format!("Commit: {}", r.id)
            }
        }
        "memory" => format!("Memory: {}", r.id),
        "reasoning" => format!("Reasoning: {}", r.id),
        _ => {
            if !r.path.is_empty() {
                format!("File: {}", r.path)
            } else {
                format!("File: {}", r.id)
            }
        }
    }
}

/// `emptyDefault.
pub(crate) fn empty_default(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic_trust::TrustLevel;

    fn search_result(id: &str, path: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            path: path.to_string(),
            source_kind: "file_chunk".to_string(),
            score: 0.91,
            is_current: true,
            content: "fn a() {}".to_string(),
            ..Default::default()
        }
    }

    /// D10 contract: the agentic ReAct ask declares the tier-L local route.
    #[test]
    fn agentic_route_hint_is_local_l() {
        assert_eq!(AGENTIC_ROUTE_HINT, "local:L");
    }

    /// E15 payload contract: the additive fields are present with stable
    /// wire names; `answer`/`sources`/`reasoning_steps` keep their shape.
    #[test]
    fn payload_shape_is_additive_and_stable() {
        let result = AgenticResult {
            answer: "The retry loop is bounded [S1].".to_string(),
            sources: vec![search_result("file_chunk:a", "src/a.rs")],
            steps: vec!["FINAL ANSWER: ...".to_string()],
            citations: vec![AgenticCitation {
                citation_id: "S1".to_string(),
                source_id: "file_chunk:a".to_string(),
                source_kind: "file_chunk".to_string(),
                path: "src/a.rs".to_string(),
                commit_hash: String::new(),
                score: 0.91,
                excerpt: "fn a() {}".to_string(),
            }],
            trust: AgenticTrust {
                score: 0.9,
                level: TrustLevel::High,
                coverage: 1.0,
                weak_context: false,
                cited_sources: 1,
                observed_sources: 1,
            },
            budget_exhausted: false,
            completion_checked: true,
            repair_turns: 0,
            invalid_citations: 0,
            search_unavailable: false,
            stop_reason: AgenticStopReason::FinalAnswer,
        };
        let value = serde_json::to_value(&result).expect("serializes");
        assert_eq!(value["answer"], "The retry loop is bounded [S1].");
        assert!(value["sources"].is_array());
        assert!(value["reasoning_steps"].is_array());
        assert!(value["citations"].is_array());
        assert_eq!(value["citations"][0]["citation_id"], "S1");
        assert_eq!(value["trust"]["level"], "high");
        assert_eq!(value["trust"]["score"], 0.9);
        assert_eq!(value["trust"]["coverage"], 1.0);
        assert_eq!(value["trust"]["weak_context"], false);
        assert_eq!(value["trust"]["cited_sources"], 1);
        assert_eq!(value["trust"]["observed_sources"], 1);
        assert_eq!(value["budget_exhausted"], false);
        assert_eq!(value["completion_checked"], true);
        assert_eq!(value["repair_turns"], 0);
        assert_eq!(value["invalid_citations"], 0);
        assert_eq!(value["search_unavailable"], false);
        assert_eq!(value["stop_reason"], "final_answer");
    }

    /// A pre-E15 payload decodes with the documented defaults: empty
    /// citations, the honest low/weak trust state, `final_answer`.
    #[test]
    fn old_agentic_payload_deserializes() {
        let old: AgenticResult =
            serde_json::from_str(r#"{"answer":"a","sources":[],"reasoning_steps":[]}"#)
                .expect("old payload decodes");
        assert_eq!(old.answer, "a");
        assert!(old.sources.is_empty());
        assert!(old.citations.is_empty());
        assert_eq!(old.trust, AgenticTrust::default());
        assert_eq!(old.trust.score, 0.0);
        assert_eq!(old.trust.coverage, 0.0);
        assert_eq!(old.trust.level, TrustLevel::Low);
        assert!(old.trust.weak_context);
        assert_eq!(old.trust.cited_sources, 0);
        assert_eq!(old.trust.observed_sources, 0);
        assert!(!old.budget_exhausted);
        assert!(!old.completion_checked);
        assert_eq!(old.repair_turns, 0);
        assert_eq!(old.invalid_citations, 0);
        assert!(!old.search_unavailable);
        assert_eq!(old.stop_reason, AgenticStopReason::FinalAnswer);
    }

    /// No field of the contract invents a `line`; empty citation strings
    /// are omitted from the wire.
    #[test]
    fn citations_have_no_line_field() {
        let empty = AgenticCitation {
            citation_id: "S1".to_string(),
            source_id: "file_chunk:a".to_string(),
            source_kind: "file_chunk".to_string(),
            path: String::new(),
            commit_hash: String::new(),
            score: 0.5,
            excerpt: String::new(),
        };
        let value = serde_json::to_value(&empty).expect("serializes");
        assert!(value.get("line").is_none());
        assert!(value.get("path").is_none(), "empty path omitted");
        assert!(value.get("commit_hash").is_none(), "empty commit omitted");
        assert!(value.get("excerpt").is_none(), "empty excerpt omitted");

        let full = AgenticCitation {
            path: "src/a.rs".to_string(),
            commit_hash: "abc1234".to_string(),
            excerpt: "fn a() {}".to_string(),
            ..empty
        };
        let value = serde_json::to_value(&full).expect("serializes");
        assert!(value.get("line").is_none());
        assert_eq!(value["path"], "src/a.rs");
        assert_eq!(value["commit_hash"], "abc1234");
        assert_eq!(value["excerpt"], "fn a() {}");
    }

    /// `sources[0]` keeps the historical wire names and none of the new
    /// fields leak a `line`.
    #[test]
    fn sources_wire_shape_unchanged() {
        let result = AgenticResult {
            sources: vec![search_result("file_chunk:a", "src/a.rs")],
            ..Default::default()
        };
        let value = serde_json::to_value(&result).expect("serializes");
        let source = &value["sources"][0];
        assert_eq!(source["code_chunk_id"], "file_chunk:a");
        assert_eq!(source["file_path"], "src/a.rs");
        assert_eq!(source["relevance_score"], 0.91);
        assert!(source.get("line").is_none());
    }

    /// E15 scope: the `at_ref` branch keeps returning plain
    /// `Vec<SearchResult>` (or the degraded `{degraded, results}` envelope)
    /// — no trust/citation fields leak into the structured branch.
    #[test]
    fn at_ref_payload_unchanged() {
        let results = vec![search_result("file_chunk:a", "src/a.rs")];
        let payload = serde_json::to_value(&results).expect("serializes");
        assert!(payload[0].get("trust").is_none());
        assert!(payload[0].get("citations").is_none());
        assert!(payload[0].get("line").is_none());

        let degraded = serde_json::json!({ "degraded": true, "results": results });
        assert!(degraded.get("trust").is_none());
        assert!(degraded.get("citations").is_none());
        assert_eq!(degraded["degraded"], true);
        assert!(degraded["results"].is_array());
    }

    /// The observation prefixes citable lines with their stable id and
    /// leaves beyond-cap sources unprefixed.
    #[test]
    fn observation_prefixes_citable_lines_with_stable_ids() {
        let citable = ObservedSource {
            citation_id: Some("S1".to_string()),
            result: search_result("file_chunk:a", "src/a.rs"),
        };
        let over_cap = ObservedSource {
            citation_id: None,
            result: search_result("file_chunk:b", "src/b.rs"),
        };
        let observation = format_observation(&[citable, over_cap]);
        assert!(
            observation.contains("[S1] - File: src/a.rs"),
            "citable line carries its id: {observation}"
        );
        assert!(
            observation.contains("\n- File: src/b.rs"),
            "beyond-cap line has no prefix: {observation}"
        );
    }

    // --- E14.5 prompt contract -------------------------------------------

    /// SHA-256 of the text with CRLF normalized to LF: the snapshot pins the
    /// prompt content, not the checkout's line endings (the repo is checked
    /// out with `core.autocrlf=true` on Windows and LF elsewhere).
    fn sha256_normalized(text: &str) -> String {
        use sha2::Digest;
        let normalized = text.replace("\r\n", "\n");
        let digest = sha2::Sha256::digest(normalized.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// D13: the fixed prefix (`SYSTEM_PROMPT + "\n\n" + CITATION_PROTOCOL`)
    /// is byte-identical across branch contexts, the branch context follows
    /// it, and the question is always the last block.
    #[test]
    fn prompt_prefix_is_byte_stable() {
        let prefix = format!("{SYSTEM_PROMPT}\n\n{CITATION_PROTOCOL}");
        let with_main = build_agentic_prompt("why did init change?", "main");
        let with_branch = build_agentic_prompt("why did init change?", "release/2.0");

        for prompt in [&with_main, &with_branch] {
            assert!(prompt.starts_with(&prefix), "fixed prefix first");
            assert!(
                prompt.ends_with("\n\nQuestion: why did init change?"),
                "question is last"
            );
        }
        assert_eq!(
            &with_main[..prefix.len()],
            &with_branch[..prefix.len()],
            "prefix bytes never depend on the branch context"
        );
        assert!(with_main.contains("branch/commit: main."));
        assert!(with_branch.contains("branch/commit: release/2.0."));

        let no_branch = build_agentic_prompt("why did init change?", "");
        assert!(no_branch.starts_with(&prefix));
        assert!(no_branch.ends_with("\n\nQuestion: why did init change?"));
        assert!(!no_branch.contains("TOPOLOGICAL CONTEXT"));
    }

    /// E14.5: the citation block is appended exactly once, whatever the
    /// branch context.
    #[test]
    fn citation_protocol_appended_once() {
        for branch in ["", "main", "feature/citations"] {
            let prompt = build_agentic_prompt("q", branch);
            assert_eq!(
                prompt.matches("CITATION PROTOCOL:").count(),
                1,
                "branch={branch:?}"
            );
            assert_eq!(
                prompt.matches(CITATION_PROTOCOL).count(),
                1,
                "branch={branch:?}"
            );
        }
    }

    /// Accidental edits to the byte-stable system prompt break this test and
    /// require a deliberate snapshot update.
    #[test]
    fn system_prompt_hash_snapshot() {
        assert_eq!(
            sha256_normalized(SYSTEM_PROMPT),
            "30cce214c64692d6e3146190954438a7201eb4b68f4c243a0601169eaa3b072f"
        );
    }

    /// Same contract for the new citation block (§E14.5).
    #[test]
    fn citation_protocol_hash_snapshot() {
        assert_eq!(
            sha256_normalized(CITATION_PROTOCOL),
            "040e7a77b5d2507818c5218f7f21977317840a655d3f0fd8c1718630e04a296f"
        );
    }

    /// E14.5/§E14.3: exhausting the step budget without a FINAL ANSWER keeps
    /// the fallback answer + trust but reports `MaxSteps` instead of a fake
    /// `FinalAnswer`.
    #[tokio::test]
    async fn max_steps_without_final_answer_sets_max_steps() {
        let ai = crate::test_support::FakeAI::with_generate(|_| Candidate {
            content: Content {
                role: "model".to_string(),
                parts: vec![Part {
                    text: "THOUGHT: need more evidence.\nSEARCH: \"anything\"".to_string(),
                }],
            },
            finish_reason: "STOP".to_string(),
            usage_metadata: None,
        });
        let service = SearchService::new(None, Some(ai));
        let result = service
            .ask_project_agentic("why?", "")
            .await
            .expect("degraded search still completes the loop");

        assert_eq!(result.steps.len(), AGENTIC_MAX_STEPS);
        assert_eq!(result.stop_reason, AgenticStopReason::MaxSteps);
        assert!(
            result.answer.contains("could not find a definitive answer"),
            "fallback answer kept: {}",
            result.answer
        );
        assert!(result.citations.is_empty());
        assert_eq!(result.trust.level, TrustLevel::Low);
    }
}
