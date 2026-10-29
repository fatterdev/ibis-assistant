use std::collections::HashMap;

use async_trait::async_trait;

use crate::anchor::repo_join;
use crate::error::GitError;
use crate::runner::GitRunner;

/// Commits claim files into the project repository (the claim-store addendum:
/// the only write path into `.threski/claims/`). `writes` maps repo-relative
/// paths to file contents; `deletes` lists repo-relative paths to remove.
/// The commit contains exactly those paths — unrelated dirty files in the
/// working tree are never swept in. Returns the resulting HEAD OID.
/// (`search.GitClaimWriter.)
#[async_trait]
pub trait GitClaimWriter: Send + Sync {
    async fn commit_claim(
        &self,
        repo_path: &str,
        message: &str,
        writes: &HashMap<String, Vec<u8>>,
        deletes: &[String],
    ) -> Result<String, GitError>;
}

/// Production [`GitClaimWriter`] backed by git commands (
/// `search.GitCommitter`): `add -A -- <paths>`, path-scoped
/// `commit -m <msg> -- <paths>`, `reset -q -- <paths>` on commit failure.
#[derive(Clone)]
pub struct GitCommitter {
    runner: GitRunner,
}

impl GitCommitter {
    pub fn new(runner: GitRunner) -> Self {
        Self { runner }
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    /// Configures the local commit identity for claim commits
    /// (`git config user.name/user.email`). Owned clones get an identity at
    /// provisioning; live personal trees are never touched.
    pub async fn set_commit_identity(
        &self,
        repo_path: &str,
        name: &str,
        email: &str,
    ) -> Result<(), GitError> {
        self.runner
            .run(repo_path, &["config", "user.name", name])
            .await?;
        self.runner
            .run(repo_path, &["config", "user.email", email])
            .await?;
        Ok(())
    }
}

#[async_trait]
impl GitClaimWriter for GitCommitter {
    async fn commit_claim(
        &self,
        repo_path: &str,
        message: &str,
        writes: &HashMap<String, Vec<u8>>,
        deletes: &[String],
    ) -> Result<String, GitError> {
        let mut paths: Vec<String> = writes.keys().cloned().collect();
        paths.extend(deletes.iter().cloned());
        paths.sort();

        for path in deletes {
            let abs = repo_join(repo_path, path);
            match tokio::fs::remove_file(&abs).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(GitError::other(format!("remove claim file {path}: {e}"))),
            }
        }
        for (path, data) in writes {
            let abs = repo_join(repo_path, path);
            if let Some(parent) = abs.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| GitError::other(format!("create claim dir for {path}: {e}")))?;
            }
            tokio::fs::write(&abs, data)
                .await
                .map_err(|e| GitError::other(format!("write claim file {path}: {e}")))?;
        }

        if paths.is_empty() {
            let out = self.runner.run(repo_path, &["rev-parse", "HEAD"]).await?;
            return Ok(String::from_utf8_lossy(&out).trim().to_string());
        }

        let add_args: Vec<&str> = ["add", "-A", "--"]
            .into_iter()
            .chain(paths.iter().map(String::as_str))
            .collect();
        self.runner.run(repo_path, &add_args).await?;

        let commit_args: Vec<&str> = ["commit", "-m", message, "--"]
            .into_iter()
            .chain(paths.iter().map(String::as_str))
            .collect();
        if let Err(e) = self.runner.run(repo_path, &commit_args).await {
            // Unstage our paths so a failed commit leaves no staged residue.
            let reset_args: Vec<&str> = ["reset", "-q", "--"]
                .into_iter()
                .chain(paths.iter().map(String::as_str))
                .collect();
            let _ = self.runner.run(repo_path, &reset_args).await;
            return Err(e);
        }

        let out = self.runner.run(repo_path, &["rev-parse", "HEAD"]).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }
}
