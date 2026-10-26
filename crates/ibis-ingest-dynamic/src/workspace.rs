//! Workspace sync for ingest.
//!
//! Server-owned clones (under `discovery_root/dynamic/<name>`) may
//! clone/clean/reset/fetch/checkout; live personal/plugin trees are read in
//! place and never touched destructively.
//!
//! Credential resolution order: request-level token →
//! DB `git_credential` by URL then host → legacy `config.json` token. The
//! whole sequence is lane-D's [`ibis_git::resolve_credential`]; the host
//! parsing is `ibis_git::get_host` (finding D-A15) — nothing re-implemented
//! here.
//!
//! Deviation (E-A15): the original request-level token came from the
//! context-value `auth.GitTokenContextKey`; here it is an explicit
//! [`sync_workspace`] parameter, mirroring the lane-E redmine decision
//! (E-A13).

use std::path::Path;
use std::sync::Arc;

use ibis_core::config::Config;
use ibis_core::workspace;
use ibis_db::executor::Executor;
use ibis_git::store::CredentialStore;
use ibis_git::sync::{
    checkout, clean_untracked, clone_repo, ensure_remote_origin, fetch_origin, reset_hard,
};
use ibis_git::{is_auth_error, resolve_credential, GitError, GitRunner};

use crate::db_bridge::ExecutorCredentialDb;
use crate::error::DynamicError;

/// Sync outcome ( returns `(repoPath, actualCommit, isAligned)`).
#[derive(Debug, Clone, PartialEq)]
pub struct SyncOutcome {
    pub repo_path: String,
    pub actual_commit: String,
    pub is_aligned: bool,
}

/// Ensures the repo is ready for ingest.
pub async fn sync_workspace(
    cfg: &Config,
    db: Option<Arc<dyn Executor>>,
    request_token: Option<&str>,
    repo_name: &str,
    origin_url: &str,
    branch: &str,
    commit: &str,
) -> Result<SyncOutcome, DynamicError> {
    let resolved = workspace::resolve(cfg, repo_name)?;
    let repo_path = resolved.path.clone();
    tracing::info!(
        "Syncing workspace for {repo_name} at {} (owned={} Branch: {branch}, Commit: {commit})",
        repo_path,
        resolved.owned
    );

    if !resolved.owned {
        return sync_live_workspace(&repo_path).await;
    }

    let parent = Path::new(&repo_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .display()
        .to_string();
    std::fs::create_dir_all(&parent).map_err(DynamicError::DynamicReposDir)?;

    // 1. Resolve git credentials: request token → DB (URL then host) →
    // legacy config token.
    let store = db.map(|db| {
        CredentialStore::new(Some(
            Arc::new(ExecutorCredentialDb(db)) as Arc<dyn ibis_git::CredentialDb>
        ))
    });
    let legacy_token = cfg.get_git_token(origin_url);
    let cred = resolve_credential(
        request_token,
        store.as_ref(),
        origin_url,
        Some(legacy_token.as_str()),
    )
    .await;

    let runner = GitRunner::new(cred);

    // 2. Clone when missing; otherwise clean, reset, re-point origin and
    // fetch (`os.IsNotExist check; any other stat error follows the
    // existing-repo path).
    let missing =
        std::fs::metadata(&repo_path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
    if missing {
        tracing::info!("Cloning repository {repo_name} from {origin_url}");
        if let Err(err) =
            clone_repo(&runner, &parent, origin_url, &repo_path, opt_branch(branch)).await
        {
            return Err(auth_or(
                err,
                origin_url,
                "Git credentials required or authentication failed during clone",
                DynamicError::Clone,
            ));
        }
    } else {
        tracing::info!(
            "Repository {repo_name} already exists. Resetting and fetching from {origin_url}"
        );
        if let Err(err) = clean_untracked(&runner, &repo_path).await {
            tracing::warn!("git clean failed: {err}");
        }
        if let Err(err) = reset_hard(&runner, &repo_path).await {
            tracing::warn!("git reset failed: {err}");
        }
        let _ = ensure_remote_origin(&runner, &repo_path, origin_url).await;

        if let Err(err) = fetch_origin(&runner, &repo_path).await {
            return Err(auth_or(
                err,
                origin_url,
                "Git credentials required or authentication failed during fetch",
                DynamicError::Fetch,
            ));
        }
    }

    // 3. Checkout the requested commit/branch. A failed commit checkout falls
    // back to the branch and marks the workspace not aligned.
    let mut is_aligned = true;
    let target = if commit.is_empty() { branch } else { commit };

    if !target.is_empty() {
        tracing::info!("Checking out target: {target}");
        if let Err(err) = checkout(&runner, &repo_path, target).await {
            if !commit.is_empty() && !branch.is_empty() {
                tracing::warn!(
                    "Failed to checkout commit {commit}. Falling back to branch {branch}. Output: {}",
                    err.command_output().unwrap_or_default()
                );
                is_aligned = false;
                if let Err(fallback) = checkout(&runner, &repo_path, branch).await {
                    return Err(DynamicError::CheckoutFallback {
                        branch: branch.to_string(),
                        source: fallback,
                    });
                }
            } else {
                return Err(DynamicError::Checkout {
                    target: target.to_string(),
                    source: err,
                });
            }
        }
    }

    // 4. Determine the actual commit we ended up on.
    let mut actual_commit = String::new();
    match runner.run(&repo_path, &["rev-parse", "HEAD"]).await {
        Ok(out) => {
            actual_commit = String::from_utf8_lossy(&out).trim().to_string();
        }
        Err(err) => tracing::warn!("Failed to rev-parse HEAD: {err}"),
    }

    Ok(SyncOutcome {
        repo_path,
        actual_commit,
        is_aligned,
    })
}

/// Live personal/plugin tree: read-only HEAD probe, never clone/reset/fetch
///
async fn sync_live_workspace(repo_path: &str) -> Result<SyncOutcome, DynamicError> {
    match std::fs::metadata(repo_path) {
        Ok(st) if st.is_dir() => {}
        Ok(_) => {
            return Err(DynamicError::LiveWorkspace {
                path: repo_path.to_string(),
                source: std::io::Error::other("path is not a directory"),
            })
        }
        Err(err) => {
            return Err(DynamicError::LiveWorkspace {
                path: repo_path.to_string(),
                source: err,
            })
        }
    }

    let runner = GitRunner::anonymous();
    let out = runner
        .run(repo_path, &["rev-parse", "HEAD"])
        .await
        .map_err(|source| DynamicError::LiveRevParse {
            path: repo_path.to_string(),
            source,
        })?;
    let actual_commit = String::from_utf8_lossy(&out).trim().to_string();
    tracing::info!(
        "Using live working tree at {repo_path} (HEAD {actual_commit}); skipping clone/reset/fetch"
    );
    Ok(SyncOutcome {
        repo_path: repo_path.to_string(),
        actual_commit,
        is_aligned: true,
    })
}

fn opt_branch(branch: &str) -> Option<&str> {
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// Auth-failure output is answered with the `CredentialsRequired` shape
/// everything else keeps the wrapped git error.
fn auth_or(
    err: GitError,
    origin_url: &str,
    message: &str,
    wrap: fn(GitError) -> DynamicError,
) -> DynamicError {
    let output = err.command_output().unwrap_or_default().to_string();
    if is_auth_error(output.as_bytes()) {
        DynamicError::credentials_required(origin_url, message)
    } else {
        wrap(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn personal_cfg(discovery: &str, live: &str) -> Config {
        Config {
            runtime_mode: "personal".to_string(),
            discovery_root: discovery.to_string(),
            projects: vec![ibis_core::config::ProjectConfig {
                name: "demo".to_string(),
                working_repo_path: live.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    async fn run_git(dir: &str, args: &[&str]) {
        GitRunner::anonymous()
            .run(dir, args)
            .await
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
    }

    async fn git_out(dir: &str, args: &[&str]) -> String {
        String::from_utf8_lossy(
            &GitRunner::anonymous()
                .run(dir, args)
                .await
                .expect("git out"),
        )
        .trim()
        .to_string()
    }

    /// Creates a git repo with one commit.
    async fn init_commit_repo(name: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let path = path.display().to_string();
        std::fs::create_dir_all(&path).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            run_git(&path, &args).await;
        }
        std::fs::write(std::path::Path::new(&path).join("README.md"), "hi").unwrap();
        run_git(&path, &["add", "."]).await;
        run_git(&path, &["commit", "-q", "-m", "init"]).await;
        (dir, path)
    }

    // Scenario: TestSyncWorkspacePersonalSkipsDestructiveGit.
    #[tokio::test]
    async fn personal_mode_never_touches_the_live_tree() {
        let (_dir, live) = init_commit_repo("live").await;
        let dirty = std::path::Path::new(&live).join("dirty.txt");
        std::fs::write(&dirty, "uncommitted").unwrap();
        let discovery = tempfile::tempdir().unwrap();
        let cfg = personal_cfg(&discovery.path().display().to_string(), &live);

        let out = sync_workspace(&cfg, None, None, "demo", "", "", "")
            .await
            .expect("sync failed");
        assert!(out.is_aligned, "live tree should be treated as aligned");
        assert_eq!(out.repo_path, live);
        assert!(!out.actual_commit.is_empty(), "expected HEAD commit");

        assert_eq!(
            std::fs::read_to_string(&dirty).unwrap(),
            "uncommitted",
            "dirty file was removed"
        );
        assert!(
            !discovery.path().join("dynamic").join("demo").exists(),
            "personal mode must not clone under dynamic/"
        );
    }

    // Scenario: TestSyncWorkspaceMissingPersonalPath.
    #[tokio::test]
    async fn missing_personal_path_fails() {
        let missing = tempfile::tempdir()
            .unwrap()
            .path()
            .join("missing")
            .display()
            .to_string();
        let cfg = personal_cfg("", &missing);
        let err = sync_workspace(&cfg, None, None, "demo", "", "", "")
            .await
            .expect_err("expected error for missing live path");
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    // Scenario: TestSyncWorkspaceServerClonesUnderDynamic.
    #[tokio::test]
    async fn server_mode_clones_under_dynamic() {
        let (_src_dir, src) = init_commit_repo("src").await;
        let discovery = tempfile::tempdir().unwrap();
        let discovery_path = discovery.path().display().to_string();
        let cfg = Config {
            runtime_mode: "server".to_string(),
            discovery_root: discovery_path.clone(),
            ..Default::default()
        };

        let out = sync_workspace(&cfg, None, None, "demo", &src, "", "")
            .await
            .expect("sync failed");
        assert!(out.is_aligned, "clone should be aligned");
        let want = Path::new(&discovery_path)
            .join("dynamic")
            .join("demo")
            .display()
            .to_string();
        assert_eq!(out.repo_path, want);
        assert!(!out.actual_commit.is_empty(), "expected cloned HEAD");
        assert!(
            Path::new(&want).join(".git").exists(),
            "expected clone at {want}"
        );
    }

    #[tokio::test]
    async fn server_mode_missing_origin_fails_loudly() {
        let discovery = tempfile::tempdir().unwrap();
        let cfg = Config {
            runtime_mode: "server".to_string(),
            discovery_root: discovery.path().display().to_string(),
            ..Default::default()
        };
        let err = sync_workspace(&cfg, None, None, "demo", "/nonexistent/remote", "", "")
            .await
            .expect_err("clone of missing remote must fail");
        assert!(err.to_string().contains("git clone failed"), "got: {err}");
    }

    #[tokio::test]
    async fn checkout_commit_falls_back_to_branch_not_aligned() {
        let (_src_dir, src) = init_commit_repo("fallback").await;
        // Second commit so the branch head differs from its first commit.
        std::fs::write(Path::new(&src).join("two.txt"), "2").unwrap();
        run_git(&src, &["add", "."]).await;
        run_git(&src, &["commit", "-q", "-m", "second"]).await;
        let branch = git_out(&src, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        assert!(!branch.is_empty());

        let discovery = tempfile::tempdir().unwrap();
        let cfg = Config {
            runtime_mode: "server".to_string(),
            discovery_root: discovery.path().display().to_string(),
            ..Default::default()
        };

        // Unknown baseline commit: checkout fails, falls back to the branch,
        // workspace reported not aligned ( init_project `requires_patch`).
        let out = sync_workspace(
            &cfg,
            None,
            None,
            "demo",
            &src,
            &branch,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .await
        .expect("sync with fallback failed");
        assert!(
            !out.is_aligned,
            "failed commit checkout must mark not-aligned"
        );
        let head = git_out(&src, &["rev-parse", "HEAD"]).await;
        assert_eq!(out.actual_commit, head, "clone must end on the branch head");
    }
}
