//! `ibis eval retrieval` — in-process retrieval evaluation (A1, ddofds
//! fase 0; plan in `docs/planning/2026-11-02-a1-retrieval-eval-plan.md`).
//!
//! `ibis eval decisions` — bounded-decision calibration (D11, ddofds fase 3):
//! runs the labeled `is_noise` dataset through the `decide()` primitive.
//!
//! Intercepted in `main.rs` BEFORE the MCP CLI passthrough: the evals mount
//! their dependencies directly (no running server, no new MCP tool — the
//! registry stays at 58 tools). The retrieval eval (re)seeds a dedicated
//! scratch database; the decisions eval talks to a chat provider selected on
//! the CLI. Both print a JSON report and exit non-zero under their quality
//! gate.

use std::sync::Arc;

use async_trait::async_trait;
use ibis_ai::types::{Candidate, Content, GenerationConfig};
use ibis_ai::{AIProvider, AiError};
use ibis_db::executor::{Executor, SurrealDbExecutor};
use ibis_optimization::decide_eval::{
    build_report as build_decision_report, load_dataset as load_decision_dataset,
    DecisionEvalRunner,
};
use ibis_optimization::eval::{
    build_report, load_dataset, seed_golden_estate, OfflineEmbedder, RetrievalEvalRunner,
};
use ibis_search::SearchService;

const DEFAULT_SURREAL_URL: &str = "ws://127.0.0.1:38000";
const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_EMBEDDING_MODEL: &str = "nomic-embed-text";
/// Local scoring model for `--rerank local` (tier-M class, the L1/L3 pick
/// for judging; `--rerank-model` overrides).
const DEFAULT_RERANK_MODEL: &str = "qwen2.5-coder:7b";
/// D11 calibration defaults: the tier-S local model and its route.
const DEFAULT_DECIDE_MODEL: &str = "granite4.1:3b";
const DEFAULT_DECIDE_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_OPENAI_COMPAT_URL: &str = "https://api.openai.com/v1";

enum EvalTarget {
    Retrieval(EvalParams),
    Decisions(DecisionParams),
}

/// `ibis eval decisions` knobs (D11 calibration runner).
struct DecisionParams {
    dataset: String,
    min_agreement: f64,
    /// `ollama` | `openai_compat`.
    provider: String,
    model: String,
    url: String,
    api_key: String,
    auto_threshold: f64,
    escalate_threshold: f64,
    out: Option<String>,
}

pub struct EvalParams {
    dataset: String,
    min_recall_5: f64,
    url: String,
    ns: String,
    db: String,
    user: String,
    pass: String,
    embedder: String,
    ollama_url: String,
    /// Channel fusion under test (additive|rrf — A2 validation).
    fusion: String,
    /// Query expansion under test (off|symbol_variant — A4).
    query_expansion: String,
    /// Reranker under test (off|local — A3).
    rerank: String,
    /// Local chat model serving the reranker when `--rerank local`.
    rerank_model: String,
    /// RRF smoothing constant k (A2 default 60; the eval sweeps it).
    rrf_k: f64,
    seed: bool,
    out: Option<String>,
}

/// Parses `retrieval --dataset <path> [flags]` (the `eval` token is already
/// consumed by the dispatch). Usage errors exit 2 via [`run_eval_cli`].
fn parse_retrieval_args(args: &[String]) -> Result<EvalParams, String> {
    if args.first().map(String::as_str) != Some("retrieval") {
        return Err(
            "unknown eval target (expected: ibis eval retrieval --dataset <path>)".to_string(),
        );
    }
    let mut params = EvalParams {
        dataset: String::new(),
        min_recall_5: 0.0,
        url: std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_SURREAL_URL.into()),
        ns: "ibis_eval".to_string(),
        db: "retrieval_golden".to_string(),
        user: "root".to_string(),
        pass: "root".to_string(),
        embedder: "auto".to_string(),
        ollama_url: DEFAULT_OLLAMA_URL.to_string(),
        fusion: "additive".to_string(),
        query_expansion: "symbol_variant".to_string(),
        rerank: "off".to_string(),
        rerank_model: DEFAULT_RERANK_MODEL.to_string(),
        rrf_k: 60.0,
        seed: true,
        out: None,
    };
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: &mut usize, name: &str| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "--dataset" => params.dataset = value(&mut i, "--dataset")?,
            "--min-recall-5" => {
                params.min_recall_5 = value(&mut i, "--min-recall-5")?
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| "--min-recall-5 must be a number".to_string())?
            }
            "--url" => params.url = value(&mut i, "--url")?,
            "--ns" => params.ns = value(&mut i, "--ns")?,
            "--db" => params.db = value(&mut i, "--db")?,
            "--user" => params.user = value(&mut i, "--user")?,
            "--pass" => params.pass = value(&mut i, "--pass")?,
            "--embedder" => {
                let v = value(&mut i, "--embedder")?;
                if !matches!(v.as_str(), "auto" | "offline" | "ollama") {
                    return Err("--embedder must be auto|offline|ollama".to_string());
                }
                params.embedder = v;
            }
            "--ollama-url" => params.ollama_url = value(&mut i, "--ollama-url")?,
            "--fusion" => {
                let v = value(&mut i, "--fusion")?;
                if !matches!(v.as_str(), "additive" | "rrf") {
                    return Err("--fusion must be additive|rrf".to_string());
                }
                params.fusion = v;
            }
            "--query-expansion" => {
                let v = value(&mut i, "--query-expansion")?;
                if !matches!(v.as_str(), "off" | "symbol_variant") {
                    return Err("--query-expansion must be off|symbol_variant".to_string());
                }
                params.query_expansion = v;
            }
            "--rerank" => {
                let v = value(&mut i, "--rerank")?;
                if !matches!(v.as_str(), "off" | "local") {
                    return Err("--rerank must be off|local".to_string());
                }
                params.rerank = v;
            }
            "--rerank-model" => params.rerank_model = value(&mut i, "--rerank-model")?,
            "--rrf-k" => {
                params.rrf_k = value(&mut i, "--rrf-k")?
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| "--rrf-k must be a positive number".to_string())?;
                if params.rrf_k <= 0.0 {
                    return Err("--rrf-k must be a positive number".to_string());
                }
            }
            "--no-seed" => params.seed = false,
            "--out" => params.out = Some(value(&mut i, "--out")?),
            other => return Err(format!("unknown eval flag: {other}")),
        }
        i += 1;
    }
    if params.dataset.is_empty() {
        return Err("--dataset <path> is required".to_string());
    }
    Ok(params)
}

/// Parses `decisions --dataset <path> [flags]` (D11 calibration runner).
fn parse_decisions_args(args: &[String]) -> Result<DecisionParams, String> {
    if args.first().map(String::as_str) != Some("decisions") {
        return Err(
            "unknown eval target (expected: ibis eval decisions --dataset <path>)".to_string(),
        );
    }
    let mut dataset = String::new();
    let mut min_agreement = 0.0;
    let mut provider = "ollama".to_string();
    let mut model: Option<String> = None;
    let mut url: Option<String> = None;
    let mut api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let mut auto_threshold = ibis_ai::decide::DEFAULT_AUTO_THRESHOLD;
    let mut escalate_threshold = ibis_ai::decide::DEFAULT_ESCALATE_THRESHOLD;
    let mut out = None;
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: &mut usize, name: &str| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag {
            "--dataset" => dataset = value(&mut i, "--dataset")?,
            "--min-agreement" => {
                min_agreement =
                    parse_unit_interval(&value(&mut i, "--min-agreement")?, "--min-agreement")?
            }
            "--provider" => {
                let v = value(&mut i, "--provider")?;
                if !matches!(v.as_str(), "ollama" | "openai_compat") {
                    return Err("--provider must be ollama|openai_compat".to_string());
                }
                provider = v;
            }
            "--model" => model = Some(value(&mut i, "--model")?),
            "--url" => url = Some(value(&mut i, "--url")?),
            "--api-key" => api_key = value(&mut i, "--api-key")?,
            "--auto-threshold" => {
                auto_threshold =
                    parse_unit_interval(&value(&mut i, "--auto-threshold")?, "--auto-threshold")?
            }
            "--escalate-threshold" => {
                escalate_threshold = parse_unit_interval(
                    &value(&mut i, "--escalate-threshold")?,
                    "--escalate-threshold",
                )?
            }
            "--out" => out = Some(value(&mut i, "--out")?),
            other => return Err(format!("unknown eval flag: {other}")),
        }
        i += 1;
    }
    if dataset.is_empty() {
        return Err("--dataset <path> is required".to_string());
    }
    if escalate_threshold > auto_threshold {
        return Err("--escalate-threshold must not exceed --auto-threshold".to_string());
    }
    let model = match model {
        Some(model) if !model.trim().is_empty() => model,
        _ if provider == "ollama" => DEFAULT_DECIDE_MODEL.to_string(),
        _ => {
            return Err(
                "--model is required for --provider openai_compat (e.g. --model qwen3.8-27b)"
                    .to_string(),
            )
        }
    };
    let url = url.unwrap_or_else(|| {
        if provider == "openai_compat" {
            DEFAULT_OPENAI_COMPAT_URL.to_string()
        } else {
            DEFAULT_DECIDE_OLLAMA_URL.to_string()
        }
    });
    Ok(DecisionParams {
        dataset,
        min_agreement,
        provider,
        model,
        url,
        api_key,
        auto_threshold,
        escalate_threshold,
        out,
    })
}

fn parse_unit_interval(value: &str, name: &str) -> Result<f64, String> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{name} must be a number"))?;
    if !(0.0..=1.0).contains(&parsed) {
        return Err(format!("{name} must be in [0, 1]"));
    }
    Ok(parsed)
}

fn parse_eval_args(args: &[String]) -> Result<EvalTarget, String> {
    match args.first().map(String::as_str) {
        Some("retrieval") => Ok(EvalTarget::Retrieval(parse_retrieval_args(args)?)),
        Some("decisions") => Ok(EvalTarget::Decisions(parse_decisions_args(args)?)),
        _ => Err(
            "unknown eval target (expected: ibis eval retrieval|decisions --dataset <path>)"
                .to_string(),
        ),
    }
}

/// CLI entry: prints errors on stderr and returns the process exit code
/// (0 = pass, 1 = quality gate or all-error run, 2 = usage/setup failure).
pub fn run_eval_cli(args: &[String]) -> i32 {
    match run_eval(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("eval: {e}");
            2
        }
    }
}

fn run_eval(args: &[String]) -> Result<i32, String> {
    let target = parse_eval_args(args)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    match target {
        EvalTarget::Retrieval(params) => runtime.block_on(run_retrieval_async(params)),
        EvalTarget::Decisions(params) => runtime.block_on(run_decisions_async(params)),
    }
}

/// A3 eval seam: the plain `OllamaProvider` only embeds, the reranker needs
/// a generation half too — the eval composes the embedding provider with the
/// local chat model serving the scoring pass. Embeddings keep the queue/cache
/// semantics of the embedder; generation is a straight delegation.
struct EmbedOllamaGenerateChat {
    embedder: ibis_ai::OllamaProvider,
    generator: ibis_ai::OllamaChatProvider,
}

#[async_trait]
impl AIProvider for EmbedOllamaGenerateChat {
    fn name(&self) -> &str {
        "ollama_embed+ollama_chat"
    }

    async fn is_functional(&self) -> bool {
        self.embedder.is_functional().await && self.generator.is_functional().await
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        self.embedder.embed_text(text).await
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        self.generator.generate_content(contents, config).await
    }
}

async fn run_retrieval_async(p: EvalParams) -> Result<i32, String> {
    let mut ollama_live = false;
    let embedder: Arc<dyn AIProvider> = match p.embedder.as_str() {
        "offline" => Arc::new(OfflineEmbedder),
        "ollama" => {
            ollama_live = true;
            Arc::new(ibis_ai::OllamaProvider::new(
                DEFAULT_EMBEDDING_MODEL,
                &p.ollama_url,
            ))
        }
        _ => {
            let probe = ibis_ai::OllamaProvider::new(DEFAULT_EMBEDDING_MODEL, &p.ollama_url);
            match probe.embed_text("ibis eval probe").await {
                Ok(_) => {
                    ollama_live = true;
                    Arc::new(probe)
                }
                Err(e) => {
                    eprintln!(
                        "eval: ollama probe failed ({e}); falling back to the offline embedder"
                    );
                    Arc::new(OfflineEmbedder)
                }
            }
        }
    };

    // A3: with `--rerank local` the reranker scores via a local chat model —
    // compose it under the SAME provider the service sees. A non-generating
    // embedder (offline fallback) fail-opens per ask with a warn; flag it so
    // the run is not mistaken for a measured rerank.
    let ai: Arc<dyn AIProvider> = if p.rerank == "local" && ollama_live {
        Arc::new(EmbedOllamaGenerateChat {
            embedder: ibis_ai::OllamaProvider::new(DEFAULT_EMBEDDING_MODEL, &p.ollama_url),
            generator: ibis_ai::OllamaChatProvider::new(&p.rerank_model, &p.ollama_url),
        })
    } else if p.rerank == "local" {
        eprintln!(
            "eval: --rerank local needs the ollama embedder for the scoring pass; \
             the offline embedder fail-opens (order stays fused)"
        );
        embedder
    } else {
        embedder
    };

    let db: Arc<dyn Executor> = Arc::new(
        SurrealDbExecutor::connect(&p.url, &p.ns, &p.db, &p.user, &p.pass)
            .await
            .map_err(|e| format!("connect {} (ns {}, db {}): {e}", p.url, p.ns, p.db))?,
    );

    let seeded = if p.seed {
        let rows = seed_golden_estate(&db, &ai).await?;
        tracing::info!("eval: golden estate seeded ({rows} rows)");
        true
    } else {
        false
    };

    let dataset = load_dataset(&p.dataset)?;
    let mut service = SearchService::new(Some(db), Some(ai.clone()));
    service.fusion_mode = ibis_search::retrieval_boost::FusionMode::resolve(&p.fusion);
    service.rrf_k = p.rrf_k;
    service.query_expansion =
        ibis_search::retrieval_boost::QueryExpansionMode::resolve(&p.query_expansion);
    service.rerank_mode = ibis_search::rerank::RerankMode::resolve(&p.rerank);
    let service = Arc::new(service);
    let runner = RetrievalEvalRunner::new(service);
    let cases = runner.run(&dataset).await;
    let mut report = build_report(
        &p.dataset,
        &dataset.version,
        ai.name(),
        &p.fusion,
        &p.query_expansion,
        &p.rerank,
        seeded,
        cases,
    );
    // Sweep provenance: the k under test belongs in the report.
    if report.fusion == "rrf" {
        report.rrf_k = Some(p.rrf_k);
    }

    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("report: {e}"))?;
    println!("{json}");
    if let Some(out) = &p.out {
        if let Some(parent) = std::path::Path::new(out).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
        }
        std::fs::write(out, format!("{json}\n"))
            .map_err(|e| format!("write report {}: {e}", out))?;
    }

    if report.aggregate.cases_ok == 0 {
        eprintln!(
            "eval: all {} cases errored (degraded DB/AI?)",
            report.aggregate.cases
        );
        return Ok(1);
    }
    if report.below_threshold(p.min_recall_5) {
        eprintln!(
            "eval: mean recall@5 {:.3} below threshold {:.3}",
            report.aggregate.recall_at_5, p.min_recall_5
        );
        return Ok(1);
    }
    Ok(0)
}

/// D11 calibration run: one `decide()` call per labeled case against the
/// selected chat provider (Ollama or any OpenAI-compatible endpoint).
async fn run_decisions_async(p: DecisionParams) -> Result<i32, String> {
    let ai: Arc<dyn AIProvider> = match p.provider.as_str() {
        "ollama" => Arc::new(ibis_ai::OllamaChatProvider::new(&p.model, &p.url)),
        "openai_compat" => Arc::new(ibis_ai::OpenAICompatProvider::new(
            &p.url, &p.api_key, &p.model,
        )),
        other => return Err(format!("unsupported provider {other:?}")),
    };
    if !ai.is_functional().await {
        return Err(format!(
            "provider {} is not functional (check --url/--model/--api-key; local servers accept any non-empty placeholder key)",
            p.provider
        ));
    }
    let dataset = load_decision_dataset(&p.dataset)?;
    let runner = DecisionEvalRunner::new(ai.clone());
    let cases = runner.run(&dataset).await;
    let report = build_decision_report(
        &p.dataset,
        &dataset.version,
        &dataset.rubric,
        ai.name(),
        ai.model(),
        p.auto_threshold,
        p.escalate_threshold,
        cases,
    );
    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("report: {e}"))?;
    println!("{json}");
    if let Some(out) = &p.out {
        if let Some(parent) = std::path::Path::new(out).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
        }
        std::fs::write(out, format!("{json}\n"))
            .map_err(|e| format!("write report {}: {e}", out))?;
    }

    if report.aggregate.cases_ok == 0 {
        eprintln!(
            "eval: all {} decision cases errored (degraded provider?)",
            report.aggregate.cases
        );
        return Ok(1);
    }
    if report.below_threshold(p.min_agreement) {
        eprintln!(
            "eval: agreement {:.3} below threshold {:.3}",
            report.aggregate.agreement, p.min_agreement
        );
        return Ok(1);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn retrieval(list: &[&str]) -> Result<EvalParams, String> {
        match parse_eval_args(&args(list))? {
            EvalTarget::Retrieval(params) => Ok(params),
            EvalTarget::Decisions(_) => panic!("expected the retrieval target"),
        }
    }

    fn decisions(list: &[&str]) -> Result<DecisionParams, String> {
        match parse_eval_args(&args(list))? {
            EvalTarget::Decisions(params) => Ok(params),
            EvalTarget::Retrieval(_) => panic!("expected the decisions target"),
        }
    }

    #[test]
    fn parse_requires_target_and_dataset() {
        assert_eq!(
            parse_eval_args(&args(&[])).err(),
            Some(
                "unknown eval target (expected: ibis eval retrieval|decisions --dataset <path>)"
                    .to_string()
            )
        );
        assert!(retrieval(&["retrieval"]).is_err());
        assert!(retrieval(&["retrieval", "--dataset", "d.yaml"]).is_ok());
        assert!(decisions(&["decisions"]).is_err());
        assert!(decisions(&["decisions", "--dataset", "d.yaml"]).is_ok());
    }

    #[test]
    fn parse_flags_and_errors() {
        let p = retrieval(&[
            "retrieval",
            "--dataset",
            "d.yaml",
            "--min-recall-5",
            "0.4",
            "--embedder",
            "offline",
            "--no-seed",
            "--out",
            "r.json",
            "--db",
            "custom",
            "--fusion",
            "rrf",
        ])
        .expect("parse");
        assert_eq!(p.min_recall_5, 0.4);
        assert_eq!(p.embedder, "offline");
        assert!(!p.seed);
        assert_eq!(p.out.as_deref(), Some("r.json"));
        assert_eq!(p.db, "custom");
        assert_eq!(p.ns, "ibis_eval");
        assert_eq!(p.fusion, "rrf");

        let rerank_p = retrieval(&[
            "retrieval",
            "--dataset",
            "d.yaml",
            "--rerank",
            "local",
            "--rerank-model",
            "gemma4:12b",
        ])
        .expect("parse");
        assert_eq!(rerank_p.rerank, "local");
        assert_eq!(rerank_p.rerank_model, "gemma4:12b");
        assert_eq!(
            retrieval(&["retrieval", "--dataset", "d.yaml"])
                .unwrap()
                .rerank,
            "off"
        );

        assert!(retrieval(&["retrieval", "--embedder", "bogus"]).is_err());
        assert!(retrieval(&["retrieval", "--min-recall-5", "abc"]).is_err());
        assert!(retrieval(&["retrieval", "--url"]).is_err());
        assert!(retrieval(&["retrieval", "--fusion", "bogus"]).is_err());
        assert!(retrieval(&["retrieval", "--rerank", "bogus"]).is_err());
        assert!(retrieval(&["retrieval", "--wat"]).is_err());
    }

    #[test]
    fn parse_decision_flags_and_defaults() {
        let p = decisions(&["decisions", "--dataset", "cases.yaml"]).expect("parse");
        assert_eq!(p.provider, "ollama");
        assert_eq!(p.model, DEFAULT_DECIDE_MODEL);
        assert_eq!(p.url, DEFAULT_DECIDE_OLLAMA_URL);
        assert_eq!(p.min_agreement, 0.0);
        assert_eq!(p.auto_threshold, ibis_ai::decide::DEFAULT_AUTO_THRESHOLD);
        assert_eq!(
            p.escalate_threshold,
            ibis_ai::decide::DEFAULT_ESCALATE_THRESHOLD
        );

        let p = decisions(&[
            "decisions",
            "--dataset",
            "cases.yaml",
            "--provider",
            "openai_compat",
            "--url",
            "http://127.0.0.1:8080/v1",
            "--model",
            "qwen3.8-27b",
            "--api-key",
            "local",
            "--min-agreement",
            "0.8",
            "--auto-threshold",
            "0.8",
            "--escalate-threshold",
            "0.3",
            "--out",
            "r.json",
        ])
        .expect("parse");
        assert_eq!(p.provider, "openai_compat");
        assert_eq!(p.model, "qwen3.8-27b");
        assert_eq!(p.url, "http://127.0.0.1:8080/v1");
        assert_eq!(p.api_key, "local");
        assert_eq!(p.min_agreement, 0.8);
        assert_eq!(p.auto_threshold, 0.8);
        assert_eq!(p.escalate_threshold, 0.3);
        assert_eq!(p.out.as_deref(), Some("r.json"));
    }

    #[test]
    fn parse_decision_errors() {
        assert!(decisions(&["decisions", "--provider", "bogus", "--dataset", "d.yaml"]).is_err());
        assert!(decisions(&[
            "decisions",
            "--dataset",
            "d.yaml",
            "--provider",
            "openai_compat"
        ])
        .is_err());
        assert!(
            decisions(&["decisions", "--dataset", "d.yaml", "--min-agreement", "1.5"]).is_err()
        );
        assert!(decisions(&[
            "decisions",
            "--dataset",
            "d.yaml",
            "--auto-threshold",
            "abc"
        ])
        .is_err());
        assert!(decisions(&[
            "decisions",
            "--dataset",
            "d.yaml",
            "--auto-threshold",
            "0.4",
            "--escalate-threshold",
            "0.7"
        ])
        .is_err());
        assert!(decisions(&["decisions", "--dataset", "d.yaml", "--wat"]).is_err());
    }
}
