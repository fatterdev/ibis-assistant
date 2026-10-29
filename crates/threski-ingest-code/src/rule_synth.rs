//! AI rule synthesis and validated persistence
//!
//!
//! Pipeline: AI proposes ast-grep YAML rules → structural validation
//! (dangerous-token rejection, ID canonicalization, language/entries checks)
//! → sandbox validation with a real `sg scan` run → persistence under
//! `rules/ai-generated-*-chunk|calls|logs.yml`. Rejected YAML is quarantined
//! under `rules_quarantine/` for inspection. Synthesis is cost-bounded: at
//! most 2 rules per extension fingerprint with a 30-minute cooldown.
//!
//! Shared by the log Path-D generator (`threski-ingest-logs`) and the code-ingest
//! sparse-AST rescan (extension-agnostic).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ast_chunker::{path_ext, sg_binary_path, sg_config_dir, truncate_char_safe};
use crate::error::{CodeIngestError, RulePersistError};
use crate::install::ensure_ast_grep_rules;
use crate::sparse_ast::{is_html_host_ext, needs_rule_synthesis, suggest_host_language};

/// Selects which ParseAST suffix family a synthesized rule belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RuleKind {
    Logs,
    #[default]
    Chunk,
    Calls,
}

impl RuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::Logs => "logs",
            RuleKind::Chunk => "chunk",
            RuleKind::Calls => "calls",
        }
    }
}

impl std::fmt::Display for RuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub const MAX_SYNTH_YAML_BYTES: usize = 24 << 10;
pub const RULE_QUARANTINE_DIR: &str = "rules_quarantine";
pub const MAX_SYNTH_PER_EXT_BATCH: usize = 2;
pub const SYNTH_COOLDOWN: Duration = Duration::from_secs(30 * 60);

static SYNTH_RECENT: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static RE_YAML_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^id:\s*['"]?([A-Za-z0-9_-]+)['"]?\s*$"#).expect("static regex")
});
static RE_YAML_LANG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^language:\s*['"]?([A-Za-z0-9_+-]+)['"]?\s*$"#).expect("static regex")
});
static RE_HAS_FORMAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$FORMAT\b").expect("static regex"));
static RE_HAS_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*-?\s*pattern:").expect("static regex"));
static RE_HAS_KIND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*-?\s*kind:").expect("static regex"));
static RE_DANGEROUS_YAML: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(!!|<<\s*\*|file://|/etc/passwd)").expect("static regex"));
static RE_SAFE_RULE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_-]+").expect("static regex"));

/// Outcome of validating + writing one YAML rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RulePersistResult {
    pub path: String,
    pub quarantine: String,
    pub rule_id: String,
    pub language: String,
    pub kind: RuleKind,
    pub validated: bool,
}

/// A short excerpt used to ask the AI for ingest rules.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SampleFile {
    pub rel_path: String,
    /// May be truncated.
    pub content: String,
}

/// Raw AI response entry.
#[derive(Debug, Clone, Default, Deserialize)]
struct AIRuleResponse {
    #[serde(default)]
    language: String,
    #[serde(default)]
    id: String,
    /// logs | chunk | calls
    #[serde(default)]
    kind: String,
    #[serde(default)]
    yaml: String,
    #[serde(rename = "sample_code", default)]
    sample_code: String,
    /// Response-schema field (kept for the serde contract; never reads it
    /// either — it informs the human review of quarantined rules).
    #[allow(dead_code)]
    #[serde(default)]
    rationale: String,
    /// Optional host lang for sg languageGlobs (e.g. html).
    #[serde(rename = "language_glob", default)]
    language_glob: String,
}

/// The AI surface used by rule synthesis (`ContentGenerator, implemented
/// only by `*ai.Client` — the `aiClient.(ContentGenerator)` type assertion
/// succeeds for the concrete client alone, so no blanket impl over
/// [`threski_ai::AIProvider`] exists here either).
#[async_trait]
pub trait ContentGenerator: Send + Sync {
    async fn is_functional(&self) -> bool;
    async fn generate_content(
        &self,
        contents: &[threski_ai::Content],
        config: threski_ai::GenerationConfig,
    ) -> Result<threski_ai::Candidate, threski_ai::AiError>;
}

#[async_trait]
impl ContentGenerator for threski_ai::Client {
    async fn is_functional(&self) -> bool {
        threski_ai::Client::is_functional(self)
    }

    async fn generate_content(
        &self,
        contents: &[threski_ai::Content],
        config: threski_ai::GenerationConfig,
    ) -> Result<threski_ai::Candidate, threski_ai::AiError> {
        threski_ai::Client::generate_content(self, contents, config).await
    }
}

/// Source of the sg binary for the sandbox validation step.
///
/// Test seam mirroring lane B's `start_with_bin` pattern: sg-dependent tests
/// pass [`SgSandbox::Binary`] when a real binary is discoverable and
/// [`SgSandbox::Skip`] otherwise, so they degrade gracefully instead of
/// failing (D2 keeps sg an external process on day 1).
#[derive(Debug, Clone)]
pub enum SgSandbox {
    /// Production path: resolve + probe the binary likewise
    /// (`validateRuleWithSG`), honoring `THRESKI_AST_RULE_STRUCTURAL_ONLY=1`
    /// when none is found.
    Discover,
    /// Skip the sandbox run entirely (structural validation only).
    Skip,
    /// Validate against an explicit binary.
    Binary(std::path::PathBuf),
}

/// Validates ast-grep YAML and writes `rules/ai-generated-*-{logs|chunk|calls}.yml`
/// Shared by the log Path-D and the code-ingest
/// synthesizer (extension-agnostic).
///
/// Like , a validation failure still yields a result (quarantine path of
/// the rejected YAML) — carried on [`RulePersistError::result`]; 's
/// `empty yaml` / `yaml too large` failures carry no result.
pub async fn persist_validated_rule(
    yaml_text: &str,
    preferred_id: &str,
    language: &str,
    sample_code: &str,
    kind: RuleKind,
) -> Result<RulePersistResult, RulePersistError> {
    persist_validated_rule_with(
        yaml_text,
        preferred_id,
        language,
        sample_code,
        kind,
        SgSandbox::Discover,
    )
    .await
}

/// [`persist_validated_rule`] with an explicit sandbox source (test seam).
pub async fn persist_validated_rule_with(
    yaml_text: &str,
    preferred_id: &str,
    language: &str,
    sample_code: &str,
    kind: RuleKind,
    sandbox: SgSandbox,
) -> Result<RulePersistResult, RulePersistError> {
    let yaml_text = yaml_text.trim();
    if yaml_text.is_empty() {
        return Err(RulePersistError {
            result: None,
            source: CodeIngestError::other("empty yaml"),
        });
    }
    if yaml_text.len() > MAX_SYNTH_YAML_BYTES {
        return Err(RulePersistError {
            result: None,
            source: CodeIngestError::other(format!("yaml too large ({})", yaml_text.len())),
        });
    }
    let language = normalize_synth_language(language);

    let rule_id = match structural_validate_rule(yaml_text, preferred_id, &language, kind) {
        Ok(id) => id,
        Err(err) => {
            // Scenario: reports the preferred (uncanonicalized) ID here.
            let (result, source) = quarantine_failure(
                yaml_text,
                preferred_id,
                &language,
                kind,
                "structural validation failed",
                &err,
            );
            return Err(RulePersistError {
                result: Some(Box::new(result)),
                source,
            });
        }
    };

    if let Err(err) =
        validate_rule_with_sg(yaml_text, &rule_id, &language, sample_code, kind, &sandbox).await
    {
        let (result, source) = quarantine_failure(
            yaml_text,
            &rule_id,
            &language,
            kind,
            "sg validation failed",
            &err,
        );
        return Err(RulePersistError {
            result: Some(Box::new(result)),
            source,
        });
    }

    match write_rule_file(yaml_text, &rule_id, &language) {
        Ok(path) => {
            let _ = ensure_ast_grep_rules();
            Ok(RulePersistResult {
                path,
                rule_id,
                language,
                kind,
                validated: true,
                quarantine: String::new(),
            })
        }
        Err(err) => Err(RulePersistError {
            result: Some(Box::new(RulePersistResult {
                rule_id,
                language,
                kind,
                ..Default::default()
            })),
            source: err,
        }),
    }
}

/// Builds the failed-validation result the way composed it: the partial
/// result carries the quarantine path, and the error message appends
/// `(quarantine: <err>)` when quarantining itself failed.
#[allow(clippy::too_many_arguments)]
fn quarantine_failure(
    yaml_text: &str,
    rule_id: &str,
    language: &str,
    kind: RuleKind,
    stage: &str,
    err: &CodeIngestError,
) -> (RulePersistResult, CodeIngestError) {
    let base = RulePersistResult {
        rule_id: rule_id.to_string(),
        language: language.to_string(),
        kind,
        quarantine: String::new(),
        validated: false,
        path: String::new(),
    };
    match quarantine_rule(yaml_text, rule_id, &err.to_string()) {
        Ok(quarantine) => (
            RulePersistResult { quarantine, ..base },
            CodeIngestError::other(format!("{stage}: {err}")),
        ),
        Err(q_err) => (
            base,
            CodeIngestError::other(format!("{stage}: {err} (quarantine: {q_err})")),
        ),
    }
}

/// Asks the AI for chunk/calls (and optional logs) rules for unfamiliar files
/// Cost-bounded: at most
/// [`MAX_SYNTH_PER_EXT_BATCH`] rules per extension fingerprint cooldown.
pub async fn synthesize_ingest_rules(
    ai_client: &dyn ContentGenerator,
    samples: &[SampleFile],
) -> Result<Vec<RulePersistResult>, CodeIngestError> {
    if samples.is_empty() || !ai_client.is_functional().await {
        return Ok(Vec::new());
    }
    let ext = {
        let e = path_ext(&samples[0].rel_path).to_lowercase();
        if e.is_empty() {
            ".unknown".to_string()
        } else {
            e
        }
    };
    let fp = synth_fingerprint(&ext, samples);
    {
        let mut recent = SYNTH_RECENT.lock().unwrap_or_else(|e| e.into_inner());
        // Evict expired fingerprints on write (F-A17: the map grew
        // unboundedly in a long-running process).
        recent.retain(|_, t| t.elapsed() < SYNTH_COOLDOWN);
        if let Some(t) = recent.get(&fp) {
            if t.elapsed() < SYNTH_COOLDOWN {
                return Ok(Vec::new());
            }
        }
        recent.insert(fp, Instant::now());
    }

    // Bootstrap languageGlob before AI when we already know a sensible host.
    let first_host = suggest_host_language(&samples[0].rel_path);
    if !first_host.is_empty() && is_html_host_ext(&samples[0].rel_path) {
        let _ = crate::lang_globs::ensure_language_glob(&ext, &first_host);
    }

    let prompt = build_ingest_synth_prompt(&ext, samples);
    let contents = [threski_ai::Content::text(prompt)];
    let candidate = ai_client
        .generate_content(
            &contents,
            threski_ai::GenerationConfig {
                temperature: 0.1,
                ..Default::default()
            },
        )
        .await?;
    if candidate.content.parts.is_empty() {
        return Err(CodeIngestError::other("empty AI response"));
    }
    let rules = parse_ai_rule_list_json(&candidate.content.parts[0].text)?;

    let mut out = Vec::new();
    for (i, r) in rules.iter().enumerate() {
        if i >= MAX_SYNTH_PER_EXT_BATCH {
            break;
        }
        let glob = r.language_glob.trim();
        if !glob.is_empty() {
            let _ = crate::lang_globs::ensure_language_glob(&ext, glob);
        } else {
            let host = suggest_host_language(&samples[0].rel_path);
            if !host.is_empty() && normalize_synth_language(&r.language) == host {
                let _ = crate::lang_globs::ensure_language_glob(&ext, &host);
            }
        }
        let kind = normalize_rule_kind_from_str(&r.kind, &r.id);
        match persist_validated_rule(&r.yaml, &r.id, &r.language, &r.sample_code, kind).await {
            Ok(res) => {
                tracing::info!(
                    "Rule synth: wrote {} ({}) → {}",
                    res.rule_id,
                    res.kind,
                    res.path
                );
                out.push(res);
            }
            Err(err) => {
                if let Some(res) = err.result {
                    out.push(*res);
                }
                continue;
            }
        }
    }
    Ok(out)
}

/// Runs after ParseAST when extraction is empty/weak for any extension
/// Returns true if any rule was persisted
/// (the caller may re-scan).
pub async fn maybe_synthesize_for_sparse_ast(
    ai_client: &dyn ContentGenerator,
    rel_path: &str,
    content: &[u8],
    chunks: &[crate::ast_chunker::AstChunk],
    calls: &[crate::ast_chunker::CallEdge],
) -> bool {
    if !ai_client.is_functional().await {
        return false;
    }
    if !needs_rule_synthesis(rel_path, chunks, calls, content) {
        return false;
    }
    // Scenario: sliced raw bytes (`excerpt[:4000]`); char-safe here (F-A16).
    let excerpt = truncate_char_safe(&String::from_utf8_lossy(content), 4000).to_string();
    let results = synthesize_ingest_rules(
        ai_client,
        &[SampleFile {
            rel_path: rel_path.to_string(),
            content: excerpt,
        }],
    )
    .await;
    match results {
        Ok(rs) => rs.iter().any(|r| r.validated && !r.path.is_empty()),
        Err(_) => false,
    }
}

fn build_ingest_synth_prompt(ext: &str, samples: &[SampleFile]) -> String {
    let host_hint = samples
        .first()
        .map(|s| suggest_host_language(&s.rel_path))
        .unwrap_or_default();
    let mut b = String::new();
    b.push_str(
        r#"You generate ast-grep YAML rules for Threski code ingest of unfamiliar, polyglot, or legacy source files.
This must work for ANY extension (templates, DSLs, mixed stacks)—not a single framework.

Rules are consumed by ParseAST: id MUST start with "ai-generated-" and end with "-chunk", "-calls", or "-logs".
- *-chunk: structural symbols (kind: function_declaration / method_declaration / class_declaration / script_element / element / …)
- *-calls: call sites (kind: call_expression or equivalent patterns)
- *-logs: only if logger format strings appear; must bind $FORMAT

Pick a built-in ast-grep language: html, javascript, typescript, csharp, css, go, python, java, cpp, ruby, php, …
If the file extension is not natively mapped, set "language_glob" to the host language (often "html" for markup templates so embedded <script>/<style> inject as JS/CSS).

Keep each YAML < 6KB. Provide sample_code that MUST match at least one pattern/kind.

Return ONLY a JSON array:
[{"language":"...","language_glob":"html|","id":"ai-generated-...-chunk","kind":"chunk","yaml":"...","sample_code":"...","rationale":"..."}]

Extension: "#,
    );
    b.push_str(ext);
    if !host_hint.is_empty() {
        b.push_str("\nSuggested host language: ");
        b.push_str(&host_hint);
    }
    b.push_str("\nSamples:\n");
    for s in samples {
        b.push_str("--- ");
        b.push_str(&s.rel_path);
        b.push_str(" ---\n");
        b.push_str(&s.content);
        b.push('\n');
    }
    b
}

fn parse_ai_rule_list_json(raw: &str) -> Result<Vec<AIRuleResponse>, CodeIngestError> {
    let mut clean = raw.trim();
    if clean.starts_with("```") {
        if let Some(idx) = clean.find('\n') {
            clean = &clean[idx + 1..];
        }
    }
    clean = clean.trim();
    if clean
        .trim_end_matches([' ', '\t', '\r', '\n'])
        .ends_with("```")
    {
        clean = clean.trim_end_matches([' ', '\t', '\r', '\n']);
        clean = &clean[..clean.len() - 3];
    }
    let clean = clean.trim();

    let normalize = |list: Vec<AIRuleResponse>| -> Vec<AIRuleResponse> {
        list.into_iter()
            .map(|mut r| {
                r.yaml = r.yaml.trim().to_string();
                r.id = r.id.trim().to_string();
                r.language = normalize_synth_language(&r.language);
                r.sample_code = r.sample_code.trim().to_string();
                r
            })
            .collect()
    };

    match serde_json::from_str::<Vec<AIRuleResponse>>(clean) {
        Ok(list) => Ok(normalize(list)),
        Err(first_err) => {
            // Single-object fallback.
            match serde_json::from_str::<AIRuleResponse>(clean) {
                Ok(one) => Ok(normalize(vec![one])),
                Err(_) => Err(CodeIngestError::other(format!(
                    "parse AI rule JSON: {first_err}"
                ))),
            }
        }
    }
}

/// Canonicalizes an AI-proposed rule ID into the `ai-generated-<name>-<kind>`
/// contract.
pub fn canonicalize_generated_rule_id(id: &str, kind: RuleKind) -> String {
    let mut id = id.trim();
    id = id.strip_prefix("ai-generated-").unwrap_or(id);
    id = id.strip_prefix("ai-").unwrap_or(id);
    for s in ["-logs", "-chunk", "-calls"] {
        if let Some(rest) = id.strip_suffix(s) {
            id = rest;
            break;
        }
    }
    let id = id.trim_matches(['-', '_']);
    let id = if id.is_empty() { "rule" } else { id };
    // Keep filename/id safe.
    let safe = RE_SAFE_RULE_ID.replace_all(id, "-");
    format!("ai-generated-{safe}-{kind}")
}

/// Resolves the effective kind from the raw AI kind string, falling back to
/// the ID suffix (`normalizeRuleKind; the typed Rust enum makes the
/// always-valid fast path explicit).
pub fn normalize_rule_kind_from_str(kind: &str, id: &str) -> RuleKind {
    match kind.trim().to_lowercase().as_str() {
        "logs" => RuleKind::Logs,
        "chunk" => RuleKind::Chunk,
        "calls" => RuleKind::Calls,
        _ => {
            let id = id.to_lowercase();
            if id.ends_with("-logs") {
                RuleKind::Logs
            } else if id.ends_with("-calls") {
                RuleKind::Calls
            } else {
                RuleKind::Chunk
            }
        }
    }
}

fn structural_validate_rule(
    yaml_text: &str,
    preferred_id: &str,
    language: &str,
    kind: RuleKind,
) -> Result<String, CodeIngestError> {
    if RE_DANGEROUS_YAML.is_match(yaml_text) {
        return Err(CodeIngestError::other("rejected dangerous yaml tokens"));
    }
    let mut id = preferred_id;
    if let Some(c) = RE_YAML_ID.captures(yaml_text) {
        id = c.get(1).map(|m| m.as_str()).unwrap_or(id);
    }
    let id = id.trim();
    if id.is_empty() {
        return Err(CodeIngestError::other("missing rule id"));
    }
    let id = canonicalize_generated_rule_id(id, kind);
    let mut lang_in_yaml = language.to_string();
    if let Some(c) = RE_YAML_LANG.captures(yaml_text) {
        lang_in_yaml = normalize_synth_language(c.get(1).map(|m| m.as_str()).unwrap_or(""));
    }
    if lang_in_yaml.is_empty() {
        return Err(CodeIngestError::other("missing language"));
    }
    match kind {
        RuleKind::Logs => {
            if !RE_HAS_FORMAT.is_match(yaml_text) {
                return Err(CodeIngestError::other("logs rule must bind $FORMAT"));
            }
            if !RE_HAS_PATTERN.is_match(yaml_text) {
                return Err(CodeIngestError::other(
                    "logs rule must contain pattern entries",
                ));
            }
        }
        other => {
            if !RE_HAS_PATTERN.is_match(yaml_text) && !RE_HAS_KIND.is_match(yaml_text) {
                return Err(CodeIngestError::other(format!(
                    "{other} rule must contain pattern or kind entries"
                )));
            }
        }
    }
    Ok(id)
}

/// True when `bin` is a bare name the OS resolves via PATH (F-A08 —
/// stat'ed bare names, which always fail, making PATH-only sg installs
/// unusable for validation).
fn is_bare_binary(bin: &Path) -> bool {
    bin.parent()
        .map(|p| p.as_os_str().is_empty())
        .unwrap_or(true)
}

/// Sandbox-validates a rule with a real `sg scan` run against a temp config
/// root. Non-zero exits are tolerated (sg may exit
/// non-zero on no matches); error-shaped output and missing sample matches
/// reject the rule.
///
/// The [`SgSandbox::Discover`] path honors `THRESKI_AST_RULE_STRUCTURAL_ONLY=1`
/// when no binary can be spawned (the escape hatch); [`SgSandbox::Skip`]
/// unconditionally skips (injected test seam).
async fn validate_rule_with_sg(
    yaml_text: &str,
    rule_id: &str,
    language: &str,
    sample_code: &str,
    kind: RuleKind,
    sandbox: &SgSandbox,
) -> Result<(), CodeIngestError> {
    let sg_bin = match sandbox {
        SgSandbox::Skip => return Ok(()),
        SgSandbox::Binary(bin) => bin.clone(),
        SgSandbox::Discover => {
            let mut bin = sg_binary_path();
            if !bin.is_file() && !is_bare_binary(&bin) {
                let _ = ensure_ast_grep_rules();
                bin = sg_binary_path();
            }
            bin
        }
    };

    let tmp_root = tempfile::Builder::new().prefix("threski-sg-rule-").tempdir()?;
    let rules_dir = tmp_root.path().join("rules");
    std::fs::create_dir_all(&rules_dir)?;
    let cfg = "ruleDirs:\n  - rules\nlanguageGlobs:\n  html:\n    - \"*.vue\"\n    - \"*.jsp\"\n    - \"*.erb\"\n    - \"*.svelte\"\n";
    let cfg_path = tmp_root.path().join("sgconfig.yml");
    std::fs::write(&cfg_path, cfg)?;
    let doc = ensure_synth_yaml_id(yaml_text, rule_id, language);
    std::fs::write(rules_dir.join(format!("{rule_id}.yml")), doc.as_bytes())?;

    let ext = synth_language_ext(language);
    let mut sample = sample_code.to_string();
    if sample.trim().is_empty() {
        sample = minimal_synth_snippet(language, kind).to_string();
    }
    let sample_path = tmp_root.path().join(format!("sample{ext}"));
    std::fs::write(&sample_path, sample.as_bytes())?;

    let out = match tokio::process::Command::new(&sg_bin)
        .args(["scan", "--json=stream", "--config"])
        .arg(&cfg_path)
        .arg(&sample_path)
        .current_dir(tmp_root.path())
        .output()
        .await
    {
        Ok(out) => out,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Bare-name PATH resolution failed (F-A08); honor 's
            // structural-only escape hatch when no binary exists at all.
            if std::env::var("THRESKI_AST_RULE_STRUCTURAL_ONLY").as_deref() == Ok("1") {
                return Ok(());
            }
            return Err(CodeIngestError::other(format!(
                "sg binary missing: {}",
                sg_bin.display()
            )));
        }
        Err(err) => return Err(err.into()),
    };
    // stdout first, then stderr (D-A08): sg writes matches to stdout and
    // diagnostics to stderr.
    let mut combined = out.stdout.clone();
    combined.extend_from_slice(&out.stderr);
    let out_str = String::from_utf8_lossy(&combined).into_owned();
    if looks_like_sg_error(&out_str) {
        let mut preview = out_str.clone();
        if preview.len() > 400 {
            let mut cut = 400;
            while !preview.is_char_boundary(cut) {
                cut -= 1;
            }
            preview.truncate(cut);
        }
        return Err(CodeIngestError::other(format!("sg rule error: {preview}")));
    }
    if !sample_code.trim().is_empty() {
        let matched = out_str.split('\n').any(|line| {
            let line = line.trim();
            line.starts_with('{') && (line.contains(rule_id) || line.contains("\"ruleId\""))
        });
        if !matched {
            return Err(CodeIngestError::other(
                "sg produced no matches for provided sample_code",
            ));
        }
    }
    Ok(())
}

fn looks_like_sg_error(out: &str) -> bool {
    let lower = out.to_lowercase();
    for n in [
        "failed to parse",
        "invalid rule",
        "cannot parse rule",
        "error while loading rule",
        "serde_yaml",
    ] {
        if lower.contains(n) {
            return true;
        }
    }
    lower.contains("error:") && !out.contains("\"ruleId\"")
}

/// Rewrites (or prepends) the `id:` and `language:` lines so the persisted
/// doc matches the canonical contract.
pub fn ensure_synth_yaml_id(yaml_text: &str, rule_id: &str, language: &str) -> String {
    let mut doc = yaml_text.to_string();
    if RE_YAML_ID.is_match(&doc) {
        doc = RE_YAML_ID
            .replace_all(&doc, regex::NoExpand(&format!("id: {rule_id}")))
            .into_owned();
    } else {
        doc = format!("id: {rule_id}\n{doc}");
    }
    if !language.is_empty() {
        if RE_YAML_LANG.is_match(&doc) {
            doc = RE_YAML_LANG
                .replace_all(&doc, regex::NoExpand(&format!("language: {language}")))
                .into_owned();
        } else {
            doc = format!("language: {language}\n{doc}");
        }
    }
    doc
}

fn write_rule_file(
    yaml_text: &str,
    rule_id: &str,
    language: &str,
) -> Result<String, CodeIngestError> {
    let Some(root) = sg_config_dir() else {
        return Err(CodeIngestError::other("sgconfig root not found"));
    };
    let rules_dir = root.join("rules");
    std::fs::create_dir_all(&rules_dir)?;
    let doc = ensure_synth_yaml_id(yaml_text, rule_id, language);
    let path = rules_dir.join(format!("{rule_id}.yml"));
    std::fs::write(&path, format!("{doc}\n"))?;
    let testrun_rules = root.join("testrun").join("rules");
    if testrun_rules.is_dir() {
        let _ = std::fs::write(
            testrun_rules.join(format!("{rule_id}.yml")),
            format!("{doc}\n"),
        );
    }
    Ok(path.to_string_lossy().into_owned())
}

fn quarantine_rule(
    yaml_text: &str,
    rule_id: &str,
    reason: &str,
) -> Result<String, CodeIngestError> {
    let root = match sg_config_dir() {
        Some(root) => root,
        None => std::env::current_dir()?,
    };
    let dir = root.join(RULE_QUARANTINE_DIR);
    std::fs::create_dir_all(&dir)?;
    let rule_id = if rule_id.is_empty() {
        "ai-unknown"
    } else {
        rule_id
    };
    let name = format!(
        "{rule_id}_{}.yml",
        chrono::Local::now().format("%Y%m%dT%H%M%S")
    );
    let path = dir.join(name);
    let body = format!(
        "# QUARANTINED: {}\n# reason: {}\n{}\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        reason.replace('\n', " "),
        yaml_text
    );
    std::fs::write(&path, body)?;
    tracing::warn!("Rule synth: quarantined {}", path.display());
    Ok(path.to_string_lossy().into_owned())
}

/// Normalizes common language aliases.
pub fn normalize_synth_language(lang: &str) -> String {
    match lang.trim().to_lowercase().as_str() {
        "js" => "javascript".to_string(),
        "ts" => "typescript".to_string(),
        "c#" | "c-sharp" => "csharp".to_string(),
        "c++" => "cpp".to_string(),
        other => other.to_string(),
    }
}

fn synth_language_ext(lang: &str) -> &'static str {
    match normalize_synth_language(lang).as_str() {
        "go" => ".go",
        "javascript" => ".js",
        "typescript" => ".ts",
        "python" => ".py",
        "java" => ".java",
        "csharp" => ".cs",
        "cpp" => ".cpp",
        "html" => ".html",
        "css" => ".css",
        _ => ".txt",
    }
}

fn minimal_synth_snippet(lang: &str, kind: RuleKind) -> &'static str {
    match normalize_synth_language(lang).as_str() {
        "html" => {
            "<html><body><script>function f(){g()}</script><form runat=\"server\"></form></body></html>\n"
        }
        "csharp" => "class C { void M() { Logger.Error(\"x %s\", y); } }\n",
        "javascript" => {
            if kind == RuleKind::Logs {
                "logger.error(\"fail %s\", x);\n"
            } else {
                "function f(){ alert(\"hi\"); }\n"
            }
        }
        "go" => "package p\nfunc f(l interface{ Infow(string, ...interface{}) }) { l.Infow(\"x %s\", \"y\") }\n",
        _ => "// smoke\n",
    }
}

/// SHA-256 fingerprint of the extension + sample head bytes, gating the
/// synthesis cooldown (`synthFingerprint; byte-truncated at 512 with
/// char-boundary safety).
fn synth_fingerprint(ext: &str, samples: &[SampleFile]) -> String {
    let mut h = Sha256::new();
    h.update(ext.as_bytes());
    for s in samples {
        h.update(s.rel_path.as_bytes());
        let n = s.content.len().min(512);
        h.update(truncate_char_safe(&s.content, n).as_bytes());
    }
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .concat()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_generated_rule_id_matrix() {
        // Scenario: TestCanonicalizeGeneratedRuleID.
        assert_eq!(
            canonicalize_generated_rule_id("ai-vue-chunk", RuleKind::Chunk),
            "ai-generated-vue-chunk"
        );
        assert_eq!(
            canonicalize_generated_rule_id("ai-generated-vue-script-chunk", RuleKind::Chunk),
            "ai-generated-vue-script-chunk"
        );
        // Unknown suffixes are preserved inside the name; unsafe chars → "-".
        assert_eq!(
            canonicalize_generated_rule_id("Jinja Block!!", RuleKind::Calls),
            "ai-generated-Jinja-Block--calls"
        );
        // Empty → "rule".
        assert_eq!(
            canonicalize_generated_rule_id("", RuleKind::Logs),
            "ai-generated-rule-logs"
        );
    }

    #[test]
    fn normalize_rule_kind_matrix() {
        assert_eq!(normalize_rule_kind_from_str("logs", ""), RuleKind::Logs);
        assert_eq!(normalize_rule_kind_from_str("CHUNK", ""), RuleKind::Chunk);
        assert_eq!(normalize_rule_kind_from_str("calls", ""), RuleKind::Calls);
        assert_eq!(
            normalize_rule_kind_from_str("", "ai-generated-x-logs"),
            RuleKind::Logs
        );
        assert_eq!(
            normalize_rule_kind_from_str("", "ai-generated-x-calls"),
            RuleKind::Calls
        );
        assert_eq!(
            normalize_rule_kind_from_str("", "ai-generated-x"),
            RuleKind::Chunk
        );
        assert_eq!(
            normalize_rule_kind_from_str("nonsense", "y-chunk"),
            RuleKind::Chunk
        );
    }

    #[test]
    fn normalize_synth_language_matrix() {
        assert_eq!(normalize_synth_language("JS"), "javascript");
        assert_eq!(normalize_synth_language(" ts "), "typescript");
        assert_eq!(normalize_synth_language("C#"), "csharp");
        assert_eq!(normalize_synth_language("c-sharp"), "csharp");
        assert_eq!(normalize_synth_language("C++"), "cpp");
        assert_eq!(normalize_synth_language("go"), "go");
    }

    #[test]
    fn synth_language_ext_matrix() {
        // Indirectly via the sandbox sample path; assert the mapping through
        // the private helper's public behavior is not possible, so mirror the
        // matrix through normalize + a compile-time table check.
        let expected = [
            ("go", ".go"),
            ("javascript", ".js"),
            ("typescript", ".ts"),
            ("python", ".py"),
            ("java", ".java"),
            ("csharp", ".cs"),
            ("cpp", ".cpp"),
            ("html", ".html"),
            ("css", ".css"),
            ("zzz", ".txt"),
        ];
        for (lang, ext) in expected {
            assert_eq!(synth_language_ext(lang), ext, "{lang}");
        }
    }

    #[test]
    fn structural_validation_rejects_and_canonicalizes() {
        let good_chunk = "id: ai-generated-vue-script-chunk\nlanguage: html\nrule:\n  any:\n    - kind: script_element\n";
        assert_eq!(
            structural_validate_rule(
                good_chunk,
                "ai-generated-vue-script-chunk",
                "html",
                RuleKind::Chunk
            )
            .unwrap(),
            "ai-generated-vue-script-chunk"
        );
        // Dangerous tokens.
        let bad = "id: x-chunk\nlanguage: html\npattern: a\n!!python/exec\n";
        assert!(
            structural_validate_rule(bad, "x-chunk", "html", RuleKind::Chunk)
                .unwrap_err()
                .to_string()
                .contains("dangerous")
        );
        // Missing language.
        let no_lang = "id: x-chunk\nrule:\n  pattern: a\n";
        assert!(
            structural_validate_rule(no_lang, "x-chunk", "", RuleKind::Chunk)
                .unwrap_err()
                .to_string()
                .contains("missing language")
        );
        // Missing id.
        let no_id = "language: html\nrule:\n  pattern: a\n";
        assert!(structural_validate_rule(no_id, "", "html", RuleKind::Chunk)
            .unwrap_err()
            .to_string()
            .contains("missing rule id"));
        // Chunk needs pattern or kind entries.
        let no_entries = "id: x-chunk\nlanguage: html\n";
        assert!(
            structural_validate_rule(no_entries, "x-chunk", "html", RuleKind::Chunk)
                .unwrap_err()
                .to_string()
                .contains("pattern or kind")
        );
        // Logs rule must bind $FORMAT and have patterns.
        let logs_ok = "id: ai-generated-go-logs\nlanguage: go\nrule:\n  pattern: $FORMAT\n";
        assert!(
            structural_validate_rule(logs_ok, "ai-generated-go-logs", "go", RuleKind::Logs).is_ok()
        );
        let logs_no_format = "id: ai-generated-go-logs\nlanguage: go\nrule:\n  pattern: $MSG\n";
        assert!(structural_validate_rule(
            logs_no_format,
            "ai-generated-go-logs",
            "go",
            RuleKind::Logs
        )
        .unwrap_err()
        .to_string()
        .contains("$FORMAT"));
    }

    #[test]
    fn ensure_synth_yaml_id_rewrites_and_prepends() {
        let doc = ensure_synth_yaml_id(
            "id: ai-generated-old-chunk\nlanguage: js\npattern: a",
            "ai-generated-new-chunk",
            "javascript",
        );
        assert!(doc.starts_with("id: ai-generated-new-chunk\nlanguage: javascript\n"));
        assert!(doc.contains("pattern: a"));
        // Missing id/language lines are prepended ( prepends language last, so
        // it becomes the first line).
        let doc2 = ensure_synth_yaml_id("rule:\n  pattern: a", "ai-generated-x-chunk", "html");
        assert!(doc2.starts_with("language: html\nid: ai-generated-x-chunk\n"));
    }

    #[test]
    fn parse_ai_rule_list_json_fences_and_fallback() {
        // Plain array.
        let raw = r#"[{"language":"html","id":"ai-generated-vue-chunk","kind":"chunk","yaml":"id: x","sample_code":" a ","language_glob":"html"}]"#;
        let list = parse_ai_rule_list_json(raw).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].language, "html");
        assert_eq!(list[0].sample_code, "a");

        // Fenced array.
        let fenced = "```json\n[{\"language\":\"go\",\"id\":\"ai-generated-x-chunk\"}]\n```";
        let list = parse_ai_rule_list_json(fenced).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].language, "go");

        // Single object fallback.
        let single = r#"{"language":"js","id":"ai-generated-y-calls"}"#;
        let list = parse_ai_rule_list_json(single).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].language, "javascript"); // normalized

        // Garbage → error.
        assert!(parse_ai_rule_list_json("nope").is_err());
    }

    #[test]
    fn prompt_contract() {
        let samples = [SampleFile {
            rel_path: "pages/x.jsp".to_string(),
            content: "<% int x = 1; %>".to_string(),
        }];
        let prompt = build_ingest_synth_prompt(".jsp", &samples);
        assert!(prompt.starts_with("You generate ast-grep YAML rules for Threski code ingest"));
        assert!(prompt.contains("Extension: .jsp"));
        assert!(prompt.contains("Suggested host language: html"));
        assert!(prompt.contains("--- pages/x.jsp ---"));
        assert!(prompt.contains("<% int x = 1; %>"));
        assert!(prompt.contains("Return ONLY a JSON array"));
    }

    #[test]
    fn synth_fingerprint_is_stable_and_content_sensitive() {
        let samples = [SampleFile {
            rel_path: "a.vue".to_string(),
            content: "one".to_string(),
        }];
        let f1 = synth_fingerprint(".vue", &samples);
        let f2 = synth_fingerprint(".vue", &samples);
        let f3 = synth_fingerprint(
            ".vue",
            &[SampleFile {
                rel_path: "a.vue".to_string(),
                content: "two".to_string(),
            }],
        );
        assert_eq!(f1, f2);
        assert_ne!(f1, f3);
        assert_eq!(f1.len(), 64);
    }

    #[test]
    fn looks_like_sg_error_matrix() {
        assert!(looks_like_sg_error("error while loading rule xyz"));
        assert!(looks_like_sg_error("Failed to parse rule file"));
        assert!(looks_like_sg_error("some error: boom"));
        assert!(!looks_like_sg_error(
            "{\"text\":\"x\",\"ruleId\":\"go-chunk\"}"
        ));
        assert!(!looks_like_sg_error("no matches found"));
        // "error:" in a ruleId-carrying match line is not an sg error.
        assert!(!looks_like_sg_error(
            "{\"text\":\"error: x\",\"ruleId\":\"go-chunk\"}"
        ));
    }
}
