//! Sparse-AST detection and host-language suggestion
//!
//!
//! Decides when the ingest should ask the AI for new ast-grep rules
//! (universal: any non-trivial file with empty/weak structural extraction)
//! and which sg language / `languageGlobs` host an extension maps to.

use crate::ast_chunker::{AstChunk, CallEdge};

/// Built-in extensions that tree-sitter/ast-grep usually parse without
/// languageGlobs.
pub fn known_sg_language(ext: &str) -> Option<&'static str> {
    match ext {
        ".go" => Some("go"),
        ".py" => Some("python"),
        ".js" | ".mjs" | ".cjs" | ".jsx" => Some("javascript"),
        ".ts" | ".tsx" => Some("typescript"),
        ".java" => Some("java"),
        ".cs" => Some("csharp"),
        ".cpp" | ".cc" | ".cxx" | ".hpp" => Some("cpp"),
        ".c" | ".h" => Some("c"),
        ".html" | ".htm" => Some("html"),
        ".css" | ".scss" => Some("css"),
        ".json" => Some("json"),
        ".yaml" | ".yml" => Some("yaml"),
        ".rs" => Some("rust"),
        ".rb" => Some("ruby"),
        ".php" => Some("php"),
        ".kt" => Some("kotlin"),
        ".swift" => Some("swift"),
        ".sql" => Some("sql"),
        _ => None,
    }
}

/// Template-ish hosts that need `languageGlobs` → html so embedded
/// script/style regions inject. Bootstrap hint list; the AI may extend it.
pub const DEFAULT_HTML_GLOB_EXTS: &[&str] = &[
    ".vue",
    ".svelte",
    ".astro",
    ".jsp",
    ".jspx",
    ".erb",
    ".ejs",
    ".hbs",
    ".mustache",
    ".njk",
    ".liquid",
    ".twig",
    ".blade.php",
];

/// Reports whether ingest should ask the AI for new ast-grep rules
/// Universal: any non-trivial file with empty
/// extraction.
pub fn needs_rule_synthesis(
    rel_path: &str,
    chunks: &[AstChunk],
    calls: &[CallEdge],
    content: &[u8],
) -> bool {
    if content.len() < 40 {
        return false;
    }
    if !chunks.is_empty() || !calls.is_empty() {
        return false;
    }
    let ext = crate::ast_chunker::path_ext(rel_path).to_lowercase();
    if ext.is_empty() {
        return true; // extension-less legacy / DSL
    }
    // Always synthesize when empty for unknown or template-like extensions.
    if known_sg_language(&ext).is_none() {
        return true;
    }
    let lower = rel_path.to_lowercase();
    for h in DEFAULT_HTML_GLOB_EXTS {
        if ext == *h || lower.ends_with(h) {
            return true;
        }
    }
    // Known language but zero structure on a sizable file → weak coverage.
    content.len() >= 200
}

/// Proposes an initial sg language / languageGlob host for an extension
///
pub fn suggest_host_language(rel_path: &str) -> String {
    let ext = crate::ast_chunker::path_ext(rel_path).to_lowercase();
    if let Some(lang) = known_sg_language(&ext) {
        return lang.to_string();
    }
    let lower = rel_path.to_lowercase();
    for h in DEFAULT_HTML_GLOB_EXTS {
        if ext == *h || lower.ends_with(h) {
            return "html".to_string();
        }
    }
    String::new()
}

/// Reports whether we should ensure a languageGlob html mapping for this path
///
pub fn is_html_host_ext(rel_path: &str) -> bool {
    let ext = crate::ast_chunker::path_ext(rel_path).to_lowercase();
    suggest_host_language(rel_path) == "html" && known_sg_language(&ext) != Some("html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_rule_synthesis_universal() {
        // Scenario: TestNeedsRuleSynthesisUniversal.
        assert!(needs_rule_synthesis(
            "pages/x.jsp",
            &[],
            &[],
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(needs_rule_synthesis(
            "App.vue",
            &[],
            &[],
            b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ));
        assert!(needs_rule_synthesis(
            "legacy.jsp",
            &[],
            &[],
            b"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        ));
        assert!(needs_rule_synthesis(
            "weird.dsl",
            &[],
            &[],
            b"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
        ));
        assert!(!needs_rule_synthesis(
            "main.go",
            &[AstChunk {
                symbol_name: String::new(),
                kind: String::new(),
                content: "func".into(),
                start_line: 1,
                end_line: 1,
            }],
            &[],
            b"package main"
        ));
    }

    #[test]
    fn needs_rule_synthesis_short_content_and_known_lang() {
        // < 40 bytes never synthesizes; empty extraction on a known language
        // under 200 bytes does not either.
        assert!(!needs_rule_synthesis("weird.dsl", &[], &[], b"short"));
        assert!(!needs_rule_synthesis("main.go", &[], &[], &[b'a'; 150]));
        // Known language with zero structure on a sizable file → weak coverage.
        assert!(needs_rule_synthesis("main.go", &[], &[], &[b'a'; 250]));
    }

    #[test]
    fn suggest_host_language_matrix() {
        // Scenario: TestSuggestHostLanguage.
        assert_eq!(suggest_host_language("a.vue"), "html");
        assert_eq!(suggest_host_language("a.go"), "go");
        assert_eq!(suggest_host_language("x.mustache"), "html");
        assert_eq!(suggest_host_language("unknown.zzz"), "");
        // Parity quirk: ".php" is a known sg language, so the compound
        // ".blade.php" extension resolves to php (synthesis is still
        // triggered via the NeedsRuleSynthesis suffix loop).
        assert_eq!(suggest_host_language("page.blade.php"), "php");
        assert!(needs_rule_synthesis(
            "page.blade.php",
            &[],
            &[],
            &[b'a'; 80]
        ));
    }

    #[test]
    fn html_host_ext_excludes_native_html() {
        assert!(is_html_host_ext("a.vue"));
        assert!(is_html_host_ext("pages/x.jsp"));
        assert!(!is_html_host_ext("a.html")); // native html host, no glob needed
        assert!(!is_html_host_ext("a.go"));
    }
}
