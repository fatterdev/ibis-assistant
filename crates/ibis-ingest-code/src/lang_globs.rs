//! `languageGlobs` maintenance in `sgconfig.yml`
//!
//!
//! Text-edits the YAML (no YAML library — the file is a controlled bootstrap
//! artifact) adding `ext` under `languageGlobs[language]` when missing. The
//! edit is mirrored to `testrun/` when present, and the synced copies beside
//! the sg binary / executable are refreshed via
//! [`crate::install::ensure_ast_grep_rules`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ast_chunker::find_sg_config_dir;
use crate::error::CodeIngestError;

static LANG_GLOB_LOCK: Mutex<()> = Mutex::new(());

/// Adds `ext` (e.g. `.jsp`) under `languageGlobs[language]` in sgconfig.yml
/// when missing. Extension-agnostic: used for any
/// unfamiliar host mapping the AI or the bootstrap hint list suggests.
///
/// Skips when the glob is already mapped under **any** language.
pub fn ensure_language_glob(ext: &str, language: &str) -> Result<(), CodeIngestError> {
    let ext = ext.trim().to_lowercase();
    let language = language.trim().to_lowercase();
    if ext.is_empty() || language.is_empty() {
        return Ok(());
    }
    let ext = if let Some(stripped) = ext.strip_prefix('.') {
        format!(".{stripped}")
    } else {
        format!(".{ext}")
    };
    let glob = format!("\"*{ext}\"");

    let _guard = LANG_GLOB_LOCK.lock();

    let Some(root) = find_sg_config_dir() else {
        return Err(CodeIngestError::other("sgconfig root not found"));
    };
    let cfg_path = root.join("sgconfig.yml");
    let text = std::fs::read_to_string(&cfg_path)?;

    // Already mapped (any language)?
    if text.contains(&format!("*{ext}")) {
        return Ok(());
    }

    let updated = upsert_language_glob(&text, &language, &glob);
    if updated == text {
        return Ok(());
    }
    std::fs::write(&cfg_path, updated.as_bytes())?;
    // Mirror to testrun when present.
    let testrun_cfg = root.join("testrun").join("sgconfig.yml");
    if testrun_cfg.parent().is_some_and(|p| p.is_dir()) {
        let _ = std::fs::write(&testrun_cfg, updated.as_bytes());
    }
    let _ = crate::install::ensure_ast_grep_rules();
    Ok(())
}

/// Pure text-edit core of [`ensure_language_glob`],
/// re-exported for unit tests.
///
/// -parity fixes (F-A14): the trailing blank line produced by
/// splitting a newline-terminated document no longer separates the appended
/// glob from its language block, and a glob for a language **missing** from
/// the section is inserted as a new language key instead of being mis-parented
/// under the last language block the end-of-input append landed in.
pub fn upsert_language_glob(text: &str, language: &str, quoted_glob: &str) -> String {
    // Ensure the languageGlobs section exists.
    if !text.contains("languageGlobs:") {
        let mut text = text.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "languageGlobs:\n  {language}:\n    - {quoted_glob}\n"
        ));
        return text;
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A newline-terminated document yields a final "" element; stash it and
    // re-append after the insertion so the glob lands inside its block.
    let trailing_newline = lines.last().is_some_and(|l| l.is_empty());
    if trailing_newline {
        lines.pop();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    let mut in_globs = false;
    let mut in_lang = false;
    let mut current_lang = String::new();
    let mut lang_indent = String::new();
    let mut inserted = false;
    for line in lines {
        let trim = line.trim();
        if trim == "languageGlobs:" {
            in_globs = true;
            in_lang = false;
            out.push(line.to_string());
            continue;
        }
        if in_globs {
            if !trim.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
                // Left the block.
                if !inserted && in_lang {
                    out.push(format!("{lang_indent}- {quoted_glob}"));
                    inserted = true;
                }
                in_globs = false;
                in_lang = false;
                out.push(line.to_string());
                continue;
            }
            if trim.starts_with(&format!("{language}:")) {
                in_lang = true;
                current_lang = language.to_string();
                lang_indent = format!("{}  ", indentation(line));
                out.push(line.to_string());
                continue;
            }
            if in_lang && trim.starts_with("- ") {
                out.push(line.to_string());
                continue;
            }
            if in_lang && !trim.is_empty() && trim.contains(':') && !trim.starts_with('-') {
                // Next language key.
                if !inserted {
                    out.push(format!("{lang_indent}- {quoted_glob}"));
                    inserted = true;
                }
                out.push(line.to_string());
                if let Some(key) = trim.strip_suffix(':') {
                    if !key.is_empty() {
                        in_lang = key == language;
                        current_lang = key.to_string();
                        if in_lang {
                            lang_indent = format!("{}  ", indentation(line));
                        }
                    }
                }
                continue;
            }
        }
        out.push(line.to_string());
    }
    if in_globs && !inserted {
        if in_lang && current_lang == language {
            out.push(format!("{lang_indent}- {quoted_glob}"));
        } else {
            // Language key missing entirely.
            out.push(format!("  {language}:"));
            out.push(format!("    - {quoted_glob}"));
        }
        inserted = true;
    }
    if trailing_newline {
        out.push(String::new());
    }
    let _ = inserted;
    out.join("\n")
}

fn indentation(line: &str) -> &str {
    let end = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    &line[..end]
}

/// Exposed for tests: applies the edit to an explicit file
/// (test seam over the global [`ensure_language_glob`]).
pub fn ensure_language_glob_in(
    root: &Path,
    ext: &str,
    language: &str,
) -> Result<(), CodeIngestError> {
    let ext = ext.trim().to_lowercase();
    let ext = if ext.starts_with('.') {
        ext
    } else {
        format!(".{ext}")
    };
    let glob = format!("\"*{ext}\"");
    let cfg_path: PathBuf = root.join("sgconfig.yml");
    let text = std::fs::read_to_string(&cfg_path)?;
    if text.contains(&format!("*{ext}")) {
        return Ok(());
    }
    let updated = upsert_language_glob(&text, language.trim().to_lowercase().as_str(), &glob);
    if updated == text {
        return Ok(());
    }
    std::fs::write(&cfg_path, updated.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_language_section() {
        let text = "ruleDirs:\n  - rules\n";
        let out = upsert_language_glob(text, "html", "\"*.jsp\"");
        assert!(out.contains("languageGlobs:\n  html:\n    - \"*.jsp\"\n"));
        assert!(out.starts_with("ruleDirs:\n  - rules\n"));
    }

    #[test]
    fn upsert_appends_glob_to_existing_language() {
        let text = "ruleDirs:\n  - rules\nlanguageGlobs:\n  html:\n    - \"*.vue\"\n";
        let out = upsert_language_glob(text, "html", "\"*.jsp\"");
        assert!(out.contains("    - \"*.vue\"\n"));
        assert!(out.contains("    - \"*.jsp\"\n"));
        // The insertion lands inside the html block.
        let vue_idx = out.find("*.vue").unwrap();
        let jsp_idx = out.find("*.jsp").unwrap();
        assert!(jsp_idx > vue_idx);
    }

    #[test]
    fn upsert_creates_new_language_key_inside_block() {
        let text = "ruleDirs:\n  - rules\nlanguageGlobs:\n  html:\n    - \"*.vue\"\n";
        let out = upsert_language_glob(text, "javascript", "\"*.jsx\"");
        assert!(out.contains("  javascript:\n    - \"*.jsx\""));
        assert!(out.contains("  html:\n    - \"*.vue\""));
    }

    #[test]
    fn upsert_handles_glob_before_next_language() {
        let text = "languageGlobs:\n  html:\n    - \"*.vue\"\n  css:\n    - \"*.styl\"\n";
        let out = upsert_language_glob(text, "html", "\"*.jsp\"");
        let vue = out.find("*.vue").unwrap();
        let jsp = out.find("*.jsp").unwrap();
        let css = out.find("css:").unwrap();
        assert!(
            jsp > vue && jsp < css,
            "glob inserted inside the html block: {out}"
        );
    }

    #[test]
    fn ensure_language_glob_in_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("sgconfig.yml"),
            "ruleDirs:\n  - rules\nlanguageGlobs:\n  html:\n    - \"*.vue\"\n",
        )
        .unwrap();
        ensure_language_glob_in(tmp.path(), ".jsp", "html").unwrap();
        ensure_language_glob_in(tmp.path(), ".jsp", "html").unwrap();
        let out = std::fs::read_to_string(tmp.path().join("sgconfig.yml")).unwrap();
        assert!(out.contains("*.jsp"));
        assert_eq!(out.matches("*.jsp").count(), 1);
        // Already-mapped glob under another language → untouched.
        ensure_language_glob_in(tmp.path(), ".vue", "javascript").unwrap();
        let out2 = std::fs::read_to_string(tmp.path().join("sgconfig.yml")).unwrap();
        assert_eq!(out, out2);
    }
}
