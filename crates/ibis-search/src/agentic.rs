//! Agentic ReAct loop: multi-step SEARCH / FINAL ANSWER
//! protocol over the hybrid ask. Parsing layers are pinned by the original test
//! estate (quoted multiline extraction, line-based fallback, chatter
//! stripping, empty-query nudge, no-action nudge, search-failure
//! continuation).

use regex::Regex;
use std::sync::OnceLock;

use ibis_ai::types::{Candidate, Content, Part};

use crate::error::{Result, SearchError};
use crate::search::{SearchResult, SearchService};

/// The outcome of an agentic search.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgenticResult {
    pub answer: String,
    pub sources: Vec<SearchResult>,
    #[serde(rename = "reasoning_steps")]
    pub steps: Vec<String>,
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

const AGENTIC_MAX_STEPS: usize = 5;

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
        let mut system_prompt = SYSTEM_PROMPT.to_string();
        if !branch_context.is_empty() {
            system_prompt.push_str(&format!(
                "\n\nTOPOLOGICAL CONTEXT:\nYou are currently operating on branch/commit: {branch_context}. Keep in mind that search results might come from different parallel branches. Use this context to provide cross-branch insights when useful."
            ));
        }

        let mut history: Vec<Content> = vec![Content {
            role: "user".to_string(),
            parts: vec![Part {
                text: format!("{system_prompt}\n\nQuestion: {query}"),
            }],
        }];

        let mut result = AgenticResult::default();
        let mut unique_sources: std::collections::HashMap<String, SearchResult> =
            std::collections::HashMap::new();

        for i in 0..AGENTIC_MAX_STEPS {
            let candidate: Candidate = ai
                .generate_content(
                    &history,
                    ibis_ai::types::GenerationConfig {
                        temperature: 0.2,
                        max_output_tokens: 2000,
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
                        let observation = format_observation(&search_results);
                        for r in search_results {
                            unique_sources.insert(r.id.clone(), r);
                        }
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

        // Flatten sources.
        result.sources = unique_sources.into_values().collect();

        if result.answer.is_empty() {
            result.answer =
                "I could not find a definitive answer after multiple steps. Please try a more specific question."
                    .to_string();
        }

        Ok(result)
    }
}

/// Formats the OBSERVATION block from the collected sources
/// (the inline `obs` builder). Content is truncated to 2000 runes.
fn format_observation(results: &[SearchResult]) -> String {
    let mut obs = String::from("OBSERVATION: Found the following evidence:\n");
    if results.is_empty() {
        obs.push_str("No results found.\n");
    }
    for r in results {
        let mut content_snippet = r.content.clone();
        let runes: Vec<char> = content_snippet.chars().collect();
        if runes.len() > 2000 {
            content_snippet = runes[..2000].iter().collect::<String>() + "...(truncated)";
        }
        obs.push_str(&format!(
            "- {} (Score: {:.2}, source_kind={}, is_current={}",
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
