//! Enrichment: resolves file / symbol / baseline context for classified
//! anomalies using stack parsing + SurrealDB + code search.
//!
//! Stages per anomaly: baseline resolution (project record → workspace HEAD →
//! DB branch tip), stack-frame extraction, basename → repo-relative path
//! resolution, keyword/template/vector code-context search, and cause
//! synthesis (heuristic or AI-refined).
//!
//! Seams: the vector-search stage goes through [`CodeSearch`] (lane H provides
//! the `threski-search` implementation); HEAD probes go through
//! [`threski_git::GitRunner`] (D-A07). All SQL binds variables (F-002).

use std::sync::OnceLock;

use async_trait::async_trait;

use threski_db::executor::{vars, Vars};
use threski_db::ids::{format_record_id, sanitize_id};
use threski_db::schema::names as schema;
use threski_db::value::{SurrealValue, I64};

use crate::analyzer::{LogAnalyzer, SemanticError};

/// Temporal baseline used when correlating log frames to source
///
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TemporalBaseline {
    /// session | workspace_head | db_branch | project_record | unknown
    pub source: String,
    pub commit: String,
    pub branch: String,
    pub repo: String,
}

impl std::fmt::Display for TemporalBaseline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if !self.repo.is_empty() {
            parts.push(format!("repo={}", self.repo));
        }
        if !self.branch.is_empty() {
            parts.push(format!("branch={}", self.branch));
        }
        if !self.commit.is_empty() {
            let mut c = self.commit.clone();
            if c.chars().count() > 12 {
                c = c.chars().take(12).collect();
            }
            parts.push(format!("commit={c}"));
        }
        if !self.source.is_empty() {
            parts.push(format!("via={}", self.source));
        }
        if parts.is_empty() {
            return write!(f, "unknown");
        }
        write!(f, "{}", parts.join(" "))
    }
}

/// Scenario: panic / runtime frames: `at pkg.(*T).M ` or
///
fn re_go_paren_frame() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\(([^()\s]+\.[a-z0-9]+):(\d+)\)").expect("static regex")
    })
}

/// `at path/file.ext:12` frames.
fn re_go_at_frame() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:^|\s)(?:at\s+)?([\w./\\-]+\.[a-z0-9]+):(\d+)\b")
            .expect("static regex")
    })
}

/// Java frames: `at com.example.Service.run(Service.java:55)`
///
fn re_java_frame() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)at\s+([\w.$]+)\(([^()]+\.[a-z0-9]+):(\d+)\)").expect("static regex")
    })
}

/// Bare path tokens.
fn re_bare_path() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b((?:[\w.-]+[\\/])+[\w.-]+\.[a-z0-9]{1,8})\b")
            .expect("static regex")
    })
}

/// Symbol hint for the paren-frame branch — hoisted to a static ( compiled
/// this regex inside the function on every call).
fn re_paren_symbol() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:\*\))?\.?([A-Za-z_][\w]*)\s+\(").expect("static regex")
    })
}

/// Pulls the most specific file/line/symbol from stack-like text
///
pub fn extract_stack_location(text: &str) -> (String, i64, String) {
    let text = text.trim();
    if text.is_empty() {
        return (String::new(), 0, String::new());
    }

    if let Some(m) = re_java_frame().captures(text) {
        let mut symbol = m.get(1).map(|c| c.as_str()).unwrap_or("").to_string();
        if let Some(idx) = symbol.rfind('.') {
            symbol = symbol[idx + 1..].to_string();
        }
        return (
            normalize_rel_path(m.get(2).map(|c| c.as_str()).unwrap_or("")),
            atoi_safe(m.get(3).map(|c| c.as_str()).unwrap_or("")),
            symbol,
        );
    }
    if let Some(m) = re_go_paren_frame().captures(text) {
        let sym = re_paren_symbol()
            .captures(text)
            .and_then(|sm| sm.get(1))
            .map(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        return (
            normalize_rel_path(m.get(1).map(|c| c.as_str()).unwrap_or("")),
            atoi_safe(m.get(2).map(|c| c.as_str()).unwrap_or("")),
            sym,
        );
    }
    if let Some(m) = re_go_at_frame().captures(text) {
        return (
            normalize_rel_path(m.get(1).map(|c| c.as_str()).unwrap_or("")),
            atoi_safe(m.get(2).map(|c| c.as_str()).unwrap_or("")),
            String::new(),
        );
    }
    if let Some(m) = re_bare_path().captures(text) {
        return (
            normalize_rel_path(m.get(1).map(|c| c.as_str()).unwrap_or("")),
            0,
            String::new(),
        );
    }
    (String::new(), 0, String::new())
}

/// Normalizes a source path to a forward-slash repo-relative form
///
pub fn normalize_rel_path(p: &str) -> String {
    let p = p.trim().replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

fn atoi_safe(s: &str) -> i64 {
    let mut n: i64 = 0;
    for r in s.chars() {
        if !r.is_ascii_digit() {
            break;
        }
        n = n * 10 + (r as i64 - '0' as i64);
    }
    n
}

/// Code-search seam for the vector stage (`search.Service.AskProject
/// projection — lane H implements with threski-search; `None` in the analyzer
/// skips the stage).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeSearchHit {
    pub path: String,
    pub content: String,
    pub is_current: bool,
}

#[async_trait]
pub trait CodeSearch: Send + Sync {
    async fn ask_project(&self, query: &str) -> crate::error::Result<Vec<CodeSearchHit>>;
}

/// DB row projection for the project record (baseline step 1).
#[derive(Debug, SurrealValue)]
pub(crate) struct BaselineRow {
    baseline_commit: Option<String>,
    baseline_branch: Option<String>,
    baseline_source: Option<String>,
}

/// DB row projection for the repo branch-tip query (baseline step 3).
#[derive(Debug, SurrealValue)]
struct BranchTipRow {
    #[allow(dead_code)]
    name: Option<String>,
    branches: Option<Vec<String>>,
    tip_hashes: Option<Vec<String>>,
}

/// DB row projection for `source_file` basename resolution.
#[derive(Debug, SurrealValue)]
struct SourcePathRow {
    rel_path: Option<String>,
    path: Option<String>,
}

/// DB row projection for `SELECT count`.
#[derive(Debug, SurrealValue)]
struct CountRow {
    c: Option<I64>,
}

/// DB row projection for lexical file_chunk search.
#[derive(Debug, SurrealValue)]
struct ChunkSearchRow {
    rel_path: Option<String>,
    path: Option<String>,
    content: Option<String>,
}

/// DB row projection for log_template format search.
#[derive(Debug, SurrealValue)]
struct TemplateSearchRow {
    source_file: Option<String>,
    #[allow(dead_code)]
    source_line: Option<I64>,
    format_string: Option<String>,
}

impl LogAnalyzer {
    /// Fills File / Line / Symbol / Cause using stack parsing + SurrealDB /
    /// code search.
    pub(crate) async fn enrich_anomalies(
        &self,
        mut errors: Vec<SemanticError>,
    ) -> Vec<SemanticError> {
        if errors.is_empty() {
            return errors;
        }
        let baseline = resolve_baseline(self).await;
        let baseline_str = baseline.to_string();

        for e in errors.iter_mut() {
            e.baseline = baseline_str.clone();
            let mut source = "none";

            // 1) Stack / template text
            let loc_text = if e.stack_trace.is_empty() {
                &e.template
            } else {
                &e.stack_trace
            };
            let (f, line, sym) = extract_stack_location(loc_text);
            if !f.is_empty() {
                if e.file.is_empty() {
                    e.file = f;
                    source = "stack";
                }
                if e.line == 0 && line > 0 {
                    e.line = line;
                }
                if e.symbol.is_empty() && !sym.is_empty() {
                    e.symbol = sym;
                }
            }

            // 2) Resolve basename → repo-relative path (prefer current ingest)
            if !e.file.is_empty() && !e.file.contains('/') {
                let resolved = resolve_source_path(self, &e.file).await;
                if !resolved.is_empty() {
                    e.file = resolved;
                    source = if source == "stack" {
                        "stack+db"
                    } else {
                        "db_path"
                    };
                }
            }

            // 3) Keyword / hybrid search when file still missing or not in the graph
            let mut snippet = String::new();
            if e.file.is_empty() || !source_path_known(self, &e.file).await {
                let query = enrichment_query(e);
                let (path, snip, hit) = search_code_context(self, query.as_str()).await;
                if hit {
                    snippet = snip;
                    if !path.is_empty()
                        && (e.file.is_empty() || !source_path_known(self, &e.file).await)
                    {
                        e.file = path;
                        source = "search";
                    }
                }
            } else if e.cause.is_empty() {
                // Still try to fetch a snippet for cause synthesis
                let query = enrichment_query(e);
                let (_, snip, hit) = search_code_context(self, query.as_str()).await;
                if hit {
                    snippet = snip;
                }
            }

            // 4) Static log template already set SourceFile — ensure cause
            if !e.file.is_empty() && e.category.starts_with("Static Log:") && e.cause.is_empty() {
                e.cause = format!(
                    "Matched AST log template in {} (line {}) against emitted format string.",
                    e.file, e.line
                );
                if source == "none" {
                    source = "static_template";
                }
            }

            // 5) Cause from snippet / AI / heuristic
            if e.cause.is_empty() {
                e.cause = synthesize_cause(self, e, &snippet, &baseline).await;
            }
            let _ = source;
        }
        errors
    }
}

/// Builds the compact enrichment query text.
pub fn enrichment_query(e: &SemanticError) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !e.category.is_empty() {
        parts.push(e.category.clone());
    }
    let msg = if e.stack_trace.is_empty() {
        &e.template
    } else {
        &e.stack_trace
    };
    let msg = strip_log_noise(msg);
    if !msg.is_empty() {
        parts.push(msg);
    }
    let q = parts.join(" ");
    char_safe_truncate(&q, 400)
}

/// Drops leading slog-ish `key=value` prefixes commonly seen in this project's
/// logs.
pub fn strip_log_noise(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re_kv = RE.get_or_init(|| {
        regex::Regex::new(r#"(?i)^(?:time|level|msg)=("[^"]*"|[^\s]+)\s*"#).expect("static regex")
    });
    let mut s = s.trim().to_string();
    for _ in 0..6 {
        let ns = re_kv.replace_all(&s, "").into_owned();
        if ns == s {
            break;
        }
        s = ns.trim().to_string();
    }
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// Resolves a bare basename to a repo-relative path from `source_file` rows
/// Bound variables (F-002).
async fn resolve_source_path(analyzer: &LogAnalyzer, basename: &str) -> String {
    let Some(db) = &analyzer.db else {
        return String::new();
    };
    if basename.is_empty() {
        return String::new();
    }
    let base = std::path::Path::new(&basename.replace('\\', "/"))
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slash_base = format!("/{base}");
    let win_base = format!("\\{base}");
    let sql = format!(
        "SELECT rel_path, path FROM {}\n\t\tWHERE rel_path = $base\n\t\t   OR string::ends_with(rel_path, $slash_base)\n\t\t   OR string::ends_with(path, $slash_base)\n\t\t   OR string::ends_with(path, $win_base)\n\t\tLIMIT 8;",
        schema::SOURCE_FILE
    );
    let v: Vars = vars([
        ("base", base.as_str()),
        ("slash_base", slash_base.as_str()),
        ("win_base", win_base.as_str()),
    ]);
    let Ok(outcome) = db.smart_query(&sql, v).await else {
        return String::new();
    };
    let Ok(rows) = outcome.rows::<SourcePathRow>(&sql) else {
        return String::new();
    };
    let mut fallback = String::new();
    for row in rows {
        let rel = row
            .rel_path
            .filter(|r| !r.is_empty())
            .map(|r| normalize_rel_path(&r))
            .or_else(|| row.path.map(|p| normalize_rel_path(&p)))
            .unwrap_or_default();
        if rel.is_empty() {
            continue;
        }
        if fallback.is_empty() {
            fallback = rel.clone();
        }
        // Prefer paths that look like this repo (internal/..., cmd/...)
        if rel.starts_with("internal/") || rel.starts_with("cmd/") {
            return rel;
        }
    }
    fallback
}

/// Reports whether a normalized path exists in the `source_file` graph
/// Bound variables (F-002).
async fn source_path_known(analyzer: &LogAnalyzer, path: &str) -> bool {
    let Some(db) = &analyzer.db else {
        return false;
    };
    let path = path.trim();
    if path.is_empty() {
        return false;
    }
    let path = normalize_rel_path(path);
    let base = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slash_path = format!("/{path}");
    let sql = format!(
        "SELECT count() AS c FROM {}\n\t\tWHERE rel_path = $path\n\t\t   OR path = $path\n\t\t   OR string::ends_with(rel_path, $slash_path)\n\t\t   OR string::ends_with(rel_path, $base)\n\t\tGROUP ALL;",
        schema::SOURCE_FILE
    );
    let v: Vars = vars([
        ("path", path.as_str()),
        ("slash_path", slash_path.as_str()),
        ("base", base.as_str()),
    ]);
    let Ok(outcome) = db.smart_query(&sql, v).await else {
        return false;
    };
    let Ok(rows) = outcome.rows::<CountRow>(&sql) else {
        return false;
    };
    rows.first()
        .and_then(|r| r.c)
        .map(|c| c.get() > 0)
        .unwrap_or(false)
}

/// Lexical + template + vector code-context search.
/// Returns `(path, snippet, hit)`.
pub(crate) async fn search_code_context(
    analyzer: &LogAnalyzer,
    query: &str,
) -> (String, String, bool) {
    let query = query.trim();
    let Some(db) = &analyzer.db else {
        return (String::new(), String::new(), false);
    };
    if query.is_empty() {
        return (String::new(), String::new(), false);
    }

    // A) Lexical search on file_chunk (works without embeddings)
    for term in distinctive_terms(query, 4) {
        if term.len() < 4 {
            continue;
        }
        let lowered = term.to_lowercase();
        let sql = format!(
            "SELECT file.rel_path as rel_path, file.path as path, content\n\t\t\tFROM {}\n\t\t\tWHERE string::lowercase(content) CONTAINS $term\n\t\t\tLIMIT 5;",
            schema::FILE_CHUNK
        );
        let Ok(outcome) = db
            .smart_query(&sql, vars([("term", lowered.as_str())]))
            .await
        else {
            continue;
        };
        let Ok(rows) = outcome.rows::<ChunkSearchRow>(&sql) else {
            continue;
        };
        for row in rows {
            let p = row
                .rel_path
                .filter(|r| !r.is_empty())
                .or(row.path.filter(|p| !p.is_empty()))
                .unwrap_or_default();
            let content = row.content.unwrap_or_default();
            if !p.is_empty() || !content.is_empty() {
                return (normalize_rel_path(&p), trim_snippet(&content, 280), true);
            }
        }
    }

    // B) Log templates by format_string / regex fragment
    for term in distinctive_terms(query, 4) {
        if term.len() < 4 {
            continue;
        }
        let sql = format!(
            "SELECT source_file, source_line, format_string\n\t\t\tFROM {}\n\t\t\tWHERE string::lowercase(format_string) CONTAINS string::lowercase($term)\n\t\t\tLIMIT 3;",
            schema::LOG_TEMPLATE
        );
        let Ok(outcome) = db.smart_query(&sql, vars([("term", term.as_str())])).await else {
            continue;
        };
        let Ok(rows) = outcome.rows::<TemplateSearchRow>(&sql) else {
            continue;
        };
        if let Some(row) = rows.first() {
            let p = row.source_file.clone().unwrap_or_default();
            if !p.is_empty() {
                let fs = row.format_string.clone().unwrap_or_default();
                return (normalize_rel_path(&p), trim_snippet(&fs, 200), true);
            }
        }
    }

    // C) Hybrid vector search when embeddings work (lane-H seam).
    if let (Some(svc), Some(ai)) = (&analyzer.code_search, &analyzer.ai) {
        if ai.is_functional() {
            if let Ok(results) = svc.ask_project(query).await {
                for r in &results {
                    if r.path.is_empty() && r.content.is_empty() {
                        continue;
                    }
                    // Prefer current HEAD chunks when correlating to baseline
                    if !r.is_current {
                        continue;
                    }
                    return (
                        normalize_rel_path(&r.path),
                        trim_snippet(&r.content, 280),
                        true,
                    );
                }
                for r in &results {
                    if !r.path.is_empty() || !r.content.is_empty() {
                        return (
                            normalize_rel_path(&r.path),
                            trim_snippet(&r.content, 280),
                            true,
                        );
                    }
                }
            }
        }
    }

    (String::new(), String::new(), false)
}

/// Builds a compact context block for the AI classification prompt
///
pub(crate) async fn gather_repo_hints(analyzer: &LogAnalyzer, lines: &[String]) -> String {
    if analyzer.db.is_none() || lines.is_empty() {
        return String::new();
    }
    let joined = lines.join("\n");
    let joined = char_safe_truncate(&joined, 600);
    let (path, snip, hit) = search_code_context(analyzer, &joined).await;
    if !hit {
        return String::new();
    }
    format!("Likely related source: {path}\nSnippet:\n{snip}")
}

/// Stop-word filtered keyword extraction.
pub fn distinctive_terms(query: &str, limit: usize) -> Vec<String> {
    let stop: [&str; 23] = [
        "error", "failed", "failure", "panic", "level", "time", "msg", "warn", "info", "debug",
        "the", "and", "for", "from", "with", "this", "that", "into", "unable", "cannot", "static",
        "log", "project",
    ];
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for f in query
        .to_lowercase()
        .split(|r: char| !(r.is_alphabetic() || r.is_numeric() || r == '_' || r == '-'))
    {
        if f.len() < 4 || stop.contains(&f) || seen.contains(f) {
            continue;
        }
        seen.insert(f.to_string());
        out.push(f.to_string());
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Truncates to at most `n` chars ( cut bytes; char-boundary safe in Rust —
/// the byte-slice version could split a UTF-8 rune).
pub(crate) fn trim_snippet(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{cut}…")
}

fn char_safe_truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect()
}

/// Resolves the code baseline for correlation:
/// 1) project record → 2) workspace HEAD → 3) DB branch tip.
pub(crate) async fn resolve_baseline(analyzer: &LogAnalyzer) -> TemporalBaseline {
    let mut b = TemporalBaseline {
        repo: analyzer.project.clone(),
        source: "unknown".to_string(),
        ..Default::default()
    };

    // 1) Project record written by init_project / prior analysis
    if let (Some(db), true) = (&analyzer.db, !analyzer.project.is_empty()) {
        let project_id = format_record_id(schema::PROJECT, &sanitize_id(&analyzer.project));
        let sql = format!(
            "SELECT baseline_commit, baseline_branch, baseline_source, name FROM {project_id};"
        );
        if let Ok(outcome) = db.execute(&sql).await {
            if let Ok(rows) = outcome.rows::<BaselineRow>(&sql) {
                if let Some(row) = rows.first() {
                    if let Some(c) = row.baseline_commit.as_ref().filter(|c| !c.is_empty()) {
                        b.commit = c.clone();
                        b.branch = row.baseline_branch.clone().unwrap_or_default();
                        b.source = row
                            .baseline_source
                            .clone()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "project_record".to_string());
                        return b;
                    }
                    if let Some(br) = row.baseline_branch.as_ref().filter(|br| !br.is_empty()) {
                        b.branch = br.clone();
                        b.source = "project_record".to_string();
                    }
                }
            }
        }
    }

    // 2) Workspace HEAD (live tree first, then discovery_root clones)
    if let (Some(cfg), true) = (&analyzer.cfg, !analyzer.project.is_empty()) {
        let candidates = threski_core::workspace::candidate_paths(Some(cfg), &analyzer.project);
        for dir in candidates {
            if !std::path::Path::new(&dir).join(".git").exists() {
                continue;
            }
            let (commit, branch) = git_head_and_branch(&analyzer.git, &dir).await;
            if !commit.is_empty() {
                b.commit = commit;
                b.branch = branch;
                b.source = "workspace_head".to_string();
                persist_baseline(analyzer, &b).await;
                return b;
            }
        }
    }

    // 3) DB branch tip for a repo named like the project
    if let (Some(db), true) = (&analyzer.db, !analyzer.project.is_empty()) {
        let repo_id = format_record_id(schema::REPO, &sanitize_id(&analyzer.project));
        let sql = format!(
            "SELECT name,\n\t\t\t\t->{contains}->{branch}.name as branches,\n\t\t\t\t->{contains}->{branch}->{pointed_to}->{commit}.hash as tip_hashes\n\t\t\tFROM {repo_id} LIMIT 1;",
            contains = schema::CONTAINS,
            branch = schema::BRANCH,
            pointed_to = schema::POINTED_TO,
            commit = schema::COMMIT,
        );
        if let Ok(outcome) = db.execute(&sql).await {
            if let Ok(rows) = outcome.rows::<BranchTipRow>(&sql) {
                if let Some(row) = rows.first() {
                    let hashes = row.tip_hashes.clone().unwrap_or_default();
                    let hashes: Vec<String> =
                        hashes.into_iter().filter(|h| !h.is_empty()).collect();
                    if let Some(first) = hashes.first() {
                        b.commit = first.clone();
                        b.source = "db_branch".to_string();
                        let branches: Vec<String> = row
                            .branches
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|x| !x.is_empty())
                            .collect();
                        if let Some(br) = branches.first() {
                            b.branch = br.clone();
                        }
                        persist_baseline(analyzer, &b).await;
                        return b;
                    }
                }
            }
        }
    }

    b
}

/// HEAD commit + abbreviated branch via the GitRunner (D-A07;
/// `gitHeadAndBranch` shelled out raw).
async fn git_head_and_branch(runner: &threski_git::GitRunner, repo_dir: &str) -> (String, String) {
    let Ok(out) = runner.run(repo_dir, &["rev-parse", "HEAD"]).await else {
        return (String::new(), String::new());
    };
    let commit = String::from_utf8_lossy(&out).trim().to_string();
    let branch = match runner
        .run(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
    {
        Ok(bout) => {
            let b = String::from_utf8_lossy(&bout).trim().to_string();
            if b == "HEAD" {
                String::new()
            } else {
                b
            }
        }
        Err(_) => String::new(),
    };
    (commit, branch)
}

/// Persists a resolved baseline back onto the project record
/// Bound variables (F-002).
async fn persist_baseline(analyzer: &LogAnalyzer, b: &TemporalBaseline) {
    let Some(db) = &analyzer.db else {
        return;
    };
    if analyzer.project.is_empty() {
        return;
    }
    let project_id = format_record_id(schema::PROJECT, &sanitize_id(&analyzer.project));
    let sql = format!(
        "UPSERT {project_id} SET baseline_commit = $commit, baseline_branch = $branch, baseline_source = $source;"
    );
    let _ = db
        .smart_query(
            &sql,
            vars([
                ("commit", b.commit.as_str()),
                ("branch", b.branch.as_str()),
                ("source", b.source.as_str()),
            ]),
        )
        .await;
}

/// Cause synthesis heuristic + AI refinement.
async fn synthesize_cause(
    analyzer: &LogAnalyzer,
    e: &SemanticError,
    snippet: &str,
    baseline: &TemporalBaseline,
) -> String {
    let ai_functional = analyzer
        .ai
        .as_ref()
        .map(|ai| ai.is_functional())
        .unwrap_or(false);

    if !snippet.is_empty() && !e.file.is_empty() {
        let mut base = format!("Correlated to {}", e.file);
        if e.line > 0 {
            base += &format!(":{}", e.line);
        }
        if !e.symbol.is_empty() {
            base += &format!(" ({})", e.symbol);
        }
        base += &format!(" using baseline {}. Nearby code: {}", baseline, snippet);
        if ai_functional {
            if let Some(refined) = refine_cause_with_ai(analyzer, e, snippet, baseline).await {
                return refined;
            }
        }
        return trim_snippet(&base, 500);
    }

    if !e.file.is_empty() {
        let mut msg = format!("Located at {}", e.file);
        if e.line > 0 {
            msg += &format!(":{}", e.line);
        }
        if !e.symbol.is_empty() {
            msg += &format!(" in {}", e.symbol);
        }
        msg += &format!(". Category: {}. Baseline: {}.", e.category, baseline);
        return msg;
    }

    if ai_functional {
        if let Some(refined) = refine_cause_with_ai(analyzer, e, snippet, baseline).await {
            return refined;
        }
    }

    if !e.category.is_empty() {
        return format!(
            "Classified as {:?}; no matching source location found in the knowledge graph for baseline {}.",
            e.category, baseline
        );
    }
    String::new()
}

/// One short AI-written root-cause sentence.
async fn refine_cause_with_ai(
    analyzer: &LogAnalyzer,
    e: &SemanticError,
    snippet: &str,
    baseline: &TemporalBaseline,
) -> Option<String> {
    let ai = analyzer.ai.as_ref()?;
    let prompt = format!(
        r#"You are diagnosing a production log anomaly for project {project} (code baseline: {baseline}).
Write ONE short root-cause sentence (max 40 words). Ground it in the code context when present. Do not invent APIs.
Category: {category}
File: {file}
Line: {line}
Symbol: {symbol}
Log/stack:
{stack}
Code context:
{snippet}
Reply with plain text only."#,
        project = analyzer.project,
        baseline = baseline,
        category = e.category,
        file = e.file,
        line = e.line,
        symbol = e.symbol,
        stack = trim_snippet(&e.stack_trace, 400),
        snippet = trim_snippet(snippet, 400),
    );

    let contents = [threski_ai::types::Content {
        role: String::new(),
        parts: vec![threski_ai::types::Part { text: prompt }],
    }];
    let cfg = threski_ai::types::GenerationConfig {
        temperature: 0.1,
        ..Default::default()
    };
    let cand = ai.generate_content(&contents, cfg).await.ok()?;
    let out = cand.content.parts.first()?.text.clone();
    let out = out
        .trim()
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\''])
        .to_string();
    Some(trim_snippet(&out, 400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_stack_location_go_paren() {
        let text = "at github.com/example/checkout/internal/db.(*Client).Query (client.go:142)\nat github.com/example/checkout/internal/search.(*Engine).Ask (search.go:88)";
        let (file, line, _) = extract_stack_location(text);
        assert_eq!(file, "client.go", "file={file:?} want client.go");
        assert_eq!(line, 142, "line={line} want 142");
    }

    #[test]
    fn extract_stack_location_java() {
        let text = "java.lang.NullPointerException\n\tat com.example.Service.run(Service.java:55)";
        let (file, line, sym) = extract_stack_location(text);
        assert!(
            file == "Service.java" && line == 55,
            "got {file}:{line} want Service.java:55"
        );
        assert_eq!(sym, "run", "symbol={sym:?} want run");
    }

    #[test]
    fn distinctive_terms_skips_noise() {
        let terms = distinctive_terms("ERROR failed connection refused database host", 4);
        for t in &terms {
            assert!(
                t != "error" && t != "failed",
                "unexpected stop word {t:?} in {terms:?}"
            );
        }
        assert!(!terms.is_empty());
        assert!(
            terms
                .iter()
                .any(|t| t == "connection" || t == "refused" || t == "database"),
            "terms={terms:?} missing expected keywords"
        );
    }

    #[test]
    fn enrichment_query_uses_category() {
        let q = enrichment_query(&SemanticError {
            category: "DB timeout".into(),
            stack_trace: "time=1 level=ERROR msg=\"boom\"".into(),
            ..Default::default()
        });
        assert!(!q.is_empty());
    }

    #[test]
    fn trim_snippet_respects_char_boundaries() {
        // 4-byte emoji: byte-based truncation would split it.
        let s = "🦀".repeat(10);
        let out = trim_snippet(&s, 3);
        assert!(out.ends_with('…'));
        assert_eq!(out.trim_end_matches('…'), "🦀🦀🦀");
    }

    #[test]
    fn baseline_display_matches_reference_format() {
        let b = TemporalBaseline {
            source: "workspace_head".into(),
            commit: "0123456789abcdef".into(),
            branch: "main".into(),
            repo: "threski".into(),
        };
        assert_eq!(
            b.to_string(),
            "repo=threski branch=main commit=0123456789ab via=workspace_head"
        );
        assert_eq!(TemporalBaseline::default().to_string(), "unknown");
    }
}
