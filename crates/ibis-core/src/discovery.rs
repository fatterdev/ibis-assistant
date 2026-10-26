//! Repo discovery scan of the discovery root, gitignore-aware
//!

use std::path::{Path, PathBuf};

/// Handles simplified `.gitignore` rules for scan noise filtering
/// (name or simple glob; directory-specific patterns supported).
#[derive(Debug, Clone, Default)]
pub struct IgnoreMatcher {
    patterns: Vec<String>,
}

impl IgnoreMatcher {
    /// Creates a matcher from a .gitignore file. Missing file is an error,
    /// mirroring the original constructor.
    pub fn new(path: &str) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Ok(Self::from_content(&contents))
    }

    pub fn from_content(contents: &str) -> Self {
        let mut patterns = Vec::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            patterns.push(line.to_string());
        }
        Self { patterns }
    }

    /// Checks if a path matches any ignore pattern.
    pub fn matches(&self, path: &str, is_dir: bool) -> bool {
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        for p in &self.patterns {
            let only_dir = p.ends_with('/');
            let clean_p = p.trim_end_matches('/');
            if only_dir && !is_dir {
                continue;
            }
            if clean_p == name {
                return true;
            }
            if glob_match(clean_p, &name) {
                return true;
            }
        }
        false
    }
}

/// filepath.Match-style glob (`*`, `?`, `[class]`) against a single path element.
fn glob_match(pattern: &str, name: &str) -> bool {
    let mut re = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => re.push_str("[^/\\\\]*"),
            '?' => re.push_str("[^/\\\\]"),
            '[' => re.push('['),
            ']' => re.push_str("]?"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re)
        .map(|r| r.is_match(name))
        .unwrap_or(false)
}

/// Walks the directory tree to find Git repositories.
#[derive(Debug, Clone)]
pub struct Scanner {
    pub root: String,
    pub excludes: Vec<String>,
}

impl Scanner {
    /// Creates a scanner with the default safety excludes.
    pub fn new(root: &str) -> Self {
        Scanner {
            root: root.to_string(),
            excludes: vec![
                "node_modules",
                "vendor",
                // We look FOR .git, but we don't scan INSIDE .git for other .git
                ".git",
                "dist",
                "build",
                "bin",
                "obj",
                ".vs",
                ".idea",
                ".vscode",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }

    pub fn scan(&self) -> std::io::Result<Vec<String>> {
        let mut repos = Vec::new();
        let root = self.root.clone();

        // Root itself a git repo?
        if is_git_repo(Path::new(&root)) {
            repos.push(root.clone());
            if let Ok(subs) = scan_submodules(Path::new(&root)) {
                repos.extend(subs);
            }
            return Ok(repos);
        }

        let root_matcher =
            IgnoreMatcher::new(&Path::new(&root).join(".gitignore").display().to_string()).ok();

        let mut out: Vec<String> = Vec::new();
        collect_repos(
            Path::new(&root),
            &self.excludes,
            root_matcher.as_ref(),
            &mut out,
        )?;
        repos.extend(out);
        Ok(repos)
    }
}

fn collect_repos(
    dir: &Path,
    excludes: &[String],
    root_matcher: Option<&IgnoreMatcher>,
    out: &mut Vec<String>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Walk errors are skipped in the original implementation.
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();

        if excludes.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
            continue;
        }
        if let Some(m) = root_matcher {
            if m.matches(&name, true) {
                continue;
            }
        }
        if is_git_repo(&path) {
            out.push(path.display().to_string());
            if let Ok(subs) = scan_submodules(&path) {
                out.extend(subs);
            }
            continue; // Don't recurse inside a git repo to find other git repos
        }
        collect_repos(&path, excludes, root_matcher, out)?;
    }
    Ok(())
}

fn is_git_repo(path: &Path) -> bool {
    path.join(".git").try_exists().unwrap_or(false)
}

fn scan_submodules(repo_root: &Path) -> std::io::Result<Vec<String>> {
    let gitmodules = repo_root.join(".gitmodules");
    let contents = match std::fs::read_to_string(&gitmodules) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut submodules = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        // parity: any line containing "path =" contributes its RHS as a path.
        if line.contains("path =") {
            if let Some(eq) = line.find('=') {
                let rel = line[eq + 1..].trim();
                let abs = repo_root.join(rel);
                if abs.try_exists().unwrap_or(false) {
                    submodules.push(PathBuf::from(&abs).display().to_string());
                }
            }
        }
    }
    Ok(submodules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_matcher() {
        let matcher = IgnoreMatcher {
            patterns: vec!["node_modules".into(), "*.log".into(), "build/".into()],
        };
        assert!(matcher.matches("node_modules", true));
        assert!(!matcher.matches("src", true));
        assert!(matcher.matches("error.log", false));
        assert!(matcher.matches("build", true));
        assert!(!matcher.matches("build", false));
    }

    #[test]
    fn scan_finds_repos_and_respects_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        let repo1 = root.join("repo1");
        std::fs::create_dir_all(repo1.join(".git")).unwrap();
        std::fs::create_dir_all(repo1.join("node_modules")).unwrap();
        std::fs::create_dir_all(repo1.join("vendor")).unwrap();
        let not_repo = root.join("not_repo");
        std::fs::create_dir_all(&not_repo).unwrap();
        let repo2 = root.join("repo2");
        std::fs::create_dir_all(repo2.join(".git")).unwrap();

        std::fs::write(repo1.join(".gitignore"), "vendor\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored_repo\n").unwrap();
        let ignored = root.join("ignored_repo");
        std::fs::create_dir_all(ignored.join(".git")).unwrap();

        let repos = Scanner::new(root.to_str().unwrap()).scan().unwrap();
        assert!(
            repos
                .iter()
                .any(|r| r.as_str() == repo1.display().to_string()),
            "repo1: {repos:?}"
        );
        assert!(
            repos
                .iter()
                .any(|r| r.as_str() == repo2.display().to_string()),
            "repo2: {repos:?}"
        );
        assert!(
            !repos
                .iter()
                .any(|r| r.as_str() == ignored.display().to_string()),
            "ignored_repo leaked"
        );
    }

    #[test]
    fn scan_submodules_lists_existing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join(".gitmodules"),
            "[sub \"s\"]\n\tpath = libs/sub\n\tpath = missing\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("libs/sub")).unwrap();
        let subs = scan_submodules(root).unwrap();
        assert_eq!(subs.len(), 1);
        assert!(subs[0].replace('\\', "/").ends_with("libs/sub"));
    }
}
