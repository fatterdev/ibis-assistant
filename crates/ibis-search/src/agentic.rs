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
//!
//! E14 (consumer side, ddofds fase 4) turns the bare max-steps loop into a
//! bounded ReAct: token budget with an output reservation, no-progress
//! detection on the normalized query, citation-based completion check
//! (off/warn/enforce) with a single repair turn, and best-effort failure
//! modes. E14 only consumes `assess()` — it never recomputes coverage/trust.

use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::OnceLock;

use ibis_ai::types::{estimate_tokens, Candidate, Content, Part};

use crate::agentic_trust::{
    assess, citation_key, AgenticCitation, AgenticTrust, CitationCatalog, ObserveOutcome,
    TrustAssessment,
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

/// Resolved agentic loop brakes (`search.agentic`, E14). Plain struct like
/// `FusionMode`/`RerankMode`: [`AgenticSettings::from_config`] clamps and
/// resolves the raw config values and never fails the boot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgenticSettings {
    pub max_steps: usize,
    /// Raw-token budget per ask; `0` = brake off.
    pub token_budget: u64,
    pub max_repeated_searches: usize,
    pub completion_check: CompletionCheckMode,
    pub completion_min_coverage: f64,
}

impl Default for AgenticSettings {
    fn default() -> Self {
        AgenticSettings::from_config(&ibis_core::config::AgenticConfig::default())
    }
}

impl AgenticSettings {
    /// Clamp + resolve the raw `search.agentic` block (§E14.1). Every clamp
    /// and every unknown value logs a warning; the resolution never fails.
    pub fn from_config(cfg: &ibis_core::config::AgenticConfig) -> Self {
        let max_steps = clamp_with_warn(cfg.max_steps, 1, 10, "search.agentic.max_steps") as usize;
        let token_budget = match cfg.token_budget {
            0 => 0,
            value @ 1..=3999 => {
                tracing::warn!(
                    "search.agentic.token_budget {value} clamped to 4000 (use 0 to disable the brake)"
                );
                4_000
            }
            value if value > 500_000 => {
                tracing::warn!("search.agentic.token_budget {value} clamped to 500000");
                500_000
            }
            value => value,
        } as u64;
        let max_repeated_searches = clamp_with_warn(
            cfg.max_repeated_searches,
            1,
            5,
            "search.agentic.max_repeated_searches",
        ) as usize;
        let completion_check = CompletionCheckMode::resolve(&cfg.completion_check);
        let completion_min_coverage = if cfg.completion_min_coverage.is_finite() {
            let clamped = cfg.completion_min_coverage.clamp(0.0, 1.0);
            if clamped != cfg.completion_min_coverage {
                tracing::warn!(
                    "search.agentic.completion_min_coverage {} clamped to {clamped}",
                    cfg.completion_min_coverage
                );
            }
            clamped
        } else {
            tracing::warn!(
                "search.agentic.completion_min_coverage {} is not finite; using 0.60",
                cfg.completion_min_coverage
            );
            0.60
        };
        AgenticSettings {
            max_steps,
            token_budget,
            max_repeated_searches,
            completion_check,
            completion_min_coverage,
        }
    }

    /// Coverage threshold in base 1000 (`round(clamp(x, 0, 1) * 1000)`).
    pub fn threshold_bp(&self) -> u64 {
        (self.completion_min_coverage.clamp(0.0, 1.0) * 1000.0).round() as u64
    }
}

fn clamp_with_warn(value: u32, min: u32, max: u32, key: &str) -> u32 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        tracing::warn!("{key} {value} clamped to {clamped}");
    }
    clamped
}

/// Citation-based completion check mode (`search.agentic.completion_check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionCheckMode {
    /// No check, no repair; `completion_checked` stays false.
    Off,
    /// Check + warn log, the answer is always accepted; no repair.
    #[default]
    Warn,
    /// Enforce the coverage threshold: one repair turn max.
    Enforce,
}

impl CompletionCheckMode {
    /// Parses the config value (`None` = unknown/empty).
    pub fn parse(value: &str) -> Option<CompletionCheckMode> {
        match value.trim().to_lowercase().as_str() {
            "off" => Some(CompletionCheckMode::Off),
            "warn" => Some(CompletionCheckMode::Warn),
            "enforce" => Some(CompletionCheckMode::Enforce),
            _ => None,
        }
    }

    /// Resolves the config value; unknown/empty values log a warning and fall
    /// back to the warn default.
    pub fn resolve(value: &str) -> CompletionCheckMode {
        match CompletionCheckMode::parse(value) {
            Some(mode) => mode,
            None => {
                tracing::warn!(
                    "unknown search.agentic.completion_check value {value:?}; using warn"
                );
                CompletionCheckMode::Warn
            }
        }
    }

    /// Config-facing name.
    pub fn as_str(&self) -> &'static str {
        match self {
            CompletionCheckMode::Off => "off",
            CompletionCheckMode::Warn => "warn",
            CompletionCheckMode::Enforce => "enforce",
        }
    }
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

/// D10 per-task routing: the ReAct ask is the heaviest local task (tier L);
/// one provider is pinned for the whole loop, so the model never switches
/// mid-task. The repair turn uses the same hint.
const AGENTIC_ROUTE_HINT: &str = "local:L";
/// Output reservation of every loop call (`max_output_tokens`): the budget
/// projection reserves it before each call so an ask can never blow past the
/// budget by a full generation.
const AGENTIC_MAX_OUTPUT_TOKENS: usize = 2000;
/// Answer when the budget stops the loop before a FINAL ANSWER.
const AGENTIC_BUDGET_ANSWER: &str =
    "I could not complete the investigation within the token budget. The evidence gathered so far is attached.";
/// Answer when the loop ends without a FINAL ANSWER (max steps, no progress,
/// AI error).
const AGENTIC_FALLBACK_ANSWER: &str =
    "I could not find a definitive answer after multiple steps. Please try a more specific question.";
/// Answer for an empty-query ask (short-circuit before any AI/DB call).
const AGENTIC_EMPTY_QUERY_ANSWER: &str = "Please provide a non-empty question.";

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
    /// Performs the multi-step ReAct search with the E14 loop brakes.
    pub async fn ask_project_agentic(
        &self,
        query: &str,
        branch_context: &str,
    ) -> Result<AgenticResult> {
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::other("AI provider not configured"))?;
        let settings = self.agentic;

        let mut result = AgenticResult::default();
        let mut catalog = CitationCatalog::default();
        let mut observed: Vec<ObservedSource> = Vec::new();
        let mut observed_keys: HashSet<String> = HashSet::new();

        // E14: empty-query short-circuit — no AI generation, no DB call.
        if query.trim().is_empty() {
            result.answer = AGENTIC_EMPTY_QUERY_ANSWER.to_string();
            result.stop_reason = AgenticStopReason::EmptyQuery;
            let assessment = assess(&catalog, &result.answer);
            result.trust = assessment.trust;
            result.invalid_citations = assessment.invalid_citations;
            return Ok(result);
        }

        let system_prompt = build_agentic_prompt(query, branch_context);
        let mut history: Vec<Content> = vec![Content {
            role: "user".to_string(),
            parts: vec![Part {
                text: system_prompt,
            }],
        }];

        // E14 loop brakes state.
        let mut budget_used: u64 = 0;
        let mut budget_exhausted = false;
        let mut search_unavailable = false;
        let mut last_fingerprint: Option<String> = None;
        let mut consecutive_count: usize = 0;
        let mut no_progress_nudged = false;
        let mut repair_turns: u8 = 0;
        let mut completion_checked = false;
        // The original FINAL ANSWER awaiting the single repair turn.
        let mut pending: Option<(String, TrustAssessment)> = None;
        // The chosen answer + its assessment (accepted FINAL or best-so-far).
        let mut accepted: Option<(String, TrustAssessment)> = None;
        // §E14.3: without a FINAL ANSWER the step budget is what stopped the
        // ask; every other terminal state overwrites this.
        let mut stop_reason = AgenticStopReason::MaxSteps;
        let mut steps_used: usize = 0;
        let threshold_bp = settings.threshold_bp();

        for _ in 0..settings.max_steps {
            // E14.2 — budget check BEFORE the call, with the output
            // reservation. Breaks with the evidence already gathered.
            let prompt_text = history_text(&history);
            let projected = budget_used
                .saturating_add(estimate_tokens(&prompt_text) as u64)
                .saturating_add(AGENTIC_MAX_OUTPUT_TOKENS as u64);
            if settings.token_budget > 0 && projected > settings.token_budget {
                budget_exhausted = true;
                stop_reason = AgenticStopReason::BudgetExhausted;
                break;
            }

            let candidate: Candidate = match ai
                .generate_content(
                    &history,
                    ibis_ai::types::GenerationConfig {
                        temperature: 0.2,
                        max_output_tokens: AGENTIC_MAX_OUTPUT_TOKENS,
                        route_hint: AGENTIC_ROUTE_HINT.to_string(),
                        // D12: agentic ask is never response-cached.
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(candidate) => candidate,
                Err(err) => {
                    if steps_used == 0 {
                        return Err(SearchError::other(format!("AI generation failed: {err}")));
                    }
                    stop_reason = if err.is_context_only() {
                        AgenticStopReason::ContextOnly
                    } else {
                        AgenticStopReason::AiError
                    };
                    break;
                }
            };
            steps_used += 1;

            let response: String = candidate
                .content
                .parts
                .iter()
                .map(|p| p.text.as_str())
                .collect();

            // E14.2 — raw-token accounting and cost recording. Usage wins
            // when present (total fallback, then estimate); with no usage the
            // estimate is both budgeted and billed so the cost never stays
            // invisible. Budget counts RAW tokens; the D13 cache discount is
            // applied by the CostTracker to the billed value only.
            let spent = match &candidate.usage_metadata {
                Some(usage) => {
                    let prompt = usage.prompt_token_count.max(0) as u64;
                    let candidates = usage.candidates_token_count.max(0) as u64;
                    if prompt + candidates > 0 {
                        prompt + candidates
                    } else if usage.total_token_count > 0 {
                        usage.total_token_count.max(0) as u64
                    } else {
                        (estimate_tokens(&prompt_text) + estimate_tokens(&response)) as u64
                    }
                }
                None => (estimate_tokens(&prompt_text) + estimate_tokens(&response)) as u64,
            };
            budget_used = budget_used.saturating_add(spent);
            if self.db.is_some() {
                let (prompt_tokens, candidate_tokens, cached) = match &candidate.usage_metadata {
                    Some(usage) => (
                        usage.prompt_token_count.max(0) as u64,
                        usage.candidates_token_count.max(0) as u64,
                        usage.cached_tokens(),
                    ),
                    None => (
                        estimate_tokens(&prompt_text) as u64,
                        estimate_tokens(&response) as u64,
                        0,
                    ),
                };
                crate::search::cost_tracker(self)
                    .record_usage(
                        "system:search_agentic",
                        prompt_tokens,
                        candidate_tokens,
                        cached,
                    )
                    .await;
            }

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
                let answer = response[loc_final.unwrap().end()..].trim().to_string();
                // E15 computes the assessment; E14 only reads it.
                let assessment = assess(&catalog, &answer);
                match settings.completion_check {
                    CompletionCheckMode::Off => {
                        accepted = Some((answer, assessment));
                        stop_reason = AgenticStopReason::FinalAnswer;
                        break;
                    }
                    CompletionCheckMode::Warn => {
                        completion_checked = true;
                        tracing::warn!(
                            coverage = assessment.trust.coverage,
                            covered_claims = assessment.covered_claims,
                            total_claims = assessment.total_claims,
                            invalid_citations = assessment.invalid_citations,
                            threshold_bp,
                            "agentic completion check (warn): answer accepted"
                        );
                        accepted = Some((answer, assessment));
                        stop_reason = AgenticStopReason::FinalAnswer;
                        break;
                    }
                    CompletionCheckMode::Enforce => {
                        completion_checked = true;
                        // After a repair turn keep the better of the two
                        // assessments; a tie keeps the original (less churn).
                        let (best_answer, best_assessment) = match pending.take() {
                            Some((pending_answer, pending_assessment))
                                if pending_assessment.coverage_bp >= assessment.coverage_bp =>
                            {
                                (pending_answer, pending_assessment)
                            }
                            _ => (answer, assessment),
                        };
                        if best_assessment.coverage_bp >= threshold_bp {
                            accepted = Some((best_answer, best_assessment));
                            stop_reason = AgenticStopReason::FinalAnswer;
                            break;
                        }
                        if repair_turns == 0 && steps_used < settings.max_steps {
                            // Budget pre-check: the repair only happens when
                            // the next call fits (the turn consumes a step
                            // and the budget).
                            let repair_text =
                                repair_observation_text(&best_assessment, threshold_bp);
                            let mut repaired_history = history.clone();
                            repaired_history.push(Content {
                                role: "user".to_string(),
                                parts: vec![Part { text: repair_text }],
                            });
                            let projected = budget_used
                                .saturating_add(
                                    estimate_tokens(&history_text(&repaired_history)) as u64
                                )
                                .saturating_add(AGENTIC_MAX_OUTPUT_TOKENS as u64);
                            if settings.token_budget > 0 && projected > settings.token_budget {
                                budget_exhausted = true;
                                stop_reason = AgenticStopReason::BudgetExhausted;
                                accepted = Some((best_answer, best_assessment));
                                break;
                            }
                            history = repaired_history;
                            repair_turns = 1;
                            pending = Some((best_answer, best_assessment));
                            continue;
                        }
                        accepted = Some((best_answer, best_assessment));
                        stop_reason = AgenticStopReason::CompletionLowCoverage;
                        break;
                    }
                }
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

                // E14.3 — empty query (raw or after normalization) is not a
                // repeated search: the existing nudge applies, nothing runs.
                let normalized = normalize_search_query(&search_query);
                if search_query.is_empty() || normalized.is_empty() {
                    tracing::info!(
                        "Agentic Search Step {}: Skipping empty search query",
                        steps_used
                    );
                    history.push(Content {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: "OBSERVATION: Search query was empty. Please provide a valid search term."
                                .to_string(),
                        }],
                    });
                    continue;
                }

                // E14.6 — DB brake: once the DB is unavailable no further
                // search is attempted (and none counts as repeated).
                if search_unavailable {
                    history.push(Content {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: "OBSERVATION: Search is unavailable (database not connected). Answer from the evidence you have or state the limitation."
                                .to_string(),
                        }],
                    });
                    continue;
                }

                // E14.3 — consecutive-repeat detection on the normalized
                // query (A, B, A is not a doom loop: the counter resets).
                let fingerprint = search_fingerprint(&normalized);
                if last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
                    consecutive_count += 1;
                } else {
                    consecutive_count = 1;
                    last_fingerprint = Some(fingerprint);
                    no_progress_nudged = false;
                }
                if consecutive_count > settings.max_repeated_searches {
                    if no_progress_nudged {
                        stop_reason = AgenticStopReason::NoProgress;
                        break;
                    }
                    history.push(Content {
                        role: "user".to_string(),
                        parts: vec![Part {
                            text: format!(
                                "OBSERVATION: This exact search was already executed {} times and returned the same evidence. Do not repeat it. Refine the query with different terms or emit FINAL ANSWER with the evidence you have.",
                                settings.max_repeated_searches
                            ),
                        }],
                    });
                    no_progress_nudged = true;
                    continue;
                }

                tracing::info!(
                    "Agentic Search Step {}: Searching for '{}'",
                    steps_used,
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
                    Err(SearchError::DatabaseNotConnected) => {
                        search_unavailable = true;
                        history.push(Content {
                            role: "user".to_string(),
                            parts: vec![Part {
                                text: "OBSERVATION: Search is unavailable (database not connected). Answer from the evidence you have or state the limitation."
                                    .to_string(),
                            }],
                        });
                        continue;
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

        // Sources and catalog share first-observation order. `sources` keeps
        // the full metadata; `citations` is the citable index (post dedup and
        // cap). Over-cap sources are present in `sources` with no id.
        result.sources = observed.into_iter().map(|o| o.result).collect();
        result.citations = catalog.entries().to_vec();

        // E14 — resolve the final answer: the accepted FINAL, the pending
        // (repair) answer, or the terminal fallback.
        let (answer, assessment) = match accepted {
            Some(pair) => pair,
            None => match pending {
                Some(pair) => pair,
                None => {
                    let fallback = if budget_exhausted {
                        AGENTIC_BUDGET_ANSWER
                    } else {
                        AGENTIC_FALLBACK_ANSWER
                    };
                    let assessment = assess(&catalog, fallback);
                    (fallback.to_string(), assessment)
                }
            },
        };
        result.answer = answer;
        result.trust = assessment.trust;
        result.invalid_citations = assessment.invalid_citations;
        result.completion_checked = completion_checked;
        result.repair_turns = repair_turns;
        result.budget_exhausted = budget_exhausted;
        result.search_unavailable = search_unavailable;
        result.stop_reason = stop_reason;

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
    observed_keys: &mut HashSet<String>,
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

/// Concatenates every history part (the budget projection input).
fn history_text(history: &[Content]) -> String {
    let mut text = String::new();
    for content in history {
        for part in &content.parts {
            text.push_str(&part.text);
            text.push('\n');
        }
    }
    text
}

/// E14.3 — query normalization for the repeated-search fingerprint:
/// trim + lowercase + collapse whitespace + strip edge punctuation (internal
/// punctuation survives: `src/main.rs`, `foo-bar`, `a_b` stay distinct).
fn normalize_search_query(query: &str) -> String {
    let lowered = query.trim().to_lowercase();
    let collapsed = lowered.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '.' | ','
                    | ';'
                    | ':'
                    | '!'
                    | '?'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | '"'
                    | '\''
            )
        })
        .to_string()
}

/// E14.3 — `hex(sha256(normalized))[..16]`: a stable dedup key/log id.
fn search_fingerprint(normalized: &str) -> String {
    let digest = Sha256::digest(normalized.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// E14.4 — the actionable repair observation (§E14.4.4 exact wording).
fn repair_observation_text(assessment: &TrustAssessment, threshold_bp: u64) -> String {
    let coverage = assessment.coverage_bp as f64 / 1000.0;
    let threshold = threshold_bp as f64 / 1000.0;
    let invalid = match assessment.invalid_citations {
        0 => String::new(),
        1 => ", 1 invalid citation".to_string(),
        n => format!(", {n} invalid citations"),
    };
    format!(
        "OBSERVATION: FINAL ANSWER coverage is {coverage:.3} ({}/{} claims cited{invalid}) below the required {threshold:.3}. Re-emit FINAL ANSWER and add [S<n>] citations to the uncited claims, using only ids shown in OBSERVATION. If a claim has no supporting evidence, remove it or state that it is unverified. Do not invent ids.",
        assessment.covered_claims, assessment.total_claims
    )
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
    use crate::test_support::{obj, rows_of, FakeDb};
    use async_trait::async_trait;
    use ibis_ai::{AIProvider, AiError};
    use ibis_db::value::Value;
    use ibis_db::Executor;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // --- scripted AI --------------------------------------------------------

    /// One canned model turn (or failure) per `generate_content` call.
    enum ScriptedTurn {
        Reply(String),
        ReplyWithUsage {
            text: String,
            prompt: i64,
            candidates: i64,
            total: i64,
            cached: Option<i64>,
        },
        Fail(AiError),
    }

    /// Records every call's prompt/config and answers from the script; the
    /// embeddings capture the executed search queries.
    struct ScriptedAi {
        turns: Mutex<VecDeque<ScriptedTurn>>,
        calls: AtomicUsize,
        prompts: Mutex<Vec<String>>,
        configs: Mutex<Vec<ibis_ai::types::GenerationConfig>>,
        embed_texts: Mutex<Vec<String>>,
    }

    impl ScriptedAi {
        fn new(turns: Vec<ScriptedTurn>) -> Arc<ScriptedAi> {
            Arc::new(ScriptedAi {
                turns: Mutex::new(turns.into()),
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                configs: Mutex::new(Vec::new()),
                embed_texts: Mutex::new(Vec::new()),
            })
        }

        fn prompt(&self, index: usize) -> String {
            self.prompts
                .lock()
                .unwrap()
                .get(index)
                .cloned()
                .unwrap_or_default()
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn embedded(&self) -> Vec<String> {
            self.embed_texts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AIProvider for ScriptedAi {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn is_functional(&self) -> bool {
            true
        }

        async fn embed_text(&self, text: &str) -> std::result::Result<Vec<f32>, AiError> {
            self.embed_texts.lock().unwrap().push(text.to_string());
            Ok(vec![0.1, 0.2, 0.3])
        }

        async fn generate_content(
            &self,
            contents: &[Content],
            config: ibis_ai::types::GenerationConfig,
        ) -> std::result::Result<Candidate, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().unwrap().push(
                contents
                    .iter()
                    .flat_map(|c| c.parts.iter().map(|p| p.text.clone()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            self.configs.lock().unwrap().push(config);
            let turn = self.turns.lock().unwrap().pop_front();
            match turn {
                Some(ScriptedTurn::Reply(text)) => Ok(candidate(&text, None)),
                Some(ScriptedTurn::ReplyWithUsage {
                    text,
                    prompt,
                    candidates,
                    total,
                    cached,
                }) => Ok(candidate(
                    &text,
                    Some(ibis_ai::types::UsageMetadata {
                        prompt_token_count: prompt,
                        candidates_token_count: candidates,
                        total_token_count: total,
                        cached_content_token_count: cached,
                    }),
                )),
                Some(ScriptedTurn::Fail(err)) => Err(err),
                None => Err(AiError::other("scripted turns exhausted")),
            }
        }
    }

    fn candidate(text: &str, usage: Option<ibis_ai::types::UsageMetadata>) -> Candidate {
        Candidate {
            content: Content {
                role: "model".into(),
                parts: vec![Part {
                    text: text.to_string(),
                }],
            },
            finish_reason: "STOP".into(),
            usage_metadata: usage,
        }
    }

    fn reply(text: &str) -> ScriptedTurn {
        ScriptedTurn::Reply(text.to_string())
    }

    fn settings(
        max_steps: usize,
        token_budget: u64,
        max_repeated: usize,
        completion: CompletionCheckMode,
        min_coverage: f64,
    ) -> AgenticSettings {
        AgenticSettings {
            max_steps,
            token_budget,
            max_repeated_searches: max_repeated,
            completion_check: completion,
            completion_min_coverage: min_coverage,
        }
    }

    fn service(
        ai: Arc<dyn AIProvider>,
        db: Option<Arc<dyn Executor>>,
        settings: AgenticSettings,
    ) -> SearchService {
        let mut svc = SearchService::new(db, Some(ai));
        svc.agentic = settings;
        svc
    }

    /// A DB whose `FROM file_chunk` channel returns one citable hit.
    fn db_with_one_hit() -> Arc<FakeDb> {
        Arc::new(FakeDb::new(vec![(
            "FROM file_chunk",
            rows_of(vec![obj(vec![
                ("id", Value::from_t("file_chunk:1")),
                ("path", Value::from_t("src/a.rs")),
                ("content", Value::from_t("fn a() {}")),
                ("hash", Value::from_t("h1")),
                ("current_hash", Value::from_t("h1")),
                ("score", Value::from_t(0.9f64)),
            ])]),
        )]))
    }

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

    // --- settings -----------------------------------------------------------

    #[test]
    fn agentic_settings_defaults_match_config() {
        let resolved = AgenticSettings::from_config(&ibis_core::config::AgenticConfig::default());
        assert_eq!(resolved.max_steps, 5);
        assert_eq!(resolved.token_budget, 120_000);
        assert_eq!(resolved.max_repeated_searches, 2);
        assert_eq!(resolved.completion_check, CompletionCheckMode::Warn);
        assert_eq!(resolved.threshold_bp(), 600);
    }

    #[test]
    fn agentic_settings_from_config_is_clamped() {
        let mut cfg = ibis_core::config::AgenticConfig {
            max_steps: 0,
            token_budget: 1,
            max_repeated_searches: 0,
            completion_check: "enforce".into(),
            completion_min_coverage: -1.0,
        };
        let resolved = AgenticSettings::from_config(&cfg);
        assert_eq!(resolved.max_steps, 1);
        assert_eq!(resolved.token_budget, 4_000);
        assert_eq!(resolved.max_repeated_searches, 1);
        assert_eq!(resolved.completion_check, CompletionCheckMode::Enforce);
        assert_eq!(resolved.completion_min_coverage, 0.0);

        cfg = ibis_core::config::AgenticConfig {
            max_steps: 99,
            token_budget: 999_999,
            max_repeated_searches: 99,
            completion_check: "off".into(),
            completion_min_coverage: 2.0,
        };
        let resolved = AgenticSettings::from_config(&cfg);
        assert_eq!(resolved.max_steps, 10);
        assert_eq!(resolved.token_budget, 500_000);
        assert_eq!(resolved.max_repeated_searches, 5);
        assert_eq!(resolved.completion_min_coverage, 1.0);

        // token_budget 0 is the explicit opt-out, never clamped up.
        let off = AgenticSettings::from_config(&ibis_core::config::AgenticConfig {
            token_budget: 0,
            ..Default::default()
        });
        assert_eq!(off.token_budget, 0);
    }

    #[test]
    fn agentic_config_unknown_completion_check_resolves_warn() {
        let resolved = AgenticSettings::from_config(&ibis_core::config::AgenticConfig {
            completion_check: "turbo".into(),
            ..Default::default()
        });
        assert_eq!(resolved.completion_check, CompletionCheckMode::Warn);
        let empty = AgenticSettings::from_config(&ibis_core::config::AgenticConfig {
            completion_check: String::new(),
            ..Default::default()
        });
        assert_eq!(empty.completion_check, CompletionCheckMode::Warn);
        assert_eq!(CompletionCheckMode::Enforce.as_str(), "enforce");
    }

    // --- parser regression estate ------------------------------------------

    #[tokio::test]
    async fn parses_quoted_multiline_search_query() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"foo\nbar\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
        let embedded = ai.embedded();
        assert!(
            embedded.iter().any(|q| q == "foo\nbar"),
            "multiline quoted query executed: {embedded:?}"
        );
    }

    #[tokio::test]
    async fn parses_line_based_search_query() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: parser stuff\nFINAL ANSWER: later"),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        let embedded = ai.embedded();
        assert!(
            embedded.iter().any(|q| q == "parser stuff"),
            "line fallback executes the first line: {embedded:?}"
        );
    }

    #[tokio::test]
    async fn strips_chatter_after_sentence_boundary() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: parser. Please note this extra chatter"),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        let embedded = ai.embedded();
        assert!(
            embedded.iter().any(|q| q == "parser"),
            "chatter stripped at the sentence boundary: {embedded:?}"
        );
    }

    #[tokio::test]
    async fn empty_search_query_pushes_nudge_without_search() {
        let ai = ScriptedAi::new(vec![reply("SEARCH: \"\""), reply("FINAL ANSWER: done")]);
        let db = db_with_one_hit();
        let svc = service(ai.clone(), Some(db), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(
            ai.prompt(1).contains("Search query was empty"),
            "empty-query nudge pushed: {}",
            ai.prompt(1)
        );
        assert!(ai.embedded().is_empty(), "no search executed");
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
    }

    #[tokio::test]
    async fn no_action_pushes_continue_nudge() {
        let ai = ScriptedAi::new(vec![reply("I am thinking."), reply("FINAL ANSWER: done")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(
            ai.prompt(1)
                .contains("Please continue. Output SEARCH or FINAL ANSWER."),
            "no-action nudge pushed: {}",
            ai.prompt(1)
        );
    }

    #[tokio::test]
    async fn search_failure_pushes_observation_and_continues() {
        let mut fake = FakeDb::new(vec![]);
        fake.fail_on = Some("FROM file_chunk".to_string());
        let db: Arc<dyn Executor> = Arc::new(fake);
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(ai.clone(), Some(db), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(
            ai.prompt(1).contains("OBSERVATION: Search failed:"),
            "failure observation pushed: {}",
            ai.prompt(1)
        );
        assert!(
            !result.search_unavailable,
            "non-DB failure keeps the flag false"
        );
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
    }

    #[tokio::test]
    async fn final_answer_extracts_text_after_marker() {
        let ai = ScriptedAi::new(vec![reply("THOUGHT: done. FINAL ANSWER: the answer")]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.answer, "the answer");
    }

    #[tokio::test]
    async fn first_marker_wins_when_both_present() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\"\nFINAL ANSWER: later"),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.embedded(), vec!["alpha".to_string()], "search wins");

        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: done\nSEARCH: \"alpha\"")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(
            result.answer.starts_with("done"),
            "first marker wins: {}",
            result.answer
        );
        assert!(ai.embedded().is_empty(), "final wins, no search");
    }

    #[tokio::test]
    async fn markers_are_case_insensitive() {
        let ai = ScriptedAi::new(vec![reply("final answer: lower case works")]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.answer, "lower case works");
    }

    #[tokio::test]
    async fn malformed_search_still_nudges() {
        // A bare `SEARCH:` with nothing after it is the empty-query nudge.
        let ai = ScriptedAi::new(vec![reply("SEARCH:"), reply("FINAL ANSWER: done")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(ai.prompt(1).contains("Search query was empty"));
        assert!(ai.embedded().is_empty());
    }

    // --- budget -------------------------------------------------------------

    #[tokio::test]
    async fn budget_exhausted_before_first_call_returns_flag_not_error() {
        let query = "x".repeat(9000);
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: never")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 4_000, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic(&query, "").await.expect("ok");
        assert_eq!(ai.call_count(), 0, "no AI call");
        assert!(result.budget_exhausted);
        assert_eq!(result.stop_reason, AgenticStopReason::BudgetExhausted);
        assert_eq!(result.answer, AGENTIC_BUDGET_ANSWER);
        assert_eq!(result.trust.level, TrustLevel::Low);
        assert!(result.trust.weak_context);
        assert!(!result.completion_checked);
        assert_eq!(result.repair_turns, 0);
    }

    #[tokio::test]
    async fn budget_stops_after_partial_progress_keeps_evidence() {
        // First call reports a huge usage → the next projection stops the
        // loop; the evidence gathered by the first search stays attached.
        let ai = ScriptedAi::new(vec![
            ScriptedTurn::ReplyWithUsage {
                text: "SEARCH: \"alpha\"".to_string(),
                prompt: 100_000,
                candidates: 30_000,
                total: 130_000,
                cached: None,
            },
            reply("FINAL ANSWER: never"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 120_000, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 1);
        assert!(result.budget_exhausted);
        assert_eq!(result.stop_reason, AgenticStopReason::BudgetExhausted);
        assert_eq!(result.answer, AGENTIC_BUDGET_ANSWER);
        assert_eq!(
            result.sources.len(),
            1,
            "evidence attached: {:?}",
            result.sources
        );
        assert_eq!(result.citations.len(), 1);
    }

    #[tokio::test]
    async fn budget_zero_disables_the_brake() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: done")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(!result.budget_exhausted);
        assert_eq!(result.answer, "done");
    }

    #[tokio::test]
    async fn usage_metadata_takes_precedence_over_estimate() {
        // The usage reports a raw prompt far above the budget: the estimate
        // would fit, the usage stops the next call.
        let ai = ScriptedAi::new(vec![
            ScriptedTurn::ReplyWithUsage {
                text: "SEARCH: \"alpha\"".to_string(),
                prompt: 500_000,
                candidates: 0,
                total: 0,
                cached: None,
            },
            reply("FINAL ANSWER: never"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 1);
        assert!(result.budget_exhausted);
        assert_eq!(result.stop_reason, AgenticStopReason::BudgetExhausted);
    }

    #[tokio::test]
    async fn usage_absent_estimates_and_records_cost() {
        let db = db_with_one_hit();
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(ai.clone(), Some(db.clone()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.answer, "done");
        let cost_records: Vec<String> = db
            .execute_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains("system:search_agentic"))
            .cloned()
            .collect();
        assert_eq!(cost_records.len(), 2, "one cost record per call");
        for record in cost_records {
            let value: u64 = record
                .split("tokens_used += ")
                .nth(1)
                .and_then(|rest| rest.trim_end_matches(';').parse().ok())
                .unwrap_or(0);
            assert!(value > 0, "estimated cost recorded: {record}");
        }
    }

    #[tokio::test]
    async fn negative_usage_is_clamped() {
        let ai = ScriptedAi::new(vec![ScriptedTurn::ReplyWithUsage {
            text: "FINAL ANSWER: done".to_string(),
            prompt: -5,
            candidates: -5,
            total: -1,
            cached: Some(-3),
        }]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.answer, "done");
        assert!(!result.budget_exhausted);
    }

    #[tokio::test]
    async fn budget_uses_raw_tokens_not_billed_tokens() {
        // usage raw = 1000 prompt tokens (900 cached) → billed 190; the
        // budget must stop on the RAW value.
        let ai = ScriptedAi::new(vec![
            ScriptedTurn::ReplyWithUsage {
                text: "SEARCH: \"alpha\"".to_string(),
                prompt: 1_000,
                candidates: 0,
                total: 1_000,
                cached: Some(900),
            },
            reply("FINAL ANSWER: never"),
        ]);
        // 3400 sits between the billed projection (~2.9k) and the raw one
        // (~3.9k) for the deterministic initial prompt.
        let db = db_with_one_hit();
        let svc = service(
            ai.clone(),
            Some(db.clone()),
            settings(5, 3_400, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 1, "raw tokens stop the second call");
        assert!(result.budget_exhausted);
        assert_eq!(result.stop_reason, AgenticStopReason::BudgetExhausted);
        // The billing layer still records the D13 discount: 1000 − 0.9×900.
        let billed: Vec<String> = db
            .execute_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains("system:search_agentic"))
            .cloned()
            .collect();
        assert_eq!(billed.len(), 1);
        assert!(billed[0].contains("+= 190"), "billed 190: {}", billed[0]);
    }

    // --- no-progress ---------------------------------------------------------

    #[tokio::test]
    async fn repeated_identical_search_is_not_executed_and_nudges() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: later"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Warn, 0.60),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.embedded().len(), 2, "third search not executed");
        assert!(
            ai.prompt(3)
                .contains("This exact search was already executed 2 times"),
            "nudge pushed: {}",
            ai.prompt(3)
        );
    }

    #[tokio::test]
    async fn second_identical_search_after_nudge_stops_no_progress() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.embedded().len(), 2, "bounded executions");
        assert_eq!(result.stop_reason, AgenticStopReason::NoProgress);
        assert_eq!(result.answer, AGENTIC_FALLBACK_ANSWER);
    }

    #[tokio::test]
    async fn different_search_resets_repeat_counter() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"beta\""),
            reply("SEARCH: \"beta\""),
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(6, 0, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(
            ai.embedded().len(),
            5,
            "each run executes the allowed times"
        );
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
    }

    #[tokio::test]
    async fn alternating_queries_are_not_consecutive_repeats() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"beta\""),
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"beta\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 1, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.embedded().len(), 4, "A, B, A, B all execute");
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
    }

    #[test]
    fn normalization_ignores_case_whitespace_and_edge_punctuation() {
        assert_eq!(normalize_search_query("  Fix   Parser. "), "fix parser");
        assert_eq!(normalize_search_query("\"Fix Parser!\""), "fix parser");
        assert_eq!(normalize_search_query("(fix: parser)"), "fix: parser");
        assert_eq!(normalize_search_query("..."), "");
    }

    #[test]
    fn normalization_preserves_internal_path_punctuation() {
        assert_eq!(normalize_search_query(" src/main.rs "), "src/main.rs");
        assert_eq!(normalize_search_query("foo-bar"), "foo-bar");
        assert_eq!(normalize_search_query("a_b"), "a_b");
        assert_ne!(
            normalize_search_query("src/main.rs"),
            normalize_search_query("src main rs")
        );
    }

    #[tokio::test]
    async fn empty_query_is_not_a_repeated_search() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"\""),
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 1, CompletionCheckMode::Warn, 0.60),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        // alpha executes once (max_repeated=1); the second alpha is nudged.
        // The empty query never entered the counter.
        let embedded = ai.embedded();
        assert_eq!(embedded, vec!["alpha".to_string()]);
    }

    #[tokio::test]
    async fn normalization_collapses_variants_into_one_fingerprint() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"Fix Parser!\""),
            reply("SEARCH: \"  fix   parser  \""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(4, 0, 1, CompletionCheckMode::Warn, 0.60),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.embedded().len(), 1, "normalized variant is a repeat");
        assert!(ai
            .prompt(2)
            .contains("This exact search was already executed 1 times"));
    }

    // --- completion check ----------------------------------------------------

    #[tokio::test]
    async fn completion_enforce_repairs_once_and_accepts_cited_answer() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: The parser is bounded. It retries."),
            reply("FINAL ANSWER: The parser is bounded [S1]. It retries [S1]."),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 3, "one repair turn");
        assert!(result.completion_checked);
        assert_eq!(result.repair_turns, 1);
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
        assert!(result.answer.contains("[S1]"));
        assert_eq!(result.trust.coverage, 1.0);
    }

    #[tokio::test]
    async fn completion_enforce_keeps_better_answer() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: Alpha is true [S1]. Beta is false."),
            reply("FINAL ANSWER: Alpha is false. Beta is false."),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.repair_turns, 1);
        assert_eq!(result.stop_reason, AgenticStopReason::CompletionLowCoverage);
        assert!(
            result.answer.contains("[S1]"),
            "the better original is kept: {}",
            result.answer
        );
        assert_eq!(result.trust.coverage, 0.5);
    }

    #[tokio::test]
    async fn completion_enforce_accepts_below_threshold_after_one_repair() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: Alpha is true. Beta is false."),
            reply("FINAL ANSWER: Gamma is true. Alpha is true [S1]. Delta is false."),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.repair_turns, 1);
        assert_eq!(result.stop_reason, AgenticStopReason::CompletionLowCoverage);
        assert_eq!(result.trust.coverage, 0.333);
    }

    #[tokio::test]
    async fn completion_warn_accepts_without_repair() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: Alpha is true. Beta is false.")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 1, "no repair");
        assert!(result.completion_checked);
        assert_eq!(result.repair_turns, 0);
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
        assert_eq!(result.trust.coverage, 0.0);
    }

    #[tokio::test]
    async fn completion_off_skips_check_but_computes_trust() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: Alpha is false.")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Off, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(!result.completion_checked);
        assert_eq!(result.repair_turns, 0);
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
        assert_eq!(result.trust.level, TrustLevel::Low);
    }

    #[tokio::test]
    async fn completion_fail_at_last_step_has_no_repair() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: Alpha is false.")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(1, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 1, "no repair call");
        assert_eq!(result.repair_turns, 0);
        assert!(result.completion_checked);
        assert_eq!(result.stop_reason, AgenticStopReason::CompletionLowCoverage);
    }

    #[tokio::test]
    async fn completion_repair_search_keeps_pending_answer() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: Alpha is true [S1]. Beta is false."),
            reply("SEARCH: \"beta\""),
            reply("FINAL ANSWER: Gamma is false."),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.embedded().len(), 2, "repair search executed");
        assert_eq!(result.repair_turns, 1);
        assert_eq!(result.stop_reason, AgenticStopReason::CompletionLowCoverage);
        assert!(
            result.answer.contains("[S1]"),
            "pending answer kept over the worse one: {}",
            result.answer
        );
    }

    #[tokio::test]
    async fn completion_repair_error_keeps_pending_answer() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: Alpha is false."),
            ScriptedTurn::Fail(AiError::other("repair boom")),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.stop_reason, AgenticStopReason::AiError);
        assert_eq!(result.repair_turns, 1);
        assert!(result.completion_checked);
        assert_eq!(result.answer, "Alpha is false.");
    }

    #[tokio::test]
    async fn completion_budget_block_keeps_pending_answer() {
        let ai = ScriptedAi::new(vec![
            ScriptedTurn::ReplyWithUsage {
                text: "FINAL ANSWER: Alpha is false.".to_string(),
                prompt: 119_500,
                candidates: 0,
                total: 119_500,
                cached: None,
            },
            reply("FINAL ANSWER: never"),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 120_000, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(ai.call_count(), 1, "repair blocked by budget");
        assert!(result.budget_exhausted);
        assert_eq!(result.stop_reason, AgenticStopReason::BudgetExhausted);
        assert_eq!(result.repair_turns, 0);
        assert!(result.completion_checked);
        assert_eq!(result.answer, "Alpha is false.");
    }

    #[tokio::test]
    async fn invalid_citations_are_counted() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: Alpha is true [S99].")]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.invalid_citations, 1);
        assert_eq!(result.trust.coverage, 0.0);
    }

    #[tokio::test]
    async fn no_citations_is_low_trust_not_error() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: Alpha is true.")]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
        assert_eq!(result.trust.coverage, 0.0);
        assert_eq!(result.trust.level, TrustLevel::Low);
    }

    #[tokio::test]
    async fn no_claims_coverage_is_zero() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: ```\nlet x = 1; // [S1]\n```")]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.trust.coverage, 0.0);
        assert_eq!(result.invalid_citations, 0);
    }

    // --- prompt --------------------------------------------------------------

    #[tokio::test]
    async fn question_is_last() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: done")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let _ = svc
            .ask_project_agentic("why is it so?", "main")
            .await
            .expect("ok");
        let prompt = ai.prompt(0);
        assert!(
            prompt.trim_end().ends_with("Question: why is it so?"),
            "question is last: {prompt}"
        );
    }

    #[tokio::test]
    async fn response_without_citations_still_parses() {
        let ai = ScriptedAi::new(vec![reply("THOUGHT: x\nFINAL ANSWER: no citations here")]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.answer, "no citations here");
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
    }

    // --- failure modes --------------------------------------------------------

    #[tokio::test]
    async fn ai_error_after_one_step_returns_best_effort() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            ScriptedTurn::Fail(AiError::other("boom")),
        ]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.stop_reason, AgenticStopReason::AiError);
        assert_eq!(result.answer, AGENTIC_FALLBACK_ANSWER);
        assert_eq!(result.sources.len(), 1);
    }

    #[tokio::test]
    async fn ai_error_before_any_step_propagates() {
        let ai = ScriptedAi::new(vec![ScriptedTurn::Fail(AiError::other("boom"))]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let err = svc
            .ask_project_agentic("why?", "")
            .await
            .expect_err("0 steps → Err");
        assert!(err.to_string().contains("AI generation failed"), "{err}");
    }

    #[tokio::test]
    async fn context_only_after_one_step_is_distinct_stop_reason() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            ScriptedTurn::Fail(AiError::ContextOnly),
        ]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.stop_reason, AgenticStopReason::ContextOnly);
    }

    #[tokio::test]
    async fn context_only_before_any_step_propagates() {
        let ai = ScriptedAi::new(vec![ScriptedTurn::Fail(AiError::ContextOnly)]);
        let svc = service(ai, Some(db_with_one_hit()), AgenticSettings::default());
        assert!(svc.ask_project_agentic("why?", "").await.is_err());
    }

    #[tokio::test]
    async fn empty_query_short_circuits_without_ai_call() {
        let ai = ScriptedAi::new(vec![reply("FINAL ANSWER: never")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            AgenticSettings::default(),
        );
        let result = svc.ask_project_agentic("   ", "").await.expect("ok");
        assert_eq!(ai.call_count(), 0);
        assert!(ai.embedded().is_empty());
        assert_eq!(result.answer, AGENTIC_EMPTY_QUERY_ANSWER);
        assert_eq!(result.stop_reason, AgenticStopReason::EmptyQuery);
        assert!(!result.completion_checked);
    }

    #[tokio::test]
    async fn db_unavailable_sets_search_unavailable_and_skips_retries() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("SEARCH: \"beta\""),
            reply("FINAL ANSWER: done"),
        ]);
        let svc = service(ai.clone(), None, AgenticSettings::default());
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert!(result.search_unavailable);
        assert!(ai.embedded().is_empty(), "no search ever executed");
        assert!(
            ai.prompt(1)
                .contains("Search is unavailable (database not connected)."),
            "unavailable observation: {}",
            ai.prompt(1)
        );
        assert!(
            ai.prompt(2)
                .contains("Search is unavailable (database not connected)."),
            "subsequent SEARCH skipped with the same observation: {}",
            ai.prompt(2)
        );
        assert_eq!(result.stop_reason, AgenticStopReason::FinalAnswer);
    }

    #[tokio::test]
    async fn max_steps_one_returns_fallback() {
        let ai = ScriptedAi::new(vec![reply("SEARCH: \"alpha\"")]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(1, 0, 2, CompletionCheckMode::Warn, 0.60),
        );
        let result = svc.ask_project_agentic("why?", "").await.expect("ok");
        assert_eq!(result.stop_reason, AgenticStopReason::MaxSteps);
        assert_eq!(result.answer, AGENTIC_FALLBACK_ANSWER);
        assert_eq!(result.sources.len(), 1);
    }

    // --- payload / cost / D-rules ---------------------------------------------

    #[tokio::test]
    async fn agentic_calls_never_use_response_cache() {
        let ai = ScriptedAi::new(vec![
            reply("SEARCH: \"alpha\""),
            reply("FINAL ANSWER: The parser is bounded. It retries."),
            reply("FINAL ANSWER: The parser is bounded [S1]."),
        ]);
        let svc = service(
            ai.clone(),
            Some(db_with_one_hit()),
            settings(5, 0, 2, CompletionCheckMode::Enforce, 0.60),
        );
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        // Every call RED-directed (including the repair) and never cached.
        for config in ai.configs.lock().unwrap().iter() {
            assert_eq!(config.route_hint, "local:L");
            assert!(config.response_cache.is_none(), "D12: never cached");
        }
        assert_eq!(ai.configs.lock().unwrap().len(), ai.call_count());
    }

    #[tokio::test]
    async fn agentic_cost_recorded_once_per_call() {
        let db = db_with_one_hit();
        let ai = ScriptedAi::new(vec![
            ScriptedTurn::ReplyWithUsage {
                text: "SEARCH: \"alpha\"".to_string(),
                prompt: 100,
                candidates: 20,
                total: 120,
                cached: None,
            },
            ScriptedTurn::ReplyWithUsage {
                text: "FINAL ANSWER: done".to_string(),
                prompt: 200,
                candidates: 10,
                total: 210,
                cached: None,
            },
        ]);
        let svc = service(ai, Some(db.clone()), AgenticSettings::default());
        let _ = svc.ask_project_agentic("why?", "").await.expect("ok");
        let cost_records: Vec<String> = db
            .execute_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|sql| sql.contains("system:search_agentic"))
            .cloned()
            .collect();
        assert_eq!(cost_records.len(), 2);
        assert!(cost_records[0].contains("+= 120"), "{}", cost_records[0]);
        assert!(cost_records[1].contains("+= 210"), "{}", cost_records[1]);
    }

    // --- E15 contract tests (kept from the reviewed base) ---------------------

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

    // --- E14.5 prompt contract ------------------------------------------------

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

        assert_eq!(result.steps.len(), AgenticSettings::default().max_steps);
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
