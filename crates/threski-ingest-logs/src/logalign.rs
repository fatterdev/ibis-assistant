//! LogAlign — AST-derived log template alignment and AI→template promotion
//!
//!
//! Static templates (loaded from `log_template` rows in the constructor or
//! promoted from AI classifications) classify log lines deterministically and
//! bypass the AI stage. Promotion writes validated templates back to
//! `log_template` (`origin='ai'`) and appends them to the in-memory
//! staticPatterns for same-batch / next-chunk bypass.
//!
//! Regex safety: the regexp is RE2 (linear); the Rust `regex` crate shares
//! that property, so the dangerous-shape heuristics below remain a noise
//! filter rather than a backtracking guard — implemented verbatim for parity.

use sha2::{Digest, Sha256};

use threski_db::executor::vars;
use threski_db::ids::{format_record_id, record_id_to_plain};

use crate::analyzer::{compile_dotall, LogAnalyzer, LogTemplatePattern, SemanticError};
use crate::error::{LogsError, Result};

pub const MAX_LOG_ALIGN_REGEX_LEN: usize = 2048;
pub const MAX_LOG_ALIGN_FORMAT_LEN: usize = 4000;
pub const MAX_AI_PROMOTE_PER_BATCH: usize = 8;
pub const MIN_STATIC_LITERAL_RUNES: usize = 8;
pub const MIN_STATIC_FORMAT_LEN: usize = 12;

/// Rejects overly broad LogAlign rules (e.g. format "w" / regex "(.*?)") that
/// would match unrelated lines and skip classify AI wrongly
///
pub fn is_useful_static_pattern(format: &str, regex: &str) -> bool {
    let mut f = format.trim().to_string();
    f = f
        .trim_matches(|c| c == '"' || c == '`' || c == '\'')
        .to_string();
    if f.chars().count() < MIN_STATIC_FORMAT_LEN {
        return false;
    }
    let r = regex.trim();
    if r.is_empty() || r == "(.*?)" || r == "(.*)" || r == ".+" || r == ".*" {
        return false;
    }
    let mut stripped = r.to_string();
    for tok in [
        "(.*?)",
        "(.*)",
        r"([-+]?\d+)",
        r"([-+]?\d+(?:\.\d+)?)",
        "(true|false)",
        r"([0-9a-fA-F]+)",
        r"[^\n]*",
    ] {
        stripped = stripped.replace(tok, "");
    }
    stripped = stripped.replace('\\', "");
    let literal_runes = stripped
        .chars()
        .filter(|ch| !matches!(ch, '(' | ')' | '?' | '+' | '*' | '.' | '|'))
        .count();
    literal_runes >= MIN_STATIC_LITERAL_RUNES
}

/// Turns an AI `<VAR>` mask template into a RE2 substring pattern suitable for
/// Phase-1 LogAlign (no `^`/`$` anchors — runtime prefixes OK)
///
pub fn template_to_flexible_regex(template: &str) -> Result<String> {
    let template = template.trim();
    if template.is_empty() {
        return Err(LogsError::other("empty template"));
    }
    if template.chars().count() > MAX_LOG_ALIGN_FORMAT_LEN {
        return Err(LogsError::other("template too long"));
    }

    // Same construction as AddKnownPattern: trailing <VAR> consumes rest of line.
    let pattern = crate::analyzer::mask_template_to_regex(template);

    if pattern.len() > MAX_LOG_ALIGN_REGEX_LEN {
        return Err(LogsError::other("regex too long"));
    }
    if looks_dangerous_regex(&pattern) {
        return Err(LogsError::other("rejected dangerous regex shape"));
    }
    Ok(pattern)
}

/// Cheap dangerous-shape heuristics. Both the regexp and the Rust `regex`
/// crate are linear-time, so classic catastrophic backtracking is not an
/// issue; we still reject nested unbounded wildcards and extremely nested
/// groups that are useless/noisy.
pub fn looks_dangerous_regex(pattern: &str) -> bool {
    let lower = pattern.to_lowercase();
    let dangerous = ["(.*)*.*", "(.*?)*", "(.+)+", "(.*)+", "(a+)+", "(.*){,"];
    for d in dangerous {
        if lower.contains(d) {
            return true;
        }
    }
    // Scenario: counted "(" occurrences — parity (bytes, not runes).
    pattern.matches('(').count() > 40
}

/// Compiles a LogAlign regex with `(?s)` and length guards
///
pub fn safe_compile_log_align(pattern: &str) -> Result<regex::Regex> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err(LogsError::other("empty regex"));
    }
    if pattern.len() > MAX_LOG_ALIGN_REGEX_LEN {
        return Err(LogsError::other(format!(
            "regex too long ({})",
            pattern.len()
        )));
    }
    if looks_dangerous_regex(pattern) {
        return Err(LogsError::other("rejected dangerous regex shape"));
    }
    compile_dotall(pattern)
}

impl LogAnalyzer {
    /// Reports whether the line already matches a static pattern
    ///
    pub(crate) fn already_covered_by_static(&self, line: &str) -> bool {
        let statics = self.static_patterns.lock().unwrap();
        statics.iter().any(|sp| sp.regex.is_match(line))
    }

    /// Synthesizes durable log_template rows from AI classifications that AST
    /// extraction did not cover. Valid rules
    /// are appended to in-memory staticPatterns immediately (same-batch /
    /// next-chunk LogAlign bypass) and UPSERTed into SurrealDB. Returns the
    /// promotion count (capped at [`MAX_AI_PROMOTE_PER_BATCH`]).
    pub async fn promote_ai_errors_to_log_align(&self, errors: &[SemanticError]) -> usize {
        if errors.is_empty() {
            return 0;
        }

        let mut promoted = 0usize;
        for e in errors {
            if promoted >= MAX_AI_PROMOTE_PER_BATCH {
                break;
            }
            let tmpl = e.template.trim();
            if tmpl.is_empty() {
                continue;
            }
            // Prefer matching against stack/context line when available.
            let probe = if e.stack_trace.is_empty() {
                tmpl
            } else {
                e.stack_trace.as_str()
            };
            if self.already_covered_by_static(probe) || self.already_covered_by_static(tmpl) {
                continue;
            }

            let Ok(regex_str) = template_to_flexible_regex(tmpl) else {
                continue;
            };
            if !is_useful_static_pattern(tmpl, &regex_str) {
                continue;
            }
            let Ok(re) = safe_compile_log_align(&regex_str) else {
                continue;
            };
            // Sanity: rule should match the probe it was derived from (or the
            // template itself).
            if !re.is_match(probe) && !re.is_match(tmpl) {
                continue;
            }

            let src_file = crate::enrich::normalize_rel_path(&e.file);
            let src_line = e.line;
            let format_str = tmpl.to_string();

            let digest = Sha256::digest(format!("ai|{format_str}|{src_file}").as_bytes());
            let id_suffix = format!(
                "ai_{}",
                digest
                    .iter()
                    .take(12)
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let log_id = format_record_id("log_template", &id_suffix);

            let pat = LogTemplatePattern {
                id: record_id_to_plain(&threski_db::value::RecordId::new(
                    "log_template",
                    id_suffix.as_str(),
                )),
                regex: re,
                format_str: format_str.clone(),
                source_file: src_file.clone(),
                source_line: src_line,
            };
            self.static_patterns.lock().unwrap().push(pat);
            self.persist_log_template(
                &log_id,
                &format_str,
                &regex_str,
                &src_file,
                src_line,
                &e.category,
            )
            .await;
            promoted += 1;
        }

        if promoted > 0 {
            let statics = self.static_patterns.lock().unwrap().len();
            tracing::info!(
                "LogAlign: promoted {promoted} AI template(s) → log_template (staticPatterns={statics})"
            );
        }
        promoted
    }

    /// UPSERTs one promoted template. Bound
    /// variables (F-002); the record target is a sanitized identifier.
    async fn persist_log_template(
        &self,
        log_id: &str,
        format_str: &str,
        regex_str: &str,
        src_file: &str,
        src_line: i64,
        category: &str,
    ) {
        let Some(db) = &self.db else {
            return;
        };
        let sql = format!(
            "UPSERT {log_id} SET format_string = $format, regex = $regex, source_file = $src_file, source_line = $src_line, origin = 'ai', category = $category, updated_at = time::now();"
        );
        if let Err(e) = db
            .smart_query(
                &sql,
                vars([
                    ("format", threski_db::value::Value::from_t(format_str)),
                    ("regex", threski_db::value::Value::from_t(regex_str)),
                    ("src_file", threski_db::value::Value::from_t(src_file)),
                    ("src_line", threski_db::value::Value::from_t(src_line)),
                    ("category", threski_db::value::Value::from_t(category)),
                ]),
            )
            .await
        {
            tracing::warn!("LogAlign: failed to persist AI log_template {log_id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::LogAnalyzer;

    #[test]
    fn template_to_flexible_regex_matches_prefixed_line() {
        let tmpl = "2026-09-18 <VAR> ERROR Database timeout for user <VAR>";
        let pat = template_to_flexible_regex(tmpl).expect("flexible regex");
        let re = safe_compile_log_align(&pat).expect("compile");

        let line = "2026-09-18 07:50:01 ERROR Database timeout for user alice";
        assert!(
            re.is_match(line),
            "expected match; pat={pat:?} line={line:?}"
        );

        let tmpl2 = "[ERROR] Failed to connect to SurrealDB: <VAR>";
        let pat2 = template_to_flexible_regex(tmpl2).expect("flexible regex 2");
        let re2 = safe_compile_log_align(&pat2).expect("compile 2");
        let prefixed =
            "2026/09/18 07:50:01 main.go:315: [ERROR] Failed to connect to SurrealDB: dial timeout";
        assert!(
            re2.is_match(prefixed),
            "expected prefixed match; pat={pat2:?}"
        );
    }

    #[test]
    fn looks_dangerous_regex_pins() {
        assert!(looks_dangerous_regex("(.*)+foo"));
        assert!(!looks_dangerous_regex("Failed to connect: .*?"));
    }

    #[test]
    fn safe_compile_rejects_empty() {
        assert!(safe_compile_log_align("").is_err());
    }

    #[test]
    fn safe_compile_rejects_oversized() {
        let long = "a".repeat(MAX_LOG_ALIGN_REGEX_LEN + 1);
        assert!(safe_compile_log_align(&long).is_err());
    }

    #[test]
    fn is_useful_static_pattern_rejects_weak() {
        assert!(!is_useful_static_pattern("w", "(.*?)"));
        assert!(is_useful_static_pattern(
            "\"CRITICAL: Failed to connect to SurrealDB: %v\"",
            "CRITICAL: Failed to connect to SurrealDB: (.*?)"
        ));
    }

    #[test]
    fn promote_skips_when_already_covered() {
        let re = regex::Regex::new("(?s)Database timeout").unwrap();
        let analyzer = LogAnalyzer::bare("t");
        analyzer
            .static_patterns
            .lock()
            .unwrap()
            .push(LogTemplatePattern {
                id: "log_template:x".into(),
                regex: re,
                format_str: "Database timeout".into(),
                source_file: String::new(),
                source_line: 0,
            });
        let n = tokio::runtime::Runtime::new().unwrap().block_on(
            analyzer.promote_ai_errors_to_log_align(&[SemanticError {
                category: "DB_TIMEOUT".into(),
                template: "ERROR Database timeout for <VAR>".into(),
                stack_trace: "ERROR Database timeout for alice".into(),
                ..Default::default()
            }]),
        );
        assert_eq!(n, 0, "expected 0 promotions when already covered");
    }
}
