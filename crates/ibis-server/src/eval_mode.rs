//! `ibis eval retrieval` — in-process retrieval evaluation (A1, ddofds
//! fase 0; plan in `docs/planning/2026-11-02-a1-retrieval-eval-plan.md`).
//!
//! Intercepted in `main.rs` BEFORE the MCP CLI passthrough: the eval mounts
//! SurrealDB + the embedding provider + `SearchService` directly (no running
//! server, no new MCP tool — the registry stays at 57 tools). The golden
//! estate is (re)seeded into a dedicated scratch database, the dataset cases
//! run through `ask_project`, and a JSON report is printed with a non-zero
//! exit under `--min-recall-5`.

use std::sync::Arc;

use ibis_ai::AIProvider;
use ibis_db::executor::{Executor, SurrealDbExecutor};
use ibis_optimization::eval::{
    build_report, load_dataset, seed_golden_estate, OfflineEmbedder, RetrievalEvalRunner,
};
use ibis_search::SearchService;

const DEFAULT_SURREAL_URL: &str = "ws://127.0.0.1:38000";
const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_EMBEDDING_MODEL: &str = "nomic-embed-text";

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
    /// RRF smoothing constant k (A2 default 60; the eval sweeps it).
    rrf_k: f64,
    seed: bool,
    out: Option<String>,
}

/// Parses `retrieval --dataset <path> [flags]` (the `eval` token is already
/// consumed by the dispatch). Usage errors exit 2 via [`run_eval_cli`].
fn parse_args(args: &[String]) -> Result<EvalParams, String> {
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
    let params = parse_args(args)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    runtime.block_on(run_async(params))
}

async fn run_async(p: EvalParams) -> Result<i32, String> {
    let ai: Arc<dyn AIProvider> = match p.embedder.as_str() {
        "offline" => Arc::new(OfflineEmbedder),
        "ollama" => Arc::new(ibis_ai::OllamaProvider::new(
            DEFAULT_EMBEDDING_MODEL,
            &p.ollama_url,
        )),
        _ => {
            let probe = ibis_ai::OllamaProvider::new(DEFAULT_EMBEDDING_MODEL, &p.ollama_url);
            match probe.embed_text("ibis eval probe").await {
                Ok(_) => Arc::new(probe),
                Err(e) => {
                    eprintln!(
                        "eval: ollama probe failed ({e}); falling back to the offline embedder"
                    );
                    Arc::new(OfflineEmbedder)
                }
            }
        }
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
    let service = Arc::new(service);
    let runner = RetrievalEvalRunner::new(service);
    let cases = runner.run(&dataset).await;
    let mut report = build_report(
        &p.dataset,
        &dataset.version,
        ai.name(),
        &p.fusion,
        &p.query_expansion,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_requires_target_and_dataset() {
        assert_eq!(
            parse_args(&args(&[])).err(),
            Some(
                "unknown eval target (expected: ibis eval retrieval --dataset <path>)".to_string()
            )
        );
        assert!(parse_args(&args(&["retrieval"])).is_err());
        assert!(parse_args(&args(&["retrieval", "--dataset", "d.yaml"])).is_ok());
    }

    #[test]
    fn parse_flags_and_errors() {
        let p = parse_args(&args(&[
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
        ]))
        .expect("parse");
        assert_eq!(p.min_recall_5, 0.4);
        assert_eq!(p.embedder, "offline");
        assert!(!p.seed);
        assert_eq!(p.out.as_deref(), Some("r.json"));
        assert_eq!(p.db, "custom");
        assert_eq!(p.ns, "ibis_eval");
        assert_eq!(p.fusion, "rrf");

        assert!(parse_args(&args(&["retrieval", "--embedder", "bogus"])).is_err());
        assert!(parse_args(&args(&["retrieval", "--min-recall-5", "abc"])).is_err());
        assert!(parse_args(&args(&["retrieval", "--url"])).is_err());
        assert!(parse_args(&args(&["retrieval", "--fusion", "bogus"])).is_err());
        assert!(parse_args(&args(&["retrieval", "--wat"])).is_err());
    }
}
