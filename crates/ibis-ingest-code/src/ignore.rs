//! Ignore-pattern loading and matching (
//! ).
//!
//! Reads `.gitignore` + `.aiignore` at the repo root (union, no negation
//! support) and matches candidate paths with `filepath.Match`-style globs.
//!
//! Parity note (F-A13): the `filepath.Match` uses the **OS separator**, so on
//! Windows `*` spans `/` in the slash-normalized relative path and patterns
//! like `src/*` overbroadly match nested paths. The Rust matcher treats
//! `/` as the separator on every platform (-Linux semantics) — an
//! intentional, recorded deviation (F-A13, D15).

use std::collections::HashSet;
use std::path::Path;

/// Reads `.gitignore` and then `.aiignore` from `repo_path`, skipping blank
/// lines and `#` comments. Missing files are skipped
/// silently; the union of both files is returned in read order.
pub fn load_ignore_patterns(repo_path: &Path) -> Vec<String> {
    let mut patterns = Vec::new();
    for name in [".gitignore", ".aiignore"] {
        let path = repo_path.join(name);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            patterns.push(line.to_string());
        }
    }
    patterns
}

/// Checks whether `path` (absolute) under `root` matches any pattern
///
///
/// Matching rules, in the original order:
/// 1. bare-name glob when the pattern has no `/` (unanchored, e.g. `*.log`,
///    `node_modules`);
/// 2. path glob — anchored patterns (leading `/`) match the rel path or a
///    directory prefix; unanchored patterns containing `/` are root-relative
///    (gitignore semantics);
/// 3. recursive directory match for unanchored `foo/` directory patterns.
pub fn is_ignored(path: &Path, root: &Path, patterns: &[String], is_dir: bool) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let Ok(rel_os) = path.strip_prefix(root) else {
        return false;
    };
    // Normalize to forward slashes for matching (git always uses forward
    // slashes).
    let rel = rel_os.to_string_lossy().replace('\\', "/");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    for p in patterns {
        let dir_only = p.ends_with('/');
        let clean_p = p.trim_end_matches('/');

        // 1. Name match (unanchored).
        if !clean_p.contains('/') && filepath_match(clean_p, &name) {
            if dir_only && !is_dir {
                continue; // pattern "foo/" doesn't match the file "foo"
            }
            return true;
        }

        // 2. Path match.
        if let Some(target) = clean_p.strip_prefix('/') {
            if filepath_match(target, &rel) {
                if dir_only && !is_dir {
                    continue;
                }
                return true;
            }
            // Prefix check for directory match.
            if rel.starts_with(&format!("{target}/")) {
                return true;
            }
        } else if clean_p.contains('/') {
            // gitignore: a pattern containing a slash is anchored to the
            // .gitignore location (repo root here).
            if filepath_match(clean_p, &rel) {
                if dir_only && !is_dir {
                    continue;
                }
                return true;
            }
            if rel.starts_with(&format!("{clean_p}/")) {
                return true;
            }
        }

        // 3. Recursive directory match for unanchored dirs.
        if dir_only && !p.starts_with('/') && !clean_p.contains('/') {
            let inside =
                rel.starts_with(&format!("{clean_p}/")) || rel.contains(&format!("/{clean_p}/"));
            if inside {
                return true;
            }
        }
    }
    false
}

/// `filepath.Match`-style glob against a single path element: `*` matches any
/// sequence of non-`/` characters, `?` a single non-`/` char, `[...]` a char
/// class. An unterminated `[` makes the pattern never match (the original callers
/// discard the `ErrBadPattern` error — same effective behavior as this crate's
/// regex compile failure, see lane-A finding LA-02).
pub fn filepath_match(pattern: &str, name: &str) -> bool {
    let mut re = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '\\' => {
                if let Some(next) = chars.next() {
                    re.push_str(&regex::escape(&next.to_string()));
                }
            }
            '[' => {
                // Copy the class verbatim; unterminated classes never match.
                let mut class = String::from("[");
                let mut closed = false;
                // Scenario: filepath.Match negation `[!abc]` → Rust regex `[^abc]`.
                if chars.peek() == Some(&'!') {
                    chars.next();
                    class.push('^');
                } else if chars.peek() == Some(&'^') {
                    if let Some(n) = chars.next() {
                        class.push(n);
                    }
                }
                // A `]` right after the (optional) negation is a literal.
                if chars.peek() == Some(&']') {
                    if let Some(n) = chars.next() {
                        class.push(n);
                    }
                }
                for n in chars.by_ref() {
                    if n == ']' {
                        closed = true;
                        break;
                    }
                    class.push(n);
                }
                if !closed {
                    return false;
                }
                class.push(']');
                // `regex::escape` would neuter the class; validate instead.
                if regex::Regex::new(&class).is_err() {
                    return false;
                }
                re.push_str(&class);
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    match regex::Regex::new(&re) {
        Ok(r) => r.is_match(name),
        Err(_) => false,
    }
}

/// Case-insensitive lookup set built from `config.IgnoredDirs` /
/// `config.IgnoredFiles` ( builds plain maps keyed by the config entry).
#[derive(Debug, Default, Clone)]
pub struct NameFilter {
    names: HashSet<String>,
}

impl NameFilter {
    pub fn from_entries(entries: &[String]) -> Self {
        NameFilter {
            names: entries.iter().cloned().collect(),
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn load_ignore_patterns_reads_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            "# comment\n*.log\n\nnode_modules/\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join(".aiignore"), "secret.go\n").unwrap();
        let patterns = load_ignore_patterns(tmp.path());
        assert_eq!(patterns, vec!["*.log", "node_modules/", "secret.go"]);
        // Missing files are skipped silently.
        let empty = tempfile::tempdir().unwrap();
        assert!(load_ignore_patterns(empty.path()).is_empty());
    }

    #[test]
    fn is_ignored_matrix() {
        let root = p("/repo");
        let patterns = vec![
            "*.log".to_string(),
            "node_modules/".to_string(),
            "/vendor".to_string(),
            "src/gen".to_string(),
            "build".to_string(),
        ];
        // 1. bare-name glob
        assert!(is_ignored(p("/repo/a.log"), root, &patterns, false));
        assert!(is_ignored(p("/repo/sub/b.log"), root, &patterns, false));
        // 2. dir-only pattern matches the dir and files inside
        assert!(is_ignored(p("/repo/node_modules"), root, &patterns, true));
        assert!(is_ignored(
            p("/repo/node_modules/lib.js"),
            root,
            &patterns,
            false
        ));
        // ... but not a file literally named node_modules
        assert!(!is_ignored(p("/repo/node_modules"), root, &patterns, false));
        // 3. anchored pattern
        assert!(is_ignored(p("/repo/vendor/x.go"), root, &patterns, false));
        assert!(!is_ignored(
            p("/repo/sub/vendor/x.go"),
            root,
            &patterns,
            false
        ));
        // 4. root-relative slash pattern
        assert!(is_ignored(p("/repo/src/gen/a.go"), root, &patterns, false));
        assert!(!is_ignored(
            p("/repo/other/src/gen/a.go"),
            root,
            &patterns,
            false
        ));
        // 5. plain bare-name patterns match the FILE NAME only (parity):
        // a nested file is not matched by "build" — the directory walk skips
        // the "build" dir itself (name match with is_dir=true) before
        // descending, and git-tracked files under it are ingested.
        assert!(!is_ignored(
            p("/repo/deep/build/out.txt"),
            root,
            &patterns,
            false
        ));
        assert!(is_ignored(p("/repo/deep/build"), root, &patterns, true));
        // empty patterns
        assert!(!is_ignored(p("/repo/x"), root, &[], false));
    }

    #[test]
    fn filepath_match_semantics() {
        // `*` does not cross `/` (-Linux separator semantics, F-A13).
        assert!(filepath_match("*.go", "main.go"));
        assert!(filepath_match("a*b", "axxb"));
        assert!(!filepath_match("*.go", "dir/main.go"));
        assert!(filepath_match("?.go", "a.go"));
        assert!(!filepath_match("?.go", "ab.go"));
        assert!(filepath_match("[abc].go", "b.go"));
        assert!(!filepath_match("[abc].go", "d.go"));
        assert!(filepath_match("[!abc].go", "d.go"));
        // Unterminated class: never matches ( ErrBadPattern discarded).
        assert!(!filepath_match("[abc", "abc"));
        // Escaped separators are literal.
        assert!(filepath_match("a\\*b", "a*b"));
        assert!(!filepath_match("a\\*b", "axxb"));
    }

    #[test]
    fn name_filter() {
        let f = NameFilter::from_entries(&["package-lock.json".to_string()]);
        assert!(f.contains("package-lock.json"));
        assert!(!f.contains("yarn.lock"));
    }
}
