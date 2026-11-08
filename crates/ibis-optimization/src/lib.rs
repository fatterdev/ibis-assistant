//! ibis-optimization — knowledge-base optimization (
//! `internal/optimization`: + ).
//!
//! [`Optimizer`] drives two detached loops consumed by the MCP tools
//! `memory_consolidate` and `optimize_knowledge`:
//!
//! - **Consolidation** ([`Optimizer::consolidate_loop`]): drains raw
//!   observations oldest-first, distills durable typed memories via the AI
//!   provider (`local:M`, personal observations `local_only:M`), merges
//!   semantic duplicates, and gives routine noise a short stale lifetime. An
//!   item is left untouched when classification or persistence fails. D11
//!   shadow mode computes the bounded `is_noise` decision next to the
//!   generative verdict and logs `decide_shadow` without changing behavior.
//! - **RAFT** ([`Optimizer::optimize_loop`]): generates a synthetic question
//!   per random code chunk and evaluates retrieval; hits reinforce the chunk
//!   (`access_count`/`last_accessed`).
//!
//! Parity notes / deliberate deltas:
//! - Personal observations never leave the machine: their distillation hint is
//!   `local_only` and the duplicate lookup is restricted to same-owner
//!   personal rows (parity, test-pinned).
//! - F-002 (D17 estate): the duplicate-lookup embedding and the personal
//!   owner bind as variables instead of being interpolated; the observation
//!   id stays inline only after `is_safe_record_id` vetting (identifier rule).
//! - The consolidation transaction keeps the original shapes: `RELATE` (merge only)
//!   after the endpoints are persisted (D17 R4) and `UPDATE` on the
//!   just-selected observation row (it exists — 3.x semantics hold).

pub mod decide_eval;
pub mod eval;
pub mod memory_consolidate;
pub mod raft;
pub mod store;

pub use decide_eval::{
    build_report as build_decision_report, load_dataset as load_decision_dataset,
    DecisionAggregate, DecisionCandidate, DecisionCase, DecisionCaseResult, DecisionDataset,
    DecisionEvalRunner, DecisionReport,
};
pub use eval::{
    build_report, load_dataset, seed_golden_estate, EvalCase, EvalReport, OfflineEmbedder,
    RetrievalDataset, RetrievalEvalRunner,
};
pub use memory_consolidate::{
    build_is_noise_state, decide_shadow_record, is_noise_question, parse_consolidation_decision,
    ConsolidationDecision, ConsolidationObservation, DecideShadow, DuplicateMemory,
    IS_NOISE_QUESTION_ID, IS_NOISE_RUBRIC_VERSION,
};
pub use raft::Optimizer;
pub use store::DbCostStore;
