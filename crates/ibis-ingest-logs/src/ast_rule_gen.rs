//! Path-D: AI → ast-grep rule generation.
//!
//! When AI classification produced errors from in-repo source files whose
//! logger style is not yet covered by `*-logs.yml` rules, the analyzer asks
//! the model for an ast-grep YAML rule, validates it structurally (and via an
//! optional `sg` dry-run), and persists it under `rules/ai-generated-*-logs.yml`.
//! Invalid rules are quarantined outside `ruleDirs` so `ParseAST` stays healthy.
//!
//! Delegation seam: delegated persistence to the unified code-ingest
//! synthesizer (`code.PersistValidatedRule`, `code.SGBinaryPath`,
//! `code.SGConfigDir`, `code.EnsureAstGrepRules`). Because `ibis-ingest-code`
//! is a parallel lane, this crate ships the [`RuleSynthesizer`] trait with a
//! self-contained default ([`LocalRuleSynthesizer`]) that ports the same
//! behavior (structural validation + canonical IDs + sg sandbox + quarantine +
//! rules sync). The wiring lane may substitute the ingest-code implementation.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;

use crate::analyzer::{strip_code_fences, LogAnalyzer, SemanticError};
use crate::error::{LogsError, Result};

pub const MAX_AST_RULE_SAMPLE_LINES: usize = 12;
pub const MAX_AST_RULE_YAML_BYTES: usize = 24 << 10;
pub const AST_RULE_QUARANTINE_DIR: &str = "rules_quarantine";
pub const AST_RULE_GEN_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Global fingerprint → last-attempt registry ( package-level
/// `astRuleGenRecent` guarded by `astRuleGenMu`).
fn recent_registry() -> &'static Mutex<HashMap<String, Instant>> {
    static REG: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The outcome of one AI → ast-grep rule attempt.
#[derive(Debug, Clone, Default)]
pub struct ASTRuleGenResult {
    /// Written rule path (empty if quarantined / skipped).
    pub path: String,
    /// Quarantine path when validation failed.
    pub quarantine: String,
    pub rule_id: String,
    pub language: String,
    pub validated: bool,
}

/// Mirrors the dual `(result, error)` return for rule persistence:
/// validation failures still carry the quarantined result.
#[derive(Debug)]
pub enum RulePersistOutcome {
    /// Validation and write succeeded.
    Written(ASTRuleGenResult),
    /// Validation failed; the YAML was quarantined.
    Quarantined {
        result: ASTRuleGenResult,
        error: LogsError,
    },
    /// The YAML was empty or too large — nothing persisted.
    Rejected(LogsError),
}

impl RulePersistOutcome {
    pub fn ok(&self) -> bool {
        matches!(self, RulePersistOutcome::Written(_))
    }
}

#[derive(Debug, Deserialize)]
pub struct AiAstRuleResponse {
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub yaml: String,
    #[serde(rename = "sample_code", default)]
    pub sample_code: String,
    #[serde(default)]
    pub rationale: String,
}

/// Persistence seam replacing the `code.PersistValidatedRule` delegation.
#[async_trait]
pub trait RuleSynthesizer: Send + Sync {
    /// Validates ast-grep YAML and writes `rules/ai-generated-*-logs.yml`
    /// (the kind is fixed to `logs` for this pipeline).
    async fn persist_validated_rule(
        &self,
        yaml: &str,
        preferred_id: &str,
        language: &str,
        sample_code: &str,
    ) -> RulePersistOutcome;

    /// Best-effort `EnsureAstGrepRules` sync (callers ignore errors).
    fn ensure_rules(&self) {}
}

impl LogAnalyzer {
    /// Gates Path-D AST rule synthesis:
    /// at most once per `process_batch_sync`, only with a functional AI, only
    /// for errors that likely come from in-repo source, and honoring the
    /// global 30-minute fingerprint cooldown.
    pub(crate) fn should_attempt_ast_rule_gen(&self, errors: &[SemanticError]) -> bool {
        let functional = self
            .ai
            .as_ref()
            .map(|ai| ai.is_functional())
            .unwrap_or(false);
        if !functional || errors.is_empty() {
            return false;
        }
        if self
            .ast_rule_gen_used
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        let lang = infer_language_from_errors(errors);
        if lang.is_empty() {
            return false;
        }
        let fp = ast_rule_fingerprint(&lang, errors);
        let reg = recent_registry().lock().unwrap();
        if let Some(t) = reg.get(&fp) {
            if t.elapsed() < AST_RULE_GEN_COOLDOWN {
                return false;
            }
        }
        true
    }

    /// Asks the AI for an ast-grep log extractor rule, validates it (structure
    /// and optional sg dry-run), and persists under `rules/ai-*-logs.yml`
    /// Returns the outcome of the attempt;
    /// `Ok(None)` when skipped.
    pub async fn generate_ast_rules_from_errors(
        &self,
        errors: &[SemanticError],
    ) -> Result<Option<ASTRuleGenResult>> {
        if !self.should_attempt_ast_rule_gen(errors) {
            return Ok(None);
        }
        self.ast_rule_gen_used
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let lang = infer_language_from_errors(errors);
        let fp = ast_rule_fingerprint(&lang, errors);
        recent_registry().lock().unwrap().insert(fp, Instant::now());

        let samples = collect_sample_lines(errors, MAX_AST_RULE_SAMPLE_LINES);
        if samples.is_empty() {
            return Ok(None);
        }

        let resp = self.ask_ai_for_ast_rule(&lang, &samples, errors).await?;
        let Some(resp) = resp else {
            return Ok(None);
        };

        let outcome = self
            .rule_synth
            .persist_validated_rule(&resp.yaml, &resp.id, &resp.language, &resp.sample_code)
            .await;
        match outcome {
            RulePersistOutcome::Written(result) => {
                self.rule_synth.ensure_rules();
                tracing::info!(
                    "AST rule gen: wrote validated rule {} → {} (ingest_code may be needed for full LogAlign effect)",
                    result.rule_id,
                    result.path
                );
                Ok(Some(result))
            }
            RulePersistOutcome::Quarantined { result, error } => {
                tracing::warn!("AST rule gen quarantined: {error}");
                Ok(Some(result))
            }
            RulePersistOutcome::Rejected(error) => Err(error),
        }
    }

    /// Builds the Path-D prompt and calls the AI.
    async fn ask_ai_for_ast_rule(
        &self,
        lang: &str,
        samples: &[String],
        errors: &[SemanticError],
    ) -> Result<Option<AiAstRuleResponse>> {
        let Some(ai) = &self.ai else {
            return Err(LogsError::other("AI client is not functional"));
        };
        let mut file_hints: Vec<&str> = Vec::new();
        for e in errors {
            if !e.file.is_empty() && file_hints.len() < 4 {
                file_hints.push(&e.file);
            }
        }

        let prompt = format!(
            r#"You generate an ast-grep YAML rule that extracts log FORMAT strings from source code.
The rule will be used like human-written rules in rules/*-logs.yml: matches must expose metavar $FORMAT (the format/message string literal).

Constraints:
- language: {lang} (or refine if samples clearly indicate otherwise; one of: go, javascript, typescript, python, java, csharp, cpp)
- id MUST start with "ai-generated-" and MUST end with "-logs" (example: ai-generated-go-zap-logs)
- Use rule.any with pattern entries; every pattern that captures a message must bind $FORMAT
- Prefer logger call shapes NOT already covered by generic Info/Error/Printf patterns (e.g. Infow, Errorw, With().Info, slog.Log, custom wrappers)
- Do NOT invent unrelated languages. Keep YAML small (< 8KB).
- Also provide a short sample_code snippet in that language that MUST match at least one pattern and include a realistic format string literal.

Return ONLY JSON (no markdown):
{{"language":"...","id":"ai-generated-...-logs","yaml":"<full yaml document>","sample_code":"<snippet>","rationale":"<one sentence>"}}

Project: {project}
File hints: {file_hints}
Sample runtime log lines:
{samples}
"#,
            lang = lang,
            project = self.project,
            file_hints = file_hints.join(", "),
            samples = samples.join("\n"),
        );

        let contents = [ibis_ai::types::Content {
            role: String::new(),
            parts: vec![ibis_ai::types::Part { text: prompt }],
        }];
        let cfg = ibis_ai::types::GenerationConfig {
            temperature: 0.1,
            ..Default::default()
        };
        let candidate = ai.generate_content(&contents, cfg).await?;
        if let Some(usage) = &candidate.usage_metadata {
            self.cost_tracker
                .record_usage(
                    &self.log_file_id,
                    usage.prompt_token_count.max(0) as u64,
                    usage.candidates_token_count.max(0) as u64,
                )
                .await;
        }
        let Some(text) = candidate.content.parts.first().map(|p| p.text.clone()) else {
            return Err(LogsError::other("empty AI response"));
        };
        Ok(Some(parse_ai_ast_rule_json(&text)?))
    }
}

/// Parses the fenced AI JSON reply.
pub fn parse_ai_ast_rule_json(raw: &str) -> Result<AiAstRuleResponse> {
    let clean = strip_code_fences(raw);
    let mut resp: AiAstRuleResponse = serde_json::from_str(&clean)
        .map_err(|e| LogsError::other(format!("parse AI AST rule JSON: {e}")))?;
    resp.yaml = resp.yaml.trim().to_string();
    resp.id = resp.id.trim().to_string();
    resp.language = normalize_ast_language(&resp.language);
    resp.sample_code = resp.sample_code.trim().to_string();
    Ok(resp)
}

/// Logs-local structural validation — pins
/// the `-logs` contract without canonicalization (the canonicalized form is
/// produced by the synthesizer's unified validation).
pub fn structural_validate_ast_rule(
    yaml_text: &str,
    preferred_id: &str,
    language: &str,
) -> Result<String> {
    if re_dangerous_yaml().is_match(yaml_text) {
        return Err(LogsError::other("rejected dangerous yaml tokens"));
    }
    let mut id = preferred_id.to_string();
    if let Some(m) = re_yaml_id().captures(yaml_text) {
        id = m.get(1).map(|c| c.as_str()).unwrap_or("").to_string();
    }
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(LogsError::other("missing rule id"));
    }
    let id = if id.starts_with("ai-") {
        id
    } else {
        format!("ai-{id}")
    };
    if !id.ends_with("-logs") {
        return Err(LogsError::other(format!(
            "rule id {id:?} must end with -logs"
        )));
    }
    let mut lang_in_yaml = language.to_string();
    if let Some(m) = re_yaml_lang().captures(yaml_text) {
        lang_in_yaml = normalize_ast_language(m.get(1).map(|c| c.as_str()).unwrap_or(""));
    }
    if lang_in_yaml.is_empty() {
        return Err(LogsError::other("missing language"));
    }
    if !re_has_format().is_match(yaml_text) {
        return Err(LogsError::other("rule must bind $FORMAT metavar"));
    }
    if !re_has_pattern().is_match(yaml_text) {
        return Err(LogsError::other("rule must contain pattern entries"));
    }
    Ok(id)
}

// ---------------------------------------------------------------------------
// LocalRuleSynthesizer — ports the unified code-ingest synthesizer subset
//.
// ---------------------------------------------------------------------------

/// Canonicalizes a generated rule id.
fn canonicalize_generated_rule_id(id: &str) -> String {
    let mut id = id.trim().to_string();
    id = id.strip_prefix("ai-generated-").unwrap_or(&id).to_string();
    id = id.strip_prefix("ai-").unwrap_or(&id).to_string();
    for s in ["-logs", "-chunk", "-calls"] {
        if id.ends_with(s) {
            id = id.strip_suffix(s).unwrap_or(&id).to_string();
            break;
        }
    }
    let mut id = id.trim_matches(|c| c == '-' || c == '_').to_string();
    if id.is_empty() {
        id = "rule".to_string();
    }
    // Keep filename/id safe
    let re_safe = regex::Regex::new(r"[^A-Za-z0-9_-]+").expect("static regex");
    id = re_safe.replace_all(&id, "-").into_owned();
    format!("ai-generated-{id}-logs")
}

/// Unified structural validation for the logs kind
/// (`structuralValidateRule with `RuleKindLogs`).
fn structural_validate_rule_canonical(
    yaml_text: &str,
    preferred_id: &str,
    language: &str,
) -> Result<String> {
    if re_dangerous_yaml().is_match(yaml_text) {
        return Err(LogsError::other("rejected dangerous yaml tokens"));
    }
    let mut id = preferred_id.to_string();
    if let Some(m) = re_yaml_id().captures(yaml_text) {
        id = m.get(1).map(|c| c.as_str()).unwrap_or("").to_string();
    }
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(LogsError::other("missing rule id"));
    }
    let id = canonicalize_generated_rule_id(&id);
    let mut lang_in_yaml = language.to_string();
    if let Some(m) = re_yaml_lang().captures(yaml_text) {
        lang_in_yaml = normalize_ast_language(m.get(1).map(|c| c.as_str()).unwrap_or(""));
    }
    if lang_in_yaml.is_empty() {
        return Err(LogsError::other("missing language"));
    }
    if !re_has_format().is_match(yaml_text) {
        return Err(LogsError::other("logs rule must bind $FORMAT"));
    }
    if !re_has_pattern().is_match(yaml_text) {
        return Err(LogsError::other("logs rule must contain pattern entries"));
    }
    Ok(id)
}

/// Locates a directory containing `sgconfig.yml`:
/// IBIS_SG_ROOT env → exe dir → exe parent → cwd walked up 8 parents.
pub fn find_sg_config_dir() -> String {
    if let Ok(env) = std::env::var("IBIS_SG_ROOT") {
        let env = env.trim().to_string();
        if !env.is_empty() && has_sg_config(&env) {
            return env;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let dir = dir.display().to_string();
            if has_sg_config(&dir) {
                return dir;
            }
            // Common layout: testrun/ibis.exe with rules at repo root.
            if let Some(parent) = std::path::Path::new(&dir).parent() {
                let parent = parent.display().to_string();
                if has_sg_config(&parent) {
                    return parent;
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd;
        for _ in 0..8 {
            if has_sg_config(&dir.display().to_string()) {
                return dir.display().to_string();
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => break,
            }
        }
    }
    String::new()
}

fn has_sg_config(dir: &str) -> bool {
    if dir.is_empty() {
        return false;
    }
    std::path::Path::new(dir).join("sgconfig.yml").exists()
}

/// Resolves the ast-grep (sg) executable for the current config root
///: config dir, config testrun, exe dir, cwd + parents
/// (incl. testrun), then PATH.
pub fn resolve_sg_binary(config_dir: &str) -> String {
    let bin_name = if cfg!(windows) { "sg.exe" } else { "sg" };
    let mut candidates: Vec<String> = Vec::new();
    if !config_dir.is_empty() {
        candidates.push(
            std::path::Path::new(config_dir)
                .join(bin_name)
                .display()
                .to_string(),
        );
        candidates.push(
            std::path::Path::new(config_dir)
                .join("testrun")
                .join(bin_name)
                .display()
                .to_string(),
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(bin_name).display().to_string());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(bin_name).display().to_string());
        candidates.push(cwd.join("testrun").join(bin_name).display().to_string());
        let mut dir = cwd;
        for _ in 0..6 {
            candidates.push(dir.join("testrun").join(bin_name).display().to_string());
            candidates.push(dir.join(bin_name).display().to_string());
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => break,
            }
        }
    }
    candidates.push(bin_name.to_string());
    for c in &candidates {
        if c.is_empty() {
            continue;
        }
        if std::path::Path::new(c).exists() {
            return c.clone();
        }
    }
    bin_name.to_string()
}

/// Copies `sgconfig.yml` + `rules/` next to the sg binary (and executable dir)
/// so LogAlign extractors are found when the cwd is a harness directory
/// (`EnsureAstGrepRules → best-effort port).
pub fn ensure_ast_grep_rules() {
    let mut src_root = find_sg_config_dir();
    if src_root.is_empty() {
        // Last resort: cwd parents containing rules/go-logs.yml.
        if let Ok(cwd) = std::env::current_dir() {
            let mut dir = cwd;
            for _ in 0..8 {
                if dir.join("rules").join("go-logs.yml").exists() {
                    src_root = dir.display().to_string();
                    break;
                }
                match dir.parent() {
                    Some(parent) => dir = parent.to_path_buf(),
                    None => break,
                }
            }
        }
    }
    if src_root.is_empty() {
        tracing::debug!("EnsureAstGrepRules: no rules source found");
        return;
    }

    let mut targets: Vec<String> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            targets.push(dir.display().to_string());
        }
    }
    let bin = resolve_sg_binary(&src_root);
    if let Some(parent) = std::path::Path::new(&bin).parent() {
        targets.push(parent.display().to_string());
    }
    for dest in targets {
        if dest.is_empty() || dest == src_root {
            continue;
        }
        if let Err(e) = sync_sg_assets(&src_root, &dest) {
            tracing::warn!("EnsureAstGrepRules: sync to {dest} failed: {e}");
        } else {
            tracing::info!("EnsureAstGrepRules: synced rules/sgconfig → {dest}");
        }
    }
}

fn sync_sg_assets(src_root: &str, dest_dir: &str) -> std::io::Result<()> {
    std::fs::copy(
        std::path::Path::new(src_root).join("sgconfig.yml"),
        std::path::Path::new(dest_dir).join("sgconfig.yml"),
    )?;
    copy_dir_recursive(
        &std::path::Path::new(src_root).join("rules"),
        &std::path::Path::new(dest_dir).join("rules"),
    )
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Default in-crate synthesizer (the `code.PersistValidatedRule` port for the
/// `logs` kind, including the sg sandbox validation).
pub struct LocalRuleSynthesizer {
    /// Per-call sg scan timeout (the original had none; bounded for task hygiene).
    pub sg_timeout: Duration,
}

impl Default for LocalRuleSynthesizer {
    fn default() -> Self {
        LocalRuleSynthesizer {
            sg_timeout: Duration::from_secs(60),
        }
    }
}

#[async_trait]
impl RuleSynthesizer for LocalRuleSynthesizer {
    async fn persist_validated_rule(
        &self,
        yaml: &str,
        preferred_id: &str,
        language: &str,
        sample_code: &str,
    ) -> RulePersistOutcome {
        let yaml_text = yaml.trim().to_string();
        if yaml_text.is_empty() {
            return RulePersistOutcome::Rejected(LogsError::other("empty yaml"));
        }
        if yaml_text.len() > MAX_AST_RULE_YAML_BYTES {
            return RulePersistOutcome::Rejected(LogsError::other(format!(
                "yaml too large ({})",
                yaml_text.len()
            )));
        }
        let language = normalize_ast_language(language);

        let rule_id = match structural_validate_rule_canonical(&yaml_text, preferred_id, &language)
        {
            Ok(id) => id,
            Err(err) => {
                let (q_path, q_err) = quarantine_rule(&yaml_text, preferred_id, &err.to_string());
                let result = ASTRuleGenResult {
                    rule_id: preferred_id.to_string(),
                    language: language.clone(),
                    quarantine: q_path,
                    ..Default::default()
                };
                let error = match q_err {
                    Some(q) => LogsError::other(format!(
                        "structural validation failed: {} (quarantine: {q})",
                        err
                    )),
                    None => LogsError::other(format!("structural validation failed: {err}")),
                };
                return RulePersistOutcome::Quarantined { result, error };
            }
        };

        if let Err(err) = self
            .validate_with_sg(&yaml_text, &rule_id, &language, sample_code)
            .await
        {
            let (q_path, q_err) = quarantine_rule(&yaml_text, &rule_id, &err.to_string());
            let result = ASTRuleGenResult {
                rule_id: rule_id.clone(),
                language: language.clone(),
                quarantine: q_path,
                ..Default::default()
            };
            let error = match q_err {
                Some(q) => {
                    LogsError::other(format!("sg validation failed: {} (quarantine: {q})", err))
                }
                None => LogsError::other(format!("sg validation failed: {err}")),
            };
            return RulePersistOutcome::Quarantined { result, error };
        }

        match write_rule_file(&yaml_text, &rule_id, &language) {
            Ok(path) => {
                ensure_ast_grep_rules();
                RulePersistOutcome::Written(ASTRuleGenResult {
                    path,
                    rule_id,
                    language,
                    validated: true,
                    quarantine: String::new(),
                })
            }
            Err(err) => RulePersistOutcome::Quarantined {
                result: ASTRuleGenResult {
                    rule_id,
                    language,
                    ..Default::default()
                },
                error: LogsError::from(err),
            },
        }
    }

    fn ensure_rules(&self) {
        ensure_ast_grep_rules();
    }
}

impl LocalRuleSynthesizer {
    /// Dry-runs the rule with ast-grep in a sandbox temp tree
    /// (`validateRuleWithSG in ).
    /// When the sg binary is missing and `IBIS_AST_RULE_STRUCTURAL_ONLY=1`,
    /// validation is skipped (structural-only mode).
    async fn validate_with_sg(
        &self,
        yaml_text: &str,
        rule_id: &str,
        language: &str,
        sample_code: &str,
    ) -> Result<()> {
        let mut sg_bin = resolve_sg_binary(&find_sg_config_dir());
        if !std::path::Path::new(&sg_bin).exists() {
            ensure_ast_grep_rules();
            sg_bin = resolve_sg_binary(&find_sg_config_dir());
            if !std::path::Path::new(&sg_bin).exists() {
                if structural_only_mode() {
                    return Ok(());
                }
                return Err(LogsError::other("sg binary missing"));
            }
        }

        let tmp_root = tempfile::Builder::new()
            .prefix("ibis-sg-rule-")
            .tempdir()
            .map_err(LogsError::Io)?;
        let rules_dir = tmp_root.path().join("rules");
        std::fs::create_dir_all(&rules_dir)?;
        let cfg = "ruleDirs:\n  - rules\nlanguageGlobs:\n  html:\n    - \"*.vue\"\n    - \"*.jsp\"\n    - \"*.erb\"\n    - \"*.svelte\"\n";
        let cfg_path = tmp_root.path().join("sgconfig.yml");
        std::fs::write(&cfg_path, cfg)?;
        // Normalize id/language in the YAML document to the canonical values.
        let doc = ensure_yaml_id(yaml_text, rule_id, language);
        let rule_path = rules_dir.join(format!("{rule_id}.yml"));
        std::fs::write(&rule_path, &doc)?;

        let ext = language_ext(language);
        let sample = if sample_code.trim().is_empty() {
            minimal_smoke_snippet(language).to_string()
        } else {
            sample_code.to_string()
        };
        let sample_path = tmp_root.path().join(format!("sample{ext}"));
        std::fs::write(&sample_path, &sample)?;

        let out = tokio::process::Command::new(&sg_bin)
            .args(["scan", "--json=stream", "--config"])
            .arg(&cfg_path)
            .arg(&sample_path)
            .current_dir(tmp_root.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await;
        let out = match out {
            Ok(out) => out,
            Err(e) => return Err(LogsError::Io(e)),
        };
        // D-A08: capture stdout and stderr separately, concatenate
        // stdout-then-stderr so diagnostics never interleave into JSON.
        let mut out_str = String::from_utf8_lossy(&out.stdout).into_owned();
        out_str.push_str(&String::from_utf8_lossy(&out.stderr));

        if looks_like_sg_error(&out_str) {
            let mut preview = out_str.clone();
            if preview.len() > 400 {
                preview = preview.chars().take(400).collect();
            }
            return Err(LogsError::other(format!("sg rule error: {preview}")));
        }

        // If sample_code was provided, require at least one JSON match line.
        if !sample_code.trim().is_empty() {
            let matched = out_str.lines().any(|line| {
                let line = line.trim();
                line.starts_with('{') && (line.contains(rule_id) || line.contains("\"ruleId\""))
            });
            if !matched {
                return Err(LogsError::other(
                    "sg produced no matches for provided sample_code",
                ));
            }
        }
        Ok(())
    }
}

/// `IBIS_AST_RULE_STRUCTURAL_ONLY=1` skips sg dry-runs (harness escape hatch).
pub fn structural_only_mode() -> bool {
    std::env::var("IBIS_AST_RULE_STRUCTURAL_ONLY")
        .ok()
        .as_deref()
        == Some("1")
}

fn re_yaml_id() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(?m)^id:\s*['"]?([A-Za-z0-9_-]+)['"]?\s*$"#).expect("static regex")
    })
}

fn re_yaml_lang() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(?m)^language:\s*['"]?([A-Za-z0-9_+-]+)['"]?\s*$"#)
            .expect("static regex")
    })
}

fn re_has_format() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\$FORMAT\b").expect("static regex"))
}

fn re_has_pattern() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?m)^\s*-?\s*pattern:").expect("static regex"))
}

fn re_dangerous_yaml() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(!!|<<\s*\*|file://|/etc/passwd)").expect("static regex")
    })
}

fn re_digits() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\d+").expect("static regex"))
}

/// Rewrites (or inserts) `id:`/`language:` lines in a rule document
///
pub fn ensure_yaml_id(yaml_text: &str, rule_id: &str, language: &str) -> String {
    let mut doc = yaml_text.to_string();
    if re_yaml_id().is_match(&doc) {
        doc = re_yaml_id()
            .replace_all(&doc, format!("id: {rule_id}"))
            .into_owned();
    } else {
        doc = format!("id: {rule_id}\n{doc}");
    }
    if !language.is_empty() {
        if re_yaml_lang().is_match(&doc) {
            doc = re_yaml_lang()
                .replace_all(&doc, format!("language: {language}"))
                .into_owned();
        } else {
            doc = format!("language: {language}\n{doc}");
        }
    }
    doc
}

/// Writes the validated rule beside the sg config (and mirrors into
/// `testrun/rules` when present).
pub fn write_rule_file(
    yaml_text: &str,
    rule_id: &str,
    language: &str,
) -> std::result::Result<String, std::io::Error> {
    let root = find_sg_config_dir();
    if root.is_empty() {
        return Err(std::io::Error::other("sgconfig root not found"));
    }
    let rules_dir = std::path::Path::new(&root).join("rules");
    std::fs::create_dir_all(&rules_dir)?;
    let doc = ensure_yaml_id(yaml_text, rule_id, language);
    let path = rules_dir.join(format!("{rule_id}.yml"));
    std::fs::write(&path, format!("{doc}\n"))?;
    // Also mirror into testrun/rules when present (harness often uses that tree).
    let testrun_rules = std::path::Path::new(&root).join("testrun").join("rules");
    if testrun_rules.is_dir() {
        let _ = std::fs::write(
            testrun_rules.join(format!("{rule_id}.yml")),
            format!("{doc}\n"),
        );
    }
    Ok(path.display().to_string())
}

/// Moves an invalid rule outside ruleDirs. Returns the
/// quarantine path and a secondary error when the quarantine write itself
/// failed.
pub fn quarantine_rule(
    yaml_text: &str,
    rule_id: &str,
    reason: &str,
) -> (String, Option<LogsError>) {
    let mut root = find_sg_config_dir();
    if root.is_empty() {
        match std::env::current_dir() {
            Ok(cwd) => root = cwd.display().to_string(),
            Err(e) => return (String::new(), Some(LogsError::Io(e))),
        }
    }
    let dir = std::path::Path::new(&root).join(AST_RULE_QUARANTINE_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (String::new(), Some(LogsError::Io(e)));
    }
    let rule_id = if rule_id.is_empty() {
        "ai-unknown-logs"
    } else {
        rule_id
    };
    let name = format!(
        "{}_{}.yml",
        rule_id,
        chrono::Local::now().format("%Y%m%dT%H%M%S")
    );
    let path = dir.join(name);
    let body = format!(
        "# QUARANTINED: {}\n# reason: {}\n{}\n",
        chrono::Utc::now().to_rfc3339(),
        sanitize_reason(reason),
        yaml_text
    );
    if let Err(e) = std::fs::write(&path, body) {
        return (String::new(), Some(LogsError::Io(e)));
    }
    tracing::warn!(
        "AST rule gen: quarantined {} ({})",
        path.display(),
        sanitize_reason(reason)
    );
    (path.display().to_string(), None)
}

fn sanitize_reason(s: &str) -> String {
    let s = s.replace('\n', " ");
    let mut cut = 200.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// Majority vote of in-repo language hints from classified files
///
pub fn infer_language_from_errors(errors: &[SemanticError]) -> String {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in errors {
        let lang = language_from_path(&e.file);
        if !lang.is_empty() {
            *counts.entry(lang).or_insert(0) += 1;
        }
    }
    let mut best = String::new();
    let mut best_n = 0usize;
    for (lang, n) in counts {
        if n > best_n {
            best = lang;
            best_n = n;
        }
    }
    best
}

fn language_from_path(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "go" => "go",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        "java" => "java",
        "cs" => "csharp",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        _ => "",
    }
    .to_string()
}

/// Normalizes AI language tokens.
pub fn normalize_ast_language(lang: &str) -> String {
    let lang = lang.trim().to_lowercase();
    match lang.as_str() {
        "js" => "javascript".to_string(),
        "ts" => "typescript".to_string(),
        "c#" | "c-sharp" => "csharp".to_string(),
        "c++" => "cpp".to_string(),
        _ => lang,
    }
}

/// File extension used for the sg sandbox sample.
pub fn language_ext(lang: &str) -> String {
    match normalize_ast_language(lang).as_str() {
        "go" => ".go",
        "javascript" => ".js",
        "typescript" => ".ts",
        "python" => ".py",
        "java" => ".java",
        "csharp" => ".cs",
        "cpp" => ".cpp",
        _ => ".txt",
    }
    .to_string()
}

/// Minimal snippets guaranteeing at least one sg match when no sample_code was
/// provided.
pub fn minimal_smoke_snippet(lang: &str) -> &'static str {
    match normalize_ast_language(lang).as_str() {
        "go" => {
            "package p\nfunc f(l interface{ Infow(string, ...interface{}) }) {\n\tl.Infow(\"smoke format %s\", \"x\")\n}\n"
        }
        "javascript" | "typescript" => "logger.infow(\"smoke format %s\", \"x\");\n",
        "python" => "logger.info(\"smoke format %s\", \"x\")\n",
        _ => "// smoke\n",
    }
}

/// Deduplicated first-line samples from stack traces/templates
///
pub fn collect_sample_lines(errors: &[SemanticError], max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(max);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    'outer: for e in errors {
        for cand in [&e.stack_trace, &e.template] {
            let mut cand = cand.trim().to_string();
            if cand.is_empty() || seen.contains(&cand) {
                continue;
            }
            seen.insert(cand.clone());
            // Keep first line only for templates that span blocks.
            if let Some(i) = cand.find('\n') {
                cand = cand[..i].to_string();
            }
            out.push(cand);
            if out.len() >= max {
                break 'outer;
            }
        }
    }
    out
}

/// Fingerprint for the cooldown registry: language + digit-masked samples
///
pub fn ast_rule_fingerprint(lang: &str, errors: &[SemanticError]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(lang.as_bytes());
    for line in collect_sample_lines(errors, 6) {
        let cleaned = re_digits().replace_all(&line, "#");
        h.update(cleaned.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Heuristic sg failure detector: prefers explicit
/// failure markers and avoids matching JSON rule payloads.
pub fn looks_like_sg_error(out: &str) -> bool {
    let lower = out.to_lowercase();
    let hard = [
        "failed to parse",
        "invalid rule",
        "cannot parse rule",
        "error while loading rule",
        "serde_yaml",
        "syntax error in rule",
    ];
    if hard.iter().any(|n| lower.contains(n)) {
        return true;
    }
    // stderr-only error without any JSON match objects
    lower.contains("error:") && !out.contains("\"ruleId\"") && !out.contains("\"ruleid\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// Env/cwd-global tests (IBIS_SG_ROOT, IBIS_AST_RULE_STRUCTURAL_ONLY) are
    /// process-global — all lanes serialize them behind one mutex.
    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap()
    }

    fn set_env(key: &str, value: &str) {
        std::env::set_var(key, value);
    }

    fn remove_env(key: &str) {
        std::env::remove_var(key);
    }

    /// `TestParseAIASTRuleJSON.
    #[test]
    fn parse_ai_ast_rule_json_strips_fences() {
        let raw = "```json\n{\"language\":\"go\",\"id\":\"ai-go-zap-logs\",\"yaml\":\"id: ai-go-zap-logs\\nlanguage: go\\nrule:\\n  any:\\n    - pattern: $L.Infow($FORMAT, $$$A)\\n\",\"sample_code\":\"x\",\"rationale\":\"zap\"}\n```";
        let resp = parse_ai_ast_rule_json(raw).expect("parse");
        assert_eq!(resp.language, "go");
        assert_eq!(resp.id, "ai-go-zap-logs");
        assert!(resp.yaml.contains("$FORMAT"), "yaml missing FORMAT");
    }

    /// `TestStructuralValidateASTRule.
    #[test]
    fn structural_validate_ast_rule_pins() {
        let ok_yaml = "id: ai-go-zap-logs\nlanguage: go\nrule:\n  any:\n    - pattern: $LOGGER.Infow($FORMAT, $$$ARGS)\n    - pattern: $LOGGER.Errorw($FORMAT, $$$ARGS)\n";
        let id = structural_validate_ast_rule(ok_yaml, "", "go").expect("valid");
        assert_eq!(id, "ai-go-zap-logs");

        let missing_format =
            "id: bad\nlanguage: go\nrule:\n  any:\n    - pattern: $LOGGER.Info($MSG)\n";
        assert!(
            structural_validate_ast_rule(missing_format, "bad", "go").is_err(),
            "expected missing $FORMAT error"
        );

        let missing_suffix =
            "id: ai-go-zap\nlanguage: go\nrule:\n  any:\n    - pattern: $LOGGER.Infow($FORMAT)\n";
        assert!(
            structural_validate_ast_rule(missing_suffix, "ai-go-zap", "go").is_err(),
            "expected -logs suffix error"
        );
    }

    /// `TestInferLanguageFromErrors.
    #[test]
    fn infer_language_from_errors_majority_vote() {
        let errors = [
            SemanticError {
                file: "internal/foo.go".into(),
                ..Default::default()
            },
            SemanticError {
                file: "pkg/bar.go".into(),
                ..Default::default()
            },
            SemanticError {
                file: "web/app.ts".into(),
                ..Default::default()
            },
        ];
        assert_eq!(infer_language_from_errors(&errors), "go");
        assert_eq!(
            infer_language_from_errors(&[
                SemanticError::default(),
                SemanticError {
                    template: "x".into(),
                    ..Default::default()
                },
            ]),
            "",
            "expected empty language without source paths"
        );
    }

    /// `TestShouldAttemptASTRuleGenOnce.
    #[test]
    fn should_attempt_ast_rule_gen_once() {
        let analyzer = LogAnalyzer::bare("t");
        analyzer
            .ast_rule_gen_used
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let errors = [SemanticError {
            file: "x.go".into(),
            template: "err".into(),
            ..Default::default()
        }];
        assert!(
            !analyzer.should_attempt_ast_rule_gen(&errors),
            "expected false when already used"
        );
    }

    /// `TestPersistValidatedASTRuleStructuralOnly + `TestQuarantineOnBadRule`
    /// — env-gated, serialized behind the global env mutex (the guard
    /// intentionally spans the awaits).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn persist_validated_rule_structural_only_and_quarantine() {
        let _guard = env_guard();

        // --- TestPersistValidatedASTRuleStructuralOnly ---
        let root = tempfile::tempdir().expect("temp root");
        std::fs::write(root.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").unwrap();
        std::fs::create_dir_all(root.path().join("rules")).unwrap();
        let root_str = root.path().display().to_string();
        set_env("IBIS_SG_ROOT", &root_str);
        set_env("IBIS_AST_RULE_STRUCTURAL_ONLY", "1");

        let yaml = "id: ai-go-zap-logs\nlanguage: go\nrule:\n  any:\n    - pattern: $LOGGER.Infow($FORMAT, $$$ARGS)\n    - pattern: $LOGGER.Errorw($FORMAT, $$$ARGS)\n";
        let synth = LocalRuleSynthesizer::default();
        let outcome = synth
            .persist_validated_rule(yaml, "ai-go-zap-logs", "go", "")
            .await;
        let res = match outcome {
            RulePersistOutcome::Written(res) => res,
            other => panic!("PersistValidatedASTRule: expected Written, got {other:?}"),
        };
        assert!(
            res.validated && !res.path.is_empty(),
            "unexpected result: {res:?}"
        );
        assert!(
            res.path.contains("ai-generated-") || res.path.contains("ai-go-zap"),
            "path={}",
            res.path
        );
        let written = std::fs::read_to_string(&res.path).expect("read rule");
        assert!(
            written.contains("$FORMAT"),
            "written rule missing FORMAT: {written}"
        );

        // --- TestQuarantineOnBadRule ---
        let outcome = synth
            .persist_validated_rule("id: nope\nlanguage: go\n", "nope", "go", "")
            .await;
        let (result, error) = match outcome {
            RulePersistOutcome::Quarantined { result, error } => (result, error),
            other => panic!("expected quarantine, got {other:?}"),
        };
        assert!(
            !result.quarantine.is_empty(),
            "expected quarantine path, got {result:?}"
        );
        assert!(
            std::path::Path::new(&result.quarantine).exists(),
            "quarantine missing: {error}"
        );

        remove_env("IBIS_SG_ROOT");
        remove_env("IBIS_AST_RULE_STRUCTURAL_ONLY");
    }
}
