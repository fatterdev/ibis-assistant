//! Bounded-decision calibration harness (D11, ddofds fase 3).
//!
//! Labeled dataset (`evaluations/decisions/cases.yaml`) + runner over the
//! [`decide`](ibis_ai::decide::decide) primitive. The runner builds the exact
//! production `is_noise` state/question pair (shadow mode and calibration
//! share [`build_is_noise_state`]/[`is_noise_question`]), asks the model once
//! per case, and reports agreement plus the auto/escalate/default buckets at
//! the caller-side thresholds. Errors (degraded model) are per-case and
//! excluded from the means, never ranking failures.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use ibis_ai::decide::{decide, DecideKind, DecideResult};
use ibis_ai::AIProvider;

use crate::eval::CaseStatus;
use crate::memory_consolidate::{
    build_is_noise_state, is_noise_question, DuplicateMemory, IS_NOISE_QUESTION_ID,
    IS_NOISE_RUBRIC_VERSION,
};

// ---------------------------------------------------------------------------
// dataset
// ---------------------------------------------------------------------------

/// One candidate memory offered to the `is_noise` decision (the same window
/// the consolidation duplicate lookup produces).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DecisionCandidate {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
}

/// One labeled calibration case.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecisionCase {
    /// Unique case id (`d01_…`).
    pub id: String,
    /// The raw observation the consolidation would distill.
    pub observation: String,
    /// Existing same-topic memories (empty when the estate is fresh).
    #[serde(default)]
    pub candidates: Vec<DecisionCandidate>,
    /// Expected `is_noise` answer (`noise` | `durable`).
    pub expected: String,
    /// Free-form provenance note (which scenario the case pins).
    #[serde(default)]
    pub note: String,
}

/// The parsed `cases.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DecisionDataset {
    /// Dataset schema version (currently `1`).
    #[serde(default = "default_version")]
    pub version: String,
    /// Rubric the labels were written against; must match the production
    /// rubric (`is_noise.v1`).
    #[serde(default = "default_rubric")]
    pub rubric: String,
    pub cases: Vec<DecisionCase>,
}

fn default_version() -> String {
    "1".to_string()
}

fn default_rubric() -> String {
    IS_NOISE_RUBRIC_VERSION.to_string()
}

/// The option ids declared by the production `is_noise` question.
pub fn expected_options() -> Vec<String> {
    match is_noise_question().kind {
        DecideKind::Choice(options) => options.into_iter().map(|(id, _)| id).collect(),
        DecideKind::Noul => Vec::new(),
    }
}

impl DecisionDataset {
    /// Strict validation: cases present, unique ids, non-empty observations,
    /// expected labels among the production options, and a rubric that
    /// matches the code. Called by [`load_dataset`].
    pub fn validate(&self) -> Result<(), String> {
        if self.rubric != IS_NOISE_RUBRIC_VERSION {
            return Err(format!(
                "dataset rubric {:?} does not match the production rubric {:?}",
                self.rubric, IS_NOISE_RUBRIC_VERSION
            ));
        }
        if self.cases.is_empty() {
            return Err("dataset has no cases".to_string());
        }
        let options = expected_options();
        let mut seen = std::collections::HashSet::new();
        for case in &self.cases {
            let id = case.id.trim();
            if id.is_empty() {
                return Err("case id is empty".to_string());
            }
            if !seen.insert(id.to_string()) {
                return Err(format!("duplicate case id: {id}"));
            }
            if case.observation.trim().is_empty() {
                return Err(format!("case {id} has an empty observation"));
            }
            if !options.iter().any(|option| option == &case.expected) {
                return Err(format!(
                    "case {id} expects {:?}, not one of {:?}",
                    case.expected, options
                ));
            }
        }
        Ok(())
    }
}

/// Reads and validates the YAML dataset.
pub fn load_dataset(path: impl AsRef<Path>) -> Result<DecisionDataset, String> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read dataset {}: {e}", path.display()))?;
    let dataset: DecisionDataset =
        serde_yaml::from_str(&text).map_err(|e| format!("failed to parse dataset: {e}"))?;
    dataset.validate()?;
    Ok(dataset)
}

// ---------------------------------------------------------------------------
// runner
// ---------------------------------------------------------------------------

/// Per-case outcome.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionCaseResult {
    pub id: String,
    pub status: CaseStatus,
    pub expected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best: Option<String>,
    pub confidence: f64,
    pub probs: BTreeMap<String, f64>,
    pub agreement: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DecisionCaseResult {
    fn error(case: &DecisionCase, error: String) -> Self {
        DecisionCaseResult {
            id: case.id.clone(),
            status: CaseStatus::Error,
            expected: case.expected.clone(),
            best: None,
            confidence: 0.0,
            probs: BTreeMap::new(),
            agreement: false,
            error: Some(error),
        }
    }
}

/// Scores the labeled cases against the bounded-decision primitive.
pub struct DecisionEvalRunner {
    ai: Arc<dyn AIProvider>,
}

impl DecisionEvalRunner {
    pub fn new(ai: Arc<dyn AIProvider>) -> Self {
        DecisionEvalRunner { ai }
    }

    /// Runs one case through `decide()` with the production state builder.
    pub async fn run_case(&self, case: &DecisionCase) -> DecisionCaseResult {
        let duplicates: Vec<DuplicateMemory> = case
            .candidates
            .iter()
            .map(|candidate| DuplicateMemory {
                id: candidate.id.clone(),
                kind: candidate.kind.clone(),
                title: candidate.title.clone(),
                content: candidate.content.clone(),
                score: 0.0,
            })
            .collect();
        let state = build_is_noise_state(&case.observation, &duplicates);
        match decide(&state, &[is_noise_question()], self.ai.as_ref()).await {
            Ok(result) => self.score(case, result),
            Err(error) => DecisionCaseResult::error(case, error),
        }
    }

    fn score(&self, case: &DecisionCase, result: DecideResult) -> DecisionCaseResult {
        let Some(answer) = result.answers.get(IS_NOISE_QUESTION_ID) else {
            return DecisionCaseResult::error(
                case,
                format!("decide({IS_NOISE_QUESTION_ID}): answer missing from the result"),
            );
        };
        DecisionCaseResult {
            id: case.id.clone(),
            status: CaseStatus::Ok,
            expected: case.expected.clone(),
            best: Some(answer.best.clone()),
            confidence: answer.confidence,
            probs: answer.probs.clone(),
            agreement: answer.best == case.expected,
            error: None,
        }
    }

    /// Runs every case sequentially and aggregates.
    pub async fn run(&self, dataset: &DecisionDataset) -> Vec<DecisionCaseResult> {
        let mut results = Vec::with_capacity(dataset.cases.len());
        for case in &dataset.cases {
            tracing::debug!("eval: running decision case {}", case.id);
            results.push(self.run_case(case).await);
        }
        results
    }
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DecisionAggregate {
    pub cases: usize,
    pub cases_ok: usize,
    pub cases_error: usize,
    /// Fraction of ok cases whose argmax matches the label.
    pub agreement: f64,
    pub mean_confidence: f64,
    /// Ok cases at or above the auto threshold.
    pub auto_cases: usize,
    /// Agreement within the auto bucket.
    pub auto_accuracy: f64,
    /// Ok cases in `[escalate_threshold, auto_threshold)`.
    pub escalated_cases: usize,
    /// Ok cases below the escalate floor.
    pub default_cases: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionReport {
    /// Dataset path as passed on the CLI.
    pub dataset: String,
    pub version: String,
    /// Rubric the labels were written against.
    pub rubric: String,
    /// Provider name (`ollama` | `openai_compat`).
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub auto_threshold: f64,
    pub escalate_threshold: f64,
    pub aggregate: DecisionAggregate,
    pub cases: Vec<DecisionCaseResult>,
}

impl DecisionReport {
    /// `true` when the agreement misses the gate (`--min-agreement`).
    pub fn below_threshold(&self, min_agreement: f64) -> bool {
        self.aggregate.cases_ok > 0 && self.aggregate.agreement < min_agreement
    }
}

/// Aggregates per-case results into a report (means over ok cases only).
#[allow(clippy::too_many_arguments)]
pub fn build_report(
    dataset: &str,
    version: &str,
    rubric: &str,
    provider: &str,
    model: Option<&str>,
    auto_threshold: f64,
    escalate_threshold: f64,
    cases: Vec<DecisionCaseResult>,
) -> DecisionReport {
    let ok: Vec<&DecisionCaseResult> = cases
        .iter()
        .filter(|case| case.status == CaseStatus::Ok)
        .collect();
    let n = ok.len() as f64;
    let agreement = if n == 0.0 {
        0.0
    } else {
        ok.iter().filter(|case| case.agreement).count() as f64 / n
    };
    let mean_confidence = if n == 0.0 {
        0.0
    } else {
        ok.iter().map(|case| case.confidence).sum::<f64>() / n
    };
    let auto: Vec<&DecisionCaseResult> = ok
        .iter()
        .copied()
        .filter(|case| case.confidence >= auto_threshold)
        .collect();
    let auto_accuracy = if auto.is_empty() {
        0.0
    } else {
        auto.iter().filter(|case| case.agreement).count() as f64 / auto.len() as f64
    };
    let escalated_cases = ok
        .iter()
        .filter(|case| case.confidence >= escalate_threshold && case.confidence < auto_threshold)
        .count();
    let default_cases = ok
        .iter()
        .filter(|case| case.confidence < escalate_threshold)
        .count();
    DecisionReport {
        dataset: dataset.to_string(),
        version: version.to_string(),
        rubric: rubric.to_string(),
        provider: provider.to_string(),
        model: model.map(str::to_string),
        auto_threshold,
        escalate_threshold,
        aggregate: DecisionAggregate {
            cases: cases.len(),
            cases_ok: ok.len(),
            cases_error: cases.len() - ok.len(),
            agreement,
            mean_confidence,
            auto_cases: auto.len(),
            auto_accuracy,
            escalated_cases,
            default_cases,
        },
        cases,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use ibis_ai::types::{Candidate, Content, GenerationConfig, Part};
    use ibis_ai::AiError;

    struct ScriptedAi {
        response: String,
        calls: Mutex<usize>,
    }

    impl ScriptedAi {
        fn new(response: &str) -> Self {
            ScriptedAi {
                response: response.to_string(),
                calls: Mutex::new(0),
            }
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
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            *self.calls.lock().unwrap() += 1;
            Ok(Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![Part {
                        text: self.response.clone(),
                    }],
                },
                ..Default::default()
            })
        }
    }

    fn case(id: &str, expected: &str) -> DecisionCase {
        DecisionCase {
            id: id.to_string(),
            observation: "Observation body.".to_string(),
            candidates: Vec::new(),
            expected: expected.to_string(),
            note: String::new(),
        }
    }

    #[test]
    fn committed_dataset_is_valid_and_balanced() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evaluations/decisions/cases.yaml");
        let dataset = load_dataset(&path).expect("committed decision dataset must validate");
        assert!(
            dataset.cases.len() >= 30,
            "calibration dataset needs at least 30 cases, got {}",
            dataset.cases.len()
        );
        let noise = dataset
            .cases
            .iter()
            .filter(|case| case.expected == "noise")
            .count();
        let durable = dataset
            .cases
            .iter()
            .filter(|case| case.expected == "durable")
            .count();
        assert!(noise >= 10, "too few noise cases: {noise}");
        assert!(durable >= 10, "too few durable cases: {durable}");
    }

    #[test]
    fn dataset_rejects_rubric_mismatch() {
        let dataset = DecisionDataset {
            version: "1".into(),
            rubric: "is_noise.v0".into(),
            cases: vec![case("d1", "noise")],
        };
        assert!(dataset.validate().unwrap_err().contains("rubric"));
    }

    #[test]
    fn dataset_rejects_unknown_expected_and_duplicates() {
        let dataset = DecisionDataset {
            version: "1".into(),
            rubric: IS_NOISE_RUBRIC_VERSION.into(),
            cases: vec![case("d1", "maybe")],
        };
        assert!(dataset.validate().unwrap_err().contains("expects"));

        let dataset = DecisionDataset {
            version: "1".into(),
            rubric: IS_NOISE_RUBRIC_VERSION.into(),
            cases: vec![case("d1", "noise"), case("d1", "durable")],
        };
        assert!(dataset.validate().unwrap_err().contains("duplicate"));
    }

    #[tokio::test]
    async fn runner_scores_agreement() {
        let ai = Arc::new(ScriptedAi::new(
            r#"{"is_noise":{"probs":{"noise":0.9,"durable":0.1}}}"#,
        ));
        let runner = DecisionEvalRunner::new(ai.clone());
        let result = runner.run_case(&case("d1", "noise")).await;
        assert_eq!(result.status, CaseStatus::Ok);
        assert!(result.agreement);
        assert_eq!(result.best.as_deref(), Some("noise"));
        assert!((result.confidence - 0.8).abs() < 1e-9);
        assert_eq!(*ai.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn runner_surfaces_invalid_responses_as_errors() {
        let ai = Arc::new(ScriptedAi::new("not json"));
        let runner = DecisionEvalRunner::new(ai);
        let result = runner.run_case(&case("d1", "noise")).await;
        assert_eq!(result.status, CaseStatus::Error);
        assert!(result.error.unwrap().contains("invalid JSON"));
    }

    #[test]
    fn report_aggregates_agreement_and_threshold_buckets() {
        let cases = vec![
            DecisionCaseResult {
                id: "a".into(),
                status: CaseStatus::Ok,
                expected: "noise".into(),
                best: Some("noise".into()),
                confidence: 0.9,
                probs: BTreeMap::new(),
                agreement: true,
                error: None,
            },
            DecisionCaseResult {
                id: "b".into(),
                status: CaseStatus::Ok,
                expected: "durable".into(),
                best: Some("noise".into()),
                confidence: 0.5,
                probs: BTreeMap::new(),
                agreement: false,
                error: None,
            },
            DecisionCaseResult {
                id: "c".into(),
                status: CaseStatus::Ok,
                expected: "durable".into(),
                best: Some("durable".into()),
                confidence: 0.2,
                probs: BTreeMap::new(),
                agreement: true,
                error: None,
            },
            DecisionCaseResult::error(&case("d", "noise"), "offline".into()),
        ];
        let report = build_report(
            "cases.yaml",
            "1",
            IS_NOISE_RUBRIC_VERSION,
            "scripted",
            Some("test-model"),
            0.75,
            0.40,
            cases,
        );
        assert_eq!(report.aggregate.cases, 4);
        assert_eq!(report.aggregate.cases_ok, 3);
        assert_eq!(report.aggregate.cases_error, 1);
        assert!((report.aggregate.agreement - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(report.aggregate.auto_cases, 1);
        assert!((report.aggregate.auto_accuracy - 1.0).abs() < 1e-9);
        assert_eq!(report.aggregate.escalated_cases, 1);
        assert_eq!(report.aggregate.default_cases, 1);
        assert!(!report.below_threshold(0.6));
        assert!(report.below_threshold(0.7));
    }
}
