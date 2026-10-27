//! ast-grep (`sg`) chunking.
//!
//! Invokes the managed `sg` binary (D2: external process on day 1) on content
//! written to a temp file with the original extension, parses the NDJSON
//! stream and dispatches matches by rule-ID suffix:
//! - `-chunk` → [`AstChunk`] (symbol guessed from the first line);
//! - `-calls` → [`CallEdge`] (callee = text before `(`);
//! - `-logs` → [`LogTemplate`] (must bind `$FORMAT`; printf verbs → regex).
//!
//! Config resolution order: `IBIS_SG_ROOT` env → exe
//! dir (and parent) → cwd walked up 8 parents; requires `sgconfig.yml`.
//! Binary probes: config dir, `testrun/`, exe dir, cwd parents, then the OS
//! `PATH` (F-A08: bare names skip the stat check, unlikewise where a
//! PATH-installed sg could never be found by stat).
//!
//! Parity note (F-A20): escaped metacharacters **before** substituting
//! printf verbs, so `%+v` (whose `+` had become `\+`) was never converted and
//! matched literal `%+v` text — such templates could never align real log
//! lines. The port substitutes verbs in a single pass over the original
//! format, escaping only the literal segments (the documented intent).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::CodeIngestError;

/// One structural chunk extracted by a `*-chunk` rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AstChunk {
    pub symbol_name: String,
    pub kind: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// One caller→callee edge extracted by a `*-calls` rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallEdge {
    pub caller_name: String,
    pub callee_name: String,
    pub start_line: u32,
}

/// One log statement template extracted by a `*-logs` rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogTemplate {
    pub format_string: String,
    pub regex: String,
    pub source_file: String,
    pub source_line: u32,
}

/// One NDJSON match emitted by `sg scan --json=stream`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SgMatch {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub range: SgRange,
    #[serde(default)]
    pub file: String,
    #[serde(rename = "ruleId", default)]
    pub rule_id: String,
    #[serde(rename = "metaVariables", default)]
    pub meta_variables: SgMetaVariables,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SgRange {
    #[serde(default)]
    pub start: SgPos,
    #[serde(default)]
    pub end: SgPos,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SgPos {
    #[serde(default)]
    pub line: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SgMetaVariables {
    #[serde(default)]
    pub single: HashMap<String, SgMetaVar>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SgMetaVar {
    #[serde(default)]
    pub text: String,
}

/// `filepath.Ext: suffix beginning at the final dot in the final path
/// element ("" when none; dot-files keep their leading dot as extension).
pub fn path_ext(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rfind('.') {
        Some(i) => &base[i..],
        None => "",
    }
}

/// Trims whitespace and wrapping quotes/backticks (`strings.Trim(f, "\"'")`
/// after a `TrimSpace`).
pub(crate) fn trim_quotes(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '"' || c == '`' || c == '\'')
}

/// Byte-count truncation that never splits a UTF-8 code point
/// ( sliced raw bytes, producing U+FFFD at marshal time; Rust would panic).
pub(crate) fn truncate_char_safe(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// printf-verb table, longest/most specific first so `%+v`/`%#v` win over
/// `%v` ( order preserved).
const VERBS: &[(&str, &str)] = &[
    ("%+v", "(.*?)"),
    ("%#v", "(.*?)"),
    ("%s", "(.*?)"),
    ("%d", r"([-+]?\d+)"),
    ("%v", "(.*?)"),
    ("%w", "(.*?)"),
    ("%f", r"([-+]?\d+(?:\.\d+)?)"),
    ("%t", "(true|false)"),
    ("%x", r"([0-9a-fA-F]+)"),
    ("%q", "(.*?)"),
];

fn escape_regex_char(c: char) -> String {
    match c {
        '\\' => "\\\\".to_string(),
        '.' => "\\.".to_string(),
        '+' => "\\+".to_string(),
        '*' => "\\*".to_string(),
        '?' => "\\?".to_string(),
        '(' => "\\(".to_string(),
        ')' => "\\)".to_string(),
        '[' => "\\[".to_string(),
        ']' => "\\]".to_string(),
        '{' => "\\{".to_string(),
        '}' => "\\}".to_string(),
        '^' => "\\^".to_string(),
        '$' => "\\$".to_string(),
        '|' => "\\|".to_string(),
        other => other.to_string(),
    }
}

/// Converts a printf-style format into an RE2-compatible substring pattern.
/// Anchors are intentionally omitted so runtime prefixes (timestamps, [INFO],
/// slog key=value tails) still match the static message body extracted from
/// source.
pub fn parse_format_to_regex(format: &str) -> String {
    let f = trim_quotes(format);
    if f.is_empty() {
        return String::new();
    }
    // Single pass: verbs are matched on the ORIGINAL text and replaced by
    // their token; every other character is escaped (F-A20).
    let mut out = String::with_capacity(f.len() + 16);
    let bytes = f.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some((verb, token)) = VERBS.iter().find(|(v, _)| f[i..].starts_with(v)) {
                out.push_str(token);
                i += verb.len();
                continue;
            }
        }
        let c = f[i..].chars().next().unwrap_or_default();
        out.push_str(&escape_regex_char(c));
        i += c.len_utf8();
    }
    out
}

/// Rejects formats/regexes that are too short or wildcard-only (e.g. a lone
/// `%w` → `(.*?)` would LogAlign-match every line).
pub fn is_useful_log_template(format: &str, regex: &str) -> bool {
    let f = trim_quotes(format);
    if f.len() < 12 {
        return false;
    }
    let r = regex.trim();
    if r.is_empty() || r == "(.*?)" || r == "(.*)" || r == ".+" || r == ".*" {
        return false;
    }
    // Strip common wildcard tokens; require remaining literal mass.
    let mut stripped = r.to_string();
    for tok in [
        "(.*?)",
        "(.*)",
        r"([-+]?\d+)",
        r"([-+]?\d+(?:\.\d+)?)",
        "(true|false)",
        r"([0-9a-fA-F]+)",
        "[^\n]*",
    ] {
        stripped = stripped.replace(tok, "");
    }
    stripped = stripped.replace('\\', "");
    let literal_runes = stripped
        .chars()
        .filter(|c| !matches!(c, '(' | ')' | '?' | '+' | '*' | '.' | '|'))
        .count();
    literal_runes >= 8
}

/// Resolved ast-grep environment: config root (+ binary). Test seam mirroring
/// lane B's `start_with_bin` pattern — production paths use [`SgLocator::discover`].
#[derive(Debug, Clone)]
pub struct SgLocator {
    pub config_dir: Option<PathBuf>,
    pub binary: PathBuf,
}

impl SgLocator {
    /// Env/exe/cwd discovery.
    pub fn discover() -> Self {
        let config_dir = find_sg_config_dir();
        let binary = resolve_sg_binary(config_dir.as_deref());
        SgLocator { config_dir, binary }
    }

    /// Explicit root (tests, seeded environments).
    pub fn with_root(root: &Path) -> Self {
        let binary = resolve_sg_binary(Some(root));
        SgLocator {
            config_dir: Some(root.to_path_buf()),
            binary,
        }
    }

    /// True when the resolved binary is a real file or may live on the PATH —
    /// either way it must pass an ast-grep `--version` probe. Guards the PATH
    /// fallback against lookalikes (Linux images ship shadow-utils' `sg`,
    /// which would otherwise be mistaken for the sandbox binary).
    pub fn binary_available(&self) -> bool {
        let resolvable = self.binary.is_file()
            || self
                .binary
                .parent()
                .map(|p| p.as_os_str().is_empty())
                .unwrap_or(true);
        resolvable && is_ast_grep_binary(&self.binary)
    }

    fn config_path(&self) -> Option<PathBuf> {
        self.config_dir.as_ref().map(|d| d.join("sgconfig.yml"))
    }
}

/// True when `bin` (a path or bare PATH name) responds to `--version` with
/// the ast-grep banner (`ast-grep 0.51.0`). Distinguishes the real ast-grep
/// CLI from shadow-utils' `sg` that Linux images ship on PATH.
fn is_ast_grep_binary(bin: &Path) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map(|out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .is_some_and(|field| field.eq_ignore_ascii_case("ast-grep"))
        })
        .unwrap_or(false)
}

/// Locates a directory containing `sgconfig.yml` (+ `rules/`)
/// (`SGConfigDir/`findSGConfigDir`).
pub fn sg_config_dir() -> Option<PathBuf> {
    find_sg_config_dir()
}

pub(crate) fn find_sg_config_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("IBIS_SG_ROOT") {
        let env = env.trim();
        if !env.is_empty() && has_sg_config(Path::new(env)) {
            return Some(PathBuf::from(env));
        }
    }
    // Common layout: testrun/ibis-assistant.exe with rules at repo root.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if has_sg_config(dir) {
                return Some(dir.to_path_buf());
            }
            if let Some(parent) = dir.parent() {
                if has_sg_config(parent) {
                    return Some(parent.to_path_buf());
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd;
        for _ in 0..8 {
            if has_sg_config(&dir) {
                return Some(dir);
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    None
}

pub fn has_sg_config(dir: &Path) -> bool {
    dir.join("sgconfig.yml").is_file()
}

/// Resolves the ast-grep (`sg`) executable for the current config root
/// (`SGBinaryPath/`resolveSGBinary`).
///
/// Candidates: config dir, `<config>/testrun`, exe dir, cwd, `<cwd>/testrun`,
/// six levels of cwd parents — then the OS `PATH` via a bare name (F-A08).
pub fn resolve_sg_binary(config_dir: Option<&Path>) -> PathBuf {
    let bin_name = if cfg!(windows) { "sg.exe" } else { "sg" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(cd) = config_dir {
        candidates.push(cd.join(bin_name));
        candidates.push(cd.join("testrun").join(bin_name));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(bin_name));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(bin_name));
        candidates.push(cwd.join("testrun").join(bin_name));
        // Walk a few parents for testrun/sg.exe (cargo test cwd is a package dir).
        let mut dir = cwd;
        for _ in 0..6 {
            candidates.push(dir.join("testrun").join(bin_name));
            candidates.push(dir.join(bin_name));
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    // Path-like candidates must exist on disk; bare names are resolved by the
    // OS PATH lookup ( stat'ed bare names, which always fail).
    for c in &candidates {
        let has_parent = c
            .parent()
            .map(|p| !p.as_os_str().is_empty())
            .unwrap_or(false);
        if has_parent && c.is_file() {
            return c.clone();
        }
    }
    PathBuf::from(bin_name)
}

/// Resolves the ast-grep binary for the current config root.
pub fn sg_binary_path() -> PathBuf {
    resolve_sg_binary(find_sg_config_dir().as_deref())
}

/// Parses `sg scan` NDJSON output into chunks/logs/calls
/// ( inline loop in `scanSGContent`; pure, unit-testable without sg).
///
/// Lines are 0-based in the stream → +1 (+`line_bias` for extracted regions).
/// Every line is parsed (F-A10: the 1MB scanner cap silently truncated the
/// stream on oversized lines).
pub fn parse_sg_output(
    out: &[u8],
    source_path: &str,
    line_bias: i64,
) -> (Vec<AstChunk>, Vec<LogTemplate>, Vec<CallEdge>) {
    let mut chunks = Vec::new();
    let mut logs = Vec::new();
    let mut calls = Vec::new();

    for line in out.split(|&b| b == b'\n') {
        if line.iter().all(|&b| b.is_ascii_whitespace()) {
            continue;
        }
        let Ok(m) = serde_json::from_slice::<SgMatch>(line) else {
            continue;
        };
        let start = (i64::from(m.range.start.line) + 1 + line_bias).max(0) as u32;
        let end = (i64::from(m.range.end.line) + 1 + line_bias).max(0) as u32;

        if m.rule_id.ends_with("-chunk") {
            chunks.push(AstChunk {
                symbol_name: guess_symbol_name(&m.text, &m.rule_id),
                kind: m.rule_id.clone(),
                content: m.text,
                start_line: start,
                end_line: end,
            });
        } else if m.rule_id.ends_with("-logs") {
            let format_str = m
                .meta_variables
                .single
                .get("FORMAT")
                .map(|v| v.text.clone())
                .unwrap_or_default();
            if trim_quotes(&format_str).is_empty() {
                continue;
            }
            let rx = parse_format_to_regex(&format_str);
            if !is_useful_log_template(&format_str, &rx) {
                continue;
            }
            logs.push(LogTemplate {
                format_string: format_str,
                regex: rx,
                source_file: source_path.to_string(),
                source_line: start,
            });
        } else if m.rule_id.ends_with("-calls") {
            let callee = m.text.split('(').next().unwrap_or("").to_string();
            calls.push(CallEdge {
                caller_name: "Unknown".to_string(),
                callee_name: callee,
                start_line: start,
            });
        }
    }
    (chunks, logs, calls)
}

/// Guesses a symbol name from the chunk's first line.
pub fn guess_symbol_name(text: &str, rule_id: &str) -> String {
    let _ = rule_id;
    let text = text.trim();
    if text.is_empty() {
        return "Snippet".to_string();
    }
    // method / function first line
    let mut first = text;
    if let Some(i) = text.find('\n') {
        first = &text[..i];
    }
    let first = first.trim();
    const PREFIXES: &[&str] = &[
        "public ",
        "private ",
        "protected ",
        "internal ",
        "static ",
        "async ",
        "void ",
        "function ",
        "func ",
    ];
    let mut stripped = first;
    for prefix in PREFIXES {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            stripped = rest;
        }
    }
    if let Some(i) = stripped.find('(') {
        if i > 0 {
            let name = stripped[..i].trim();
            let parts: Vec<&str> = name.split_whitespace().collect();
            if let Some(last) = parts.last() {
                return (*last).to_string();
            }
        }
    }
    if let Some(rest) = stripped.strip_prefix("class ") {
        let rest = rest.trim();
        if let Some(i) = rest.find([' ', '{']) {
            if i > 0 {
                return rest[..i].to_string();
            }
        }
        return rest.to_string();
    }
    "Snippet".to_string()
}

/// Runs ast-grep on `filePath`'s content.
///
/// Template-like extensions are ensured under `languageGlobs.html` first so
/// embedded script/style regions inject.
pub async fn parse_ast(
    file_path: &str,
    content: &[u8],
) -> Result<(Vec<AstChunk>, Vec<LogTemplate>, Vec<CallEdge>), CodeIngestError> {
    let mut ext = path_ext(file_path).to_string();
    if ext.is_empty() {
        ext = ".txt".to_string();
    }
    if crate::sparse_ast::is_html_host_ext(file_path) {
        let _ = crate::lang_globs::ensure_language_glob(&ext, "html");
    }
    let locator = SgLocator::discover();
    scan_sg_content(file_path, content, &ext, 0, &locator).await
}

/// [`parse_ast`] against an explicit locator (test seam).
pub async fn parse_ast_with(
    file_path: &str,
    content: &[u8],
    locator: &SgLocator,
) -> Result<(Vec<AstChunk>, Vec<LogTemplate>, Vec<CallEdge>), CodeIngestError> {
    let mut ext = path_ext(file_path).to_string();
    if ext.is_empty() {
        ext = ".txt".to_string();
    }
    scan_sg_content(file_path, content, &ext, 0, locator).await
}

/// Runs ast-grep on content written to a temp file with the given extension
///
///
/// `line_bias` is added to reported start/end lines (0 for host files;
/// `hostLine-1` for extracted regions — the extraction pipeline itself is
/// dead originally and omitted, F-A09).
///
/// Cancellation: the spawned sg process is killed when the future is dropped
/// (`kill_on_drop`), mirroring the `exec.CommandContext`.
pub(crate) async fn scan_sg_content(
    source_path: &str,
    content: &[u8],
    ext: &str,
    line_bias: i64,
    locator: &SgLocator,
) -> Result<(Vec<AstChunk>, Vec<LogTemplate>, Vec<CallEdge>), CodeIngestError> {
    let tmp = tempfile::Builder::new()
        .prefix("sg_ast_")
        .suffix(ext)
        .tempfile()?;
    std::fs::write(tmp.path(), content)?;

    let mut cmd = tokio::process::Command::new(&locator.binary);
    cmd.arg("scan").arg("--json=stream");
    if let Some(cfg) = locator.config_path() {
        cmd.arg("--config").arg(cfg);
    }
    cmd.arg(tmp.path());
    if let Some(dir) = &locator.config_dir {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = cmd.output().await?;

    // Non-zero exit tolerated when stdout has data (sg exits non-zero on
    // "no matches" for some configs).
    if !out.status.success() && out.stdout.is_empty() {
        let mut preview = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if preview.len() > 240 {
            let mut cut = 240;
            while !preview.is_char_boundary(cut) {
                cut -= 1;
            }
            preview.truncate(cut);
        }
        tracing::debug!(
            "ast-grep scan failed for {} (config={:?}): {} {}",
            source_path,
            locator.config_dir,
            out.status,
            preview
        );
    }

    Ok(parse_sg_output(&out.stdout, source_path, line_bias))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_ext_matches_filepath_ext() {
        assert_eq!(path_ext("main.go"), ".go");
        assert_eq!(path_ext("dir.v2/file"), "");
        assert_eq!(path_ext(".gitignore"), ".gitignore");
        assert_eq!(path_ext("a.blade.php"), ".php");
        assert_eq!(path_ext("C:\\dir\\file.cs"), ".cs");
        assert_eq!(path_ext("noext"), "");
        assert_eq!(path_ext(""), "");
    }

    #[test]
    fn parse_format_regex_flexible_anchors() {
        // Scenario: TestParseFormatToRegexFlexibleAnchors.
        let pat = parse_format_to_regex("\"Starting Ibis Assistant (Mode: %s)...\"");
        assert!(!pat.is_empty());
        assert!(!pat.starts_with('^') && !pat.ends_with('$'));
        let re = regex::Regex::new(&format!("(?s){pat}")).unwrap();
        let runtime =
            "2026/09/18 07:50:01 main.go:260: [INFO] Starting Ibis Assistant (Mode: sse)...";
        assert!(
            re.is_match(runtime),
            "expected substring match; pat={pat:?}"
        );
    }

    #[test]
    fn parse_format_regex_empty() {
        // Scenario: TestParseFormatToRegexEmpty.
        assert_eq!(parse_format_to_regex("\"\""), "");
    }

    #[test]
    fn parse_format_regex_verbs() {
        assert_eq!(
            parse_format_to_regex("%d/%f/%t/%x"),
            r"([-+]?\d+)/([-+]?\d+(?:\.\d+)?)/(true|false)/([0-9a-fA-F]+)"
        );
        // %+v is converted (F-A20) instead of matching literal "%+v".
        assert_eq!(parse_format_to_regex("x %+v y"), "x (.*?) y");
        assert_eq!(parse_format_to_regex("%#v"), "(.*?)");
        // the strings.ReplaceAll hits the "%s" inside "%%s" → "%(.*?)".
        assert_eq!(parse_format_to_regex("%%s"), "%(.*?)");
        // Lone % is a literal.
        assert_eq!(parse_format_to_regex("100%"), "100%");
    }

    #[test]
    fn useful_log_template_filter() {
        // ≥12 chars, ≥8 literal runes.
        assert!(is_useful_log_template(
            "\"connection refused to host db\"",
            &parse_format_to_regex("\"connection refused to host %s\"")
        ));
        // Too short.
        assert!(!is_useful_log_template("\"%s\"", "(.*?)"));
        // Wildcard-only regex.
        assert!(!is_useful_log_template("\"a very long message\"", "(.*?)"));
        assert!(!is_useful_log_template("\"a very long message\"", ".*"));
        // A lone %w → (.*?) is rejected.
        assert!(!is_useful_log_template("\"failed: %w\"", "(.*?)"));
    }

    #[test]
    fn guess_symbol_name_matrix() {
        assert_eq!(guess_symbol_name("", ""), "Snippet");
        assert_eq!(guess_symbol_name("func main() {", "go-chunk"), "main");
        assert_eq!(
            guess_symbol_name("public static void Run(int a)", ""),
            "Run"
        );
        assert_eq!(guess_symbol_name("class Foo {", ""), "Foo");
        assert_eq!(guess_symbol_name("class Foo", ""), "Foo");
        assert_eq!(guess_symbol_name("  async fetch(data) {", ""), "fetch");
        // Scenario: quirk: the `(` index must be > 0, so receiver-style method lines
        // (which start with `(` after prefix stripping) are not recognized.
        assert_eq!(
            guess_symbol_name("func (s *Server) Start() error {", ""),
            "Snippet"
        );
        assert_eq!(guess_symbol_name("const x = 1", ""), "Snippet");
    }

    #[test]
    fn parse_sg_output_dispatches_by_rule_suffix() {
        let ndjson = concat!(
            r#"{"text":"func main() {","range":{"start":{"line":9},"end":{"line":12}},"ruleId":"go-chunk"}"#,
            "\n",
            r#"{"text":"log.Printf","range":{"start":{"line":3},"end":{}},"ruleId":"go-logs","metaVariables":{"single":{"FORMAT":{"text":"\"starting server on %s\""}}}}"#,
            "\n",
            r#"{"text":"startServer(1)","range":{"start":{"line":4},"end":{}},"ruleId":"go-calls"}"#,
            "\nnot json\n",
            r#"{"text":"x","range":{"start":{"line":0},"end":{}},"ruleId":"other"}"#,
            "\n",
        );
        let (chunks, logs, calls) = parse_sg_output(ndjson.as_bytes(), "main.go", 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "go-chunk");
        assert_eq!(chunks[0].start_line, 10);
        assert_eq!(chunks[0].end_line, 13);
        assert_eq!(chunks[0].symbol_name, "main");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].source_line, 4);
        assert_eq!(logs[0].format_string, "\"starting server on %s\"");
        assert!(logs[0].regex.contains("(.*?)"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].callee_name, "startServer");
        assert_eq!(calls[0].start_line, 5);
        // The unparsable/uninteresting lines produced nothing.
        assert_eq!(chunks[0].content, "func main() {");
    }

    #[test]
    fn parse_sg_output_line_bias_and_short_logs() {
        // line_bias shifts reported lines; too-short FORMAT is filtered.
        let ndjson = concat!(
            r#"{"text":"x","range":{"start":{"line":0},"end":{"line":2}},"ruleId":"r-chunk"}"#,
            "\n",
            r#"{"text":"log","range":{"start":{"line":0},"end":{}},"ruleId":"x-logs","metaVariables":{"single":{"FORMAT":{"text":"\"%s\""}}}}"#,
            "\n",
        );
        let (chunks, logs, calls) = parse_sg_output(ndjson.as_bytes(), "f.go", 5);
        assert_eq!(chunks[0].start_line, 6);
        assert_eq!(chunks[0].end_line, 8);
        assert!(logs.is_empty(), "short format must be filtered");
        assert!(calls.is_empty());
    }
}
