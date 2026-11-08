//! Bounded decisions (D11, ddofds fase 3).
//!
//! Closed, repeated decisions ("is this observation noise?", "is this log
//! already known?") are asked as typed questions over a state: the model
//! returns a probability distribution per question and the caller derives a
//! confidence margin (`max - second`). The caller — never this module — owns
//! the abstention policy: auto when the margin is high, escalate to the
//! current generative path when it is not.
//!
//! Design pins:
//! - One single-turn call, no tools, no conversation state.
//! - Prompt order follows the D13 convention: byte-stable instructions →
//!   state → questions (the query) last.
//! - The response schema is strict: `{"<id>": {"probs": {"<option>": p}}}`.
//!   Invalid JSON, unknown/missing options or unnormalized probabilities are
//!   an abstention (the caller escalates), never a best-effort parse.
//! - `Noul` questions carry no option set and are skipped without a model
//!   call (forward-compat for question kinds that do not map to a closed
//!   choice yet).
//!
//! Routing: the default hint is `local:S` (the D10 tier-S local model). A
//! privacy-bound caller uses [`decide_with_hint`] with `local_only:S` so
//! personal content never leaves the machine.

use std::collections::{BTreeMap, HashSet};

use crate::traits::AIProvider;
use crate::types::{Content, GenerationConfig, UsageMetadata};

/// Default route hint for a bounded decision (D10 tier S local, cloud
/// fallback preserved by the `local:` base).
pub const DECIDE_ROUTE_HINT: &str = "local:S";

/// Effectively greedy sampling. The provider layer treats `0.0` as "unset"
/// (`GenerationConfig::default()`), so 0.0 would silently fall back to the
/// model default (often 0.8); 0.001 is the smallest temperature that is
/// actually transmitted.
pub const DECIDE_TEMPERATURE: f64 = 0.001;

/// Default auto threshold (θ_auto) for callers implementing the abstention
/// policy. Calibrated from `ibis eval decisions`; not enforced here.
pub const DEFAULT_AUTO_THRESHOLD: f64 = 0.75;

/// Default escalate floor (θ_escalate) for callers implementing the
/// abstention policy. Calibrated from `ibis eval decisions`; not enforced
/// here.
pub const DEFAULT_ESCALATE_THRESHOLD: f64 = 0.40;

/// Probability tolerance for `sum(probs) == 1` (providers round to a few
/// decimals; a genuinely unnormalized distribution is an abstention).
const PROB_SUM_EPSILON: f64 = 1e-3;

/// One typed question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecideQuestion {
    pub id: String,
    pub kind: DecideKind,
    /// Caller-owned rubric/instruction text. Kept out of [`DecideQuestion::id`]
    /// so the same question id can be calibrated across rubric revisions.
    pub instructions: String,
}

/// The question shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecideKind {
    /// No option set: declared but not answerable as a distribution.
    /// [`decide`] skips it without a model call.
    Noul,
    /// Closed option set: `(option id, description)`.
    Choice(Vec<(String, String)>),
}

impl DecideQuestion {
    /// Builds a closed-choice question from `(id, description)` pairs.
    pub fn choice(id: &str, instructions: &str, options: &[(&str, &str)]) -> Self {
        DecideQuestion {
            id: id.to_string(),
            kind: DecideKind::Choice(
                options
                    .iter()
                    .map(|(option, description)| (option.to_string(), description.to_string()))
                    .collect(),
            ),
            instructions: instructions.to_string(),
        }
    }

    /// Builds a question with no option set (skipped by [`decide`]).
    pub fn noul(id: &str, instructions: &str) -> Self {
        DecideQuestion {
            id: id.to_string(),
            kind: DecideKind::Noul,
            instructions: instructions.to_string(),
        }
    }
}

/// One answered question.
#[derive(Debug, Clone, PartialEq)]
pub struct DecideAnswer {
    /// Option id → probability (deterministic `BTreeMap` order).
    pub probs: BTreeMap<String, f64>,
    /// Highest-probability option (first in sorted order on ties).
    pub best: String,
    /// `max - second` (the abstention margin; `max` when there is one option).
    pub confidence: f64,
}

/// The typed answer set plus the provider usage for billing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecideResult {
    pub answers: BTreeMap<String, DecideAnswer>,
    pub usage: Option<UsageMetadata>,
}

/// Asks typed questions about `state` through the default `local:S` route.
pub async fn decide(
    state: &str,
    questions: &[DecideQuestion],
    ai: &dyn AIProvider,
) -> Result<DecideResult, String> {
    decide_with_hint(state, questions, ai, DECIDE_ROUTE_HINT).await
}

/// Asks typed questions about `state` with an explicit route hint.
///
/// Privacy-bound callers pass `local_only:S`; every other caller should use
/// [`decide`].
pub async fn decide_with_hint(
    state: &str,
    questions: &[DecideQuestion],
    ai: &dyn AIProvider,
    route_hint: &str,
) -> Result<DecideResult, String> {
    validate_questions(questions)?;
    let active: Vec<&DecideQuestion> = questions
        .iter()
        .filter(|question| matches!(question.kind, DecideKind::Choice(_)))
        .collect();
    if active.is_empty() {
        return Ok(DecideResult::default());
    }
    if state.trim().is_empty() {
        return Err("decide: empty state".to_string());
    }
    let prompt = build_decide_prompt(state, &active)?;
    let response = ai
        .generate_content(
            &[Content::text(prompt)],
            GenerationConfig {
                temperature: DECIDE_TEMPERATURE,
                route_hint: route_hint.to_string(),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    let text = response.content.joined_text();
    let answers = parse_decide_response(&text, &active)?;
    Ok(DecideResult {
        answers,
        usage: response.usage_metadata,
    })
}

fn validate_questions(questions: &[DecideQuestion]) -> Result<(), String> {
    if questions.is_empty() {
        return Err("decide: no questions".to_string());
    }
    let mut ids: HashSet<&str> = HashSet::new();
    for question in questions {
        let id = question.id.trim();
        if id.is_empty() {
            return Err("decide: empty question id".to_string());
        }
        if !ids.insert(id) {
            return Err(format!("decide: duplicate question id {id:?}"));
        }
        if let DecideKind::Choice(options) = &question.kind {
            if options.is_empty() {
                return Err(format!("decide: question {id:?} has no options"));
            }
            let mut option_ids: HashSet<&str> = HashSet::new();
            for (option, _) in options {
                let option = option.trim();
                if option.is_empty() {
                    return Err(format!("decide: question {id:?} has an empty option id"));
                }
                if !option_ids.insert(option) {
                    return Err(format!(
                        "decide: question {id:?} repeats option id {option:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// D13 prompt stability: byte-stable instructions block for every bounded
/// decision. Everything dynamic (state, questions) follows it; the questions —
/// the query — go last. No timestamp or request-id may ever enter this const.
pub const DECIDE_INSTRUCTIONS: &str = r#"You are a bounded decision classifier.
You receive a STATE and one or more typed QUESTIONS. Answer each question by distributing probability mass over its declared options.

Rules:
- Output ONLY a JSON object, with no prose and no markdown fences.
- Top-level keys are the question ids.
- Each question value is an object with a "probs" object.
- "probs" keys are exactly the option ids declared for that question.
- Every probability is a number in [0, 1] and the values sum to 1 per question.
- Base the distribution only on the STATE; do not invent facts.
- When the STATE is ambiguous, spread the mass instead of forcing certainty.

Schema:
{"<question_id>": {"probs": {"<option_id>": <probability>}}}"#;

#[derive(serde::Serialize)]
struct WireQuestion<'a> {
    id: &'a str,
    instructions: &'a str,
    options: Vec<WireOption<'a>>,
}

#[derive(serde::Serialize)]
struct WireOption<'a> {
    id: &'a str,
    description: &'a str,
}

/// Assembles the decide prompt: byte-stable instructions → state → questions
/// (the query) last (D13).
fn build_decide_prompt(state: &str, questions: &[&DecideQuestion]) -> Result<String, String> {
    let wire: Vec<WireQuestion<'_>> = questions
        .iter()
        .map(|question| {
            let options = match &question.kind {
                DecideKind::Choice(options) => options
                    .iter()
                    .map(|(id, description)| WireOption {
                        id: id.trim(),
                        description,
                    })
                    .collect(),
                DecideKind::Noul => Vec::new(),
            };
            WireQuestion {
                id: question.id.trim(),
                instructions: &question.instructions,
                options,
            }
        })
        .collect();
    let questions_json =
        serde_json::to_string(&wire).map_err(|e| format!("decide: serialize questions: {e}"))?;
    let mut prompt =
        String::with_capacity(DECIDE_INSTRUCTIONS.len() + state.len() + questions_json.len() + 32);
    prompt.push_str(DECIDE_INSTRUCTIONS);
    prompt.push_str("\n\nSTATE:\n");
    prompt.push_str(state.trim());
    prompt.push_str("\n\nQUESTIONS:\n");
    prompt.push_str(&questions_json);
    Ok(prompt)
}

/// Strips a leading/trailing markdown fence; models add one despite the
/// instructions. Anything else (prose before/after the object) stays invalid.
fn strip_code_fences(text: &str) -> &str {
    let mut text = text.trim();
    if let Some(stripped) = text.strip_prefix("```json") {
        text = stripped;
    } else if let Some(stripped) = text.strip_prefix("```") {
        text = stripped;
    }
    if let Some(stripped) = text.strip_suffix("```") {
        text = stripped;
    }
    text.trim()
}

fn parse_decide_response(
    text: &str,
    questions: &[&DecideQuestion],
) -> Result<BTreeMap<String, DecideAnswer>, String> {
    let text = strip_code_fences(text);
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("decide: invalid JSON response: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "decide: response is not a JSON object".to_string())?;
    // Single-question tolerance: accept the bare `{"probs": {...}}` shape
    // some models emit instead of wrapping it in the question id.
    let bare_single = questions.len() == 1 && object.contains_key("probs");
    let mut answers = BTreeMap::new();
    for question in questions {
        let options = match &question.kind {
            DecideKind::Choice(options) => options,
            DecideKind::Noul => continue,
        };
        let answer_object = if bare_single {
            object
        } else {
            object
                .get(question.id.trim())
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| format!("decide: answer for question {:?} missing", question.id))?
        };
        let probs_object = answer_object
            .get("probs")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| format!("decide: question {:?} has no \"probs\" object", question.id))?;
        if probs_object.len() != options.len() {
            return Err(format!(
                "decide: question {:?} returned {} probabilities for {} options",
                question.id,
                probs_object.len(),
                options.len()
            ));
        }
        let mut probs: BTreeMap<String, f64> = BTreeMap::new();
        for (option, _) in options {
            let option = option.trim();
            let raw = probs_object.get(option).ok_or_else(|| {
                format!(
                    "decide: question {:?} is missing option {:?}",
                    question.id, option
                )
            })?;
            let probability = raw.as_f64().ok_or_else(|| {
                format!(
                    "decide: question {:?} option {:?} is not a number",
                    question.id, option
                )
            })?;
            if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
                return Err(format!(
                    "decide: question {:?} option {:?} probability {probability} is outside [0, 1]",
                    question.id, option
                ));
            }
            probs.insert(option.to_string(), probability);
        }
        let sum: f64 = probs.values().sum();
        if (sum - 1.0).abs() > PROB_SUM_EPSILON {
            return Err(format!(
                "decide: question {:?} probabilities sum to {sum}, not 1",
                question.id
            ));
        }
        let (best, confidence) = best_and_confidence(&probs);
        answers.insert(
            question.id.trim().to_string(),
            DecideAnswer {
                probs,
                best,
                confidence,
            },
        );
    }
    Ok(answers)
}

/// Highest-probability option (first in `BTreeMap` order on ties) and the
/// margin over the runner-up.
fn best_and_confidence(probs: &BTreeMap<String, f64>) -> (String, f64) {
    let mut best: Option<(&String, f64)> = None;
    let mut second = 0.0f64;
    for (option, probability) in probs {
        match best {
            None => best = Some((option, *probability)),
            Some((_, best_probability)) if *probability > best_probability => {
                second = best_probability;
                best = Some((option, *probability));
            }
            Some(_) => {
                if *probability > second {
                    second = *probability;
                }
            }
        }
    }
    match best {
        Some((option, probability)) => (option.clone(), (probability - second).clamp(0.0, 1.0)),
        None => (String::new(), 0.0),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::error::AiError;
    use crate::types::{Candidate, Part};

    struct ScriptedAi {
        response: String,
        error: Option<String>,
        usage: Option<UsageMetadata>,
        calls: Mutex<Vec<(String, GenerationConfig)>>,
    }

    impl ScriptedAi {
        fn new(response: &str) -> Self {
            ScriptedAi {
                response: response.to_string(),
                error: None,
                usage: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            ScriptedAi {
                response: String::new(),
                error: Some(message.to_string()),
                usage: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn prompt(&self) -> String {
            self.calls.lock().unwrap()[0].0.clone()
        }

        fn config(&self) -> GenerationConfig {
            self.calls.lock().unwrap()[0].1.clone()
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

        async fn generate_content(
            &self,
            contents: &[Content],
            config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            let prompt = contents
                .first()
                .map(Content::joined_text)
                .unwrap_or_default();
            self.calls.lock().unwrap().push((prompt, config));
            if let Some(error) = &self.error {
                return Err(AiError::other(error.clone()));
            }
            Ok(Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![Part {
                        text: self.response.clone(),
                    }],
                },
                usage_metadata: self.usage.clone(),
                ..Default::default()
            })
        }
    }

    fn is_noise() -> DecideQuestion {
        DecideQuestion::choice(
            "is_noise",
            "Is the observation noise or durable?",
            &[
                ("noise", "routine, transient, no durable value"),
                ("durable", "a durable project fact"),
            ],
        )
    }

    #[tokio::test]
    async fn parses_valid_distribution_and_margin() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":0.8,"durable":0.2}}}"#);
        let result = decide("observation", &[is_noise()], &ai).await.unwrap();
        let answer = result.answers.get("is_noise").unwrap();
        assert_eq!(answer.best, "noise");
        assert!((answer.confidence - 0.6).abs() < 1e-9);
        assert_eq!(answer.probs.get("noise"), Some(&0.8));
    }

    #[tokio::test]
    async fn single_question_inner_object_is_accepted() {
        let ai = ScriptedAi::new(r#"{"probs":{"noise":0.1,"durable":0.9}}"#);
        let result = decide("observation", &[is_noise()], &ai).await.unwrap();
        assert_eq!(result.answers.get("is_noise").unwrap().best, "durable");
    }

    #[tokio::test]
    async fn fences_are_stripped() {
        let ai = ScriptedAi::new(
            "```json\n{\"is_noise\":{\"probs\":{\"noise\":1.0,\"durable\":0.0}}}\n```",
        );
        let result = decide("observation", &[is_noise()], &ai).await.unwrap();
        assert_eq!(result.answers.get("is_noise").unwrap().best, "noise");
    }

    #[tokio::test]
    async fn invalid_json_abstains() {
        let ai = ScriptedAi::new("the answer is noise");
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert!(error.contains("invalid JSON"), "{error}");
    }

    #[tokio::test]
    async fn unnormalized_probabilities_abstain() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":0.8,"durable":0.8}}}"#);
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert!(error.contains("sum to 1.6"), "{error}");
    }

    #[tokio::test]
    async fn unknown_or_missing_option_abstains() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":0.8,"other":0.2}}}"#);
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert!(error.contains("missing option \"durable\""), "{error}");

        let ai =
            ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":0.8,"durable":0.1,"other":0.1}}}"#);
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert!(error.contains("3 probabilities for 2 options"), "{error}");
    }

    #[tokio::test]
    async fn out_of_range_probability_abstains() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.2,"durable":-0.2}}}"#);
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert!(error.contains("outside [0, 1]"), "{error}");
    }

    #[tokio::test]
    async fn missing_answer_abstains() {
        let ai = ScriptedAi::new(r#"{"other":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert!(
            error.contains("answer for question \"is_noise\" missing"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn noul_only_questions_skip_the_model() {
        let ai = ScriptedAi::new("{}");
        let result = decide(
            "observation",
            &[DecideQuestion::noul("score", "rate it")],
            &ai,
        )
        .await
        .unwrap();
        assert!(result.answers.is_empty());
        assert!(ai.calls.lock().unwrap().is_empty(), "no model call");
    }

    #[tokio::test]
    async fn mixed_noul_and_choice_sends_only_the_choice() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        let result = decide(
            "observation",
            &[DecideQuestion::noul("score", "rate it"), is_noise()],
            &ai,
        )
        .await
        .unwrap();
        assert_eq!(result.answers.len(), 1);
        assert!(!ai.prompt().contains("\"score\""));
    }

    #[tokio::test]
    async fn duplicate_question_ids_are_rejected() {
        let ai = ScriptedAi::new("{}");
        let error = decide("observation", &[is_noise(), is_noise()], &ai)
            .await
            .unwrap_err();
        assert!(error.contains("duplicate question id"), "{error}");
        assert!(ai.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_state_is_rejected_without_a_model_call() {
        let ai = ScriptedAi::new("{}");
        let error = decide("   ", &[is_noise()], &ai).await.unwrap_err();
        assert!(error.contains("empty state"), "{error}");
        assert!(ai.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn choice_without_options_is_rejected() {
        let ai = ScriptedAi::new("{}");
        let question = DecideQuestion::choice("empty", "no options", &[]);
        let error = decide("observation", &[question], &ai).await.unwrap_err();
        assert!(error.contains("has no options"), "{error}");
    }

    #[tokio::test]
    async fn local_s_hint_and_near_greedy_temperature() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        decide("observation", &[is_noise()], &ai).await.unwrap();
        let config = ai.config();
        assert_eq!(config.route_hint, "local:S");
        assert_eq!(config.temperature, DECIDE_TEMPERATURE);
        assert!(config.temperature > 0.0, "0.0 is the unset sentinel");
    }

    #[tokio::test]
    async fn explicit_hint_is_honored_for_privacy_bound_calls() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        decide_with_hint("observation", &[is_noise()], &ai, "local_only:S")
            .await
            .unwrap();
        assert_eq!(ai.config().route_hint, "local_only:S");
    }

    #[tokio::test]
    async fn prompt_prefix_is_byte_stable_and_questions_are_last() {
        let ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        decide("observation A", &[is_noise()], &ai).await.unwrap();
        let first = ai.prompt();

        let ai_b = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        decide("observation B\nlonger", &[is_noise()], &ai_b)
            .await
            .unwrap();
        let second = ai_b.prompt();

        assert_eq!(&first[..DECIDE_INSTRUCTIONS.len()], DECIDE_INSTRUCTIONS);
        assert_eq!(&second[..DECIDE_INSTRUCTIONS.len()], DECIDE_INSTRUCTIONS);
        assert_ne!(first, second);
        assert!(first.contains("\n\nSTATE:\nobservation A\n\nQUESTIONS:\n"));
        assert!(second.ends_with(
            r#"[{"id":"is_noise","instructions":"Is the observation noise or durable?","options":[{"id":"noise","description":"routine, transient, no durable value"},{"id":"durable","description":"a durable project fact"}]}]"#
        ));
    }

    #[tokio::test]
    async fn usage_is_returned_for_billing() {
        let mut ai = ScriptedAi::new(r#"{"is_noise":{"probs":{"noise":1.0,"durable":0.0}}}"#);
        ai.usage = Some(UsageMetadata {
            prompt_token_count: 12,
            candidates_token_count: 3,
            total_token_count: 15,
            cached_content_token_count: None,
        });
        let result = decide("observation", &[is_noise()], &ai).await.unwrap();
        assert_eq!(result.usage.unwrap().prompt_token_count, 12);
    }

    #[tokio::test]
    async fn provider_error_abstains() {
        let ai = ScriptedAi::failing("offline");
        let error = decide("observation", &[is_noise()], &ai).await.unwrap_err();
        assert_eq!(error, "offline");
    }

    #[test]
    fn single_option_confidence_is_the_probability() {
        let mut probs = BTreeMap::new();
        probs.insert("only".to_string(), 1.0);
        let (best, confidence) = best_and_confidence(&probs);
        assert_eq!(best, "only");
        assert!((confidence - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ties_resolve_to_the_sorted_first_option() {
        let mut probs = BTreeMap::new();
        probs.insert("b".to_string(), 0.5);
        probs.insert("a".to_string(), 0.5);
        let (best, confidence) = best_and_confidence(&probs);
        assert_eq!(best, "a");
        assert!(confidence.abs() < 1e-9);
    }
}
