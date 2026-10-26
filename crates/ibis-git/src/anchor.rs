use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::error::GitError;
use crate::runner::{parse_lines, GitRunner};

/// Joins a repo-relative slash path onto the repo root. Leading separators
/// are stripped first so an absolute-looking rel path cannot escape the repo
/// (`filepath.Join semantics differ from `Path::join` here).
pub(crate) fn repo_join(repo_path: &str, rel_path: &str) -> PathBuf {
    Path::new(repo_path).join(rel_path.trim_start_matches(['/', '\\']))
}

pub(crate) fn hash_bytes(data: &[u8]) -> String {
    let sum = Sha256::digest(data);
    let mut hex = String::with_capacity(sum.len() * 2);
    for byte in sum {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// A full 40-char lowercase commit OID.
pub fn is_full_commit_oid(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Supplies the git facts needed to bind memory claims to the commit DAG
/// (introduced_in / invalidated_at). Preserves the
/// `search.GitAnchorSource` interface ; production
/// uses git commands via [`GitAnchorRunner`], tests inject fakes.
///
/// `file_at_ref` returns raw bytes so consumers can do exact UTF-8
/// validation (phase-3f hot-ref cache contract).
#[async_trait]
pub trait GitAnchorSource: Send + Sync {
    async fn rev_parse_head(&self, repo_path: &str) -> Result<String, GitError>;
    async fn head_branch(&self, repo_path: &str) -> Result<String, GitError>;
    async fn work_file_hash(
        &self,
        repo_path: &str,
        rel_path: &str,
    ) -> Result<Option<String>, GitError>;
    async fn head_file_hash(
        &self,
        repo_path: &str,
        rel_path: &str,
    ) -> Result<Option<String>, GitError>;
    async fn is_clean(&self, repo_path: &str) -> Result<bool, GitError>;
    /// Batch-checks commit OIDs against the object database in a single
    /// process; returns the subset that is missing.
    async fn verify_commits(
        &self,
        repo_path: &str,
        oids: &[String],
    ) -> Result<HashSet<String>, GitError>;
    async fn list_ancestors(&self, repo_path: &str, oid: &str) -> Result<Vec<String>, GitError>;
    async fn resolve_ref(&self, repo_path: &str, reference: &str) -> Result<String, GitError>;
    async fn list_tree(&self, repo_path: &str, oid: &str) -> Result<Vec<String>, GitError>;
    async fn file_at_ref(
        &self,
        repo_path: &str,
        oid: &str,
        rel_path: &str,
    ) -> Result<Option<Vec<u8>>, GitError>;
    async fn file_history(&self, repo_path: &str, rel_path: &str) -> Result<Vec<String>, GitError>;
    async fn merge_base(&self, repo_path: &str, a: &str, b: &str) -> Result<String, GitError>;
    async fn is_shallow(&self, repo_path: &str) -> Result<bool, GitError>;
    async fn diff_paths(
        &self,
        repo_path: &str,
        from_oid: &str,
        to_oid: &str,
    ) -> Result<Vec<String>, GitError>;
}

/// Production [`GitAnchorSource`] backed by the credential-injecting git
/// runner.
#[derive(Clone)]
pub struct GitAnchorRunner {
    runner: GitRunner,
}

impl GitAnchorRunner {
    pub fn new(runner: GitRunner) -> Self {
        Self { runner }
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }
}

#[async_trait]
impl GitAnchorSource for GitAnchorRunner {
    async fn rev_parse_head(&self, repo_path: &str) -> Result<String, GitError> {
        let out = self.runner.run(repo_path, &["rev-parse", "HEAD"]).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    async fn head_branch(&self, repo_path: &str) -> Result<String, GitError> {
        let out = self
            .runner
            .run(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    async fn work_file_hash(
        &self,
        repo_path: &str,
        rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        let path = repo_join(repo_path, rel_path);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(hash_bytes(&data))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(GitError::other(format!(
                "hash working file {rel_path}: {e}"
            ))),
        }
    }

    async fn head_file_hash(
        &self,
        repo_path: &str,
        rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        let spec = format!("HEAD:{rel_path}");
        match self.runner.run(repo_path, &["show", &spec]).await {
            Ok(out) => Ok(Some(hash_bytes(&out))),
            // Path absent at HEAD is a normal outcome, not an error.
            Err(e) => {
                tracing::debug!(repo = repo_path, path = rel_path, error = %e, "head file hash: git show failed, treating as absent");
                Ok(None)
            }
        }
    }

    async fn is_clean(&self, repo_path: &str) -> Result<bool, GitError> {
        let out = self
            .runner
            .run(repo_path, &["status", "--porcelain"])
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().is_empty())
    }

    async fn verify_commits(
        &self,
        repo_path: &str,
        oids: &[String],
    ) -> Result<HashSet<String>, GitError> {
        let mut missing = HashSet::new();
        if oids.is_empty() {
            return Ok(missing);
        }
        let stdin: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let out = self
            .runner
            .run_with_stdin(repo_path, &stdin, &["cat-file", "--batch-check"])
            .await?;
        for line in out.split(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(line);
            let fields: Vec<&str> = text.split_whitespace().collect();
            if fields.len() >= 2 && fields[1] == "missing" {
                missing.insert(fields[0].to_string());
            }
        }
        Ok(missing)
    }

    async fn list_ancestors(&self, repo_path: &str, oid: &str) -> Result<Vec<String>, GitError> {
        let out = self.runner.run(repo_path, &["rev-list", oid]).await?;
        Ok(parse_lines(&out))
    }

    async fn resolve_ref(&self, repo_path: &str, reference: &str) -> Result<String, GitError> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(GitError::other("empty ref"));
        }
        if is_full_commit_oid(reference) {
            return Ok(reference.to_string());
        }
        let spec = format!("{reference}^{{commit}}");
        let out = self
            .runner
            .run(repo_path, &["rev-parse", "--verify", &spec])
            .await
            .map_err(|e| GitError::context(format!("resolve ref {reference}"), e))?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    async fn list_tree(&self, repo_path: &str, oid: &str) -> Result<Vec<String>, GitError> {
        let out = self
            .runner
            .run(repo_path, &["ls-tree", "-r", "--name-only", oid])
            .await?;
        Ok(parse_lines(&out))
    }

    async fn file_at_ref(
        &self,
        repo_path: &str,
        oid: &str,
        rel_path: &str,
    ) -> Result<Option<Vec<u8>>, GitError> {
        let spec = format!("{oid}:{rel_path}");
        match self.runner.run(repo_path, &["show", &spec]).await {
            Ok(out) => Ok(Some(out)),
            // Path absent at oid is a normal outcome, not an error.
            Err(e) => {
                tracing::debug!(repo = repo_path, path = rel_path, error = %e, "file at ref: git show failed, treating as absent");
                Ok(None)
            }
        }
    }

    async fn file_history(&self, repo_path: &str, rel_path: &str) -> Result<Vec<String>, GitError> {
        let out = self
            .runner
            .run(repo_path, &["log", "--format=%H", "--", rel_path])
            .await?;
        Ok(parse_lines(&out))
    }

    async fn merge_base(&self, repo_path: &str, a: &str, b: &str) -> Result<String, GitError> {
        let out = self.runner.run(repo_path, &["merge-base", a, b]).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    async fn is_shallow(&self, repo_path: &str) -> Result<bool, GitError> {
        let out = self
            .runner
            .run(repo_path, &["rev-parse", "--is-shallow-repository"])
            .await?;
        Ok(String::from_utf8_lossy(&out).trim() == "true")
    }

    async fn diff_paths(
        &self,
        repo_path: &str,
        from_oid: &str,
        to_oid: &str,
    ) -> Result<Vec<String>, GitError> {
        let out = self
            .runner
            .run(repo_path, &["diff", "--name-only", from_oid, to_oid])
            .await?;
        Ok(parse_lines(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_oid_detection() {
        assert!(is_full_commit_oid(&"a".repeat(40)));
        assert!(!is_full_commit_oid(&"a".repeat(39)));
        assert!(!is_full_commit_oid(&"A".repeat(40)));
        assert!(!is_full_commit_oid(&"g".repeat(40)));
        assert!(!is_full_commit_oid("HEAD"));
    }

    #[test]
    fn repo_join_strips_leading_separators() {
        let joined = repo_join("C:\\repo", ".ibis/claims/x.md");
        assert!(joined.starts_with(Path::new("C:\\repo")));
        let escaped = repo_join("C:\\repo", "/etc/passwd");
        assert!(escaped.starts_with(Path::new("C:\\repo")));
        assert!(escaped.to_string_lossy().contains("etc/passwd"));
    }
}
