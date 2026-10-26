use crate::error::GitError;
use crate::runner::{parse_lines, GitRunner};

/// `git clone [-b <branch>] <url> <path>` for server-owned clones (
/// ). Personal/plugin workspaces never clone.
pub async fn clone_repo(
    runner: &GitRunner,
    dir: &str,
    url: &str,
    path: &str,
    branch: Option<&str>,
) -> Result<Vec<u8>, GitError> {
    let mut args: Vec<&str> = vec!["clone"];
    if let Some(branch) = branch {
        args.push("-b");
        args.push(branch);
    }
    args.push(url);
    args.push(path);
    runner.run(dir, &args).await
}

/// `git clean -fd` (owned clones only).
pub async fn clean_untracked(runner: &GitRunner, dir: &str) -> Result<Vec<u8>, GitError> {
    runner.run(dir, &["clean", "-fd"]).await
}

/// `git reset --hard` (owned clones only).
pub async fn reset_hard(runner: &GitRunner, dir: &str) -> Result<Vec<u8>, GitError> {
    runner.run(dir, &["reset", "--hard"]).await
}

/// Points `origin` at `url`: `remote set-url` first, `remote add` when no
/// origin exists. Failures are logged and swallowed — the subsequent fetch
/// surfaces the real error (`SyncWorkspace behavior).
pub async fn ensure_remote_origin(
    runner: &GitRunner,
    dir: &str,
    url: &str,
) -> Result<(), GitError> {
    if runner
        .run(dir, &["remote", "set-url", "origin", url])
        .await
        .is_err()
    {
        if let Err(e) = runner.run(dir, &["remote", "add", "origin", url]).await {
            tracing::warn!(error = %e, "Failed to add remote origin");
        }
    }
    Ok(())
}

/// `git fetch origin` (owned clones only). Callers should invalidate the
/// ancestor cache afterwards (the world-state spec).
pub async fn fetch_origin(runner: &GitRunner, dir: &str) -> Result<Vec<u8>, GitError> {
    runner.run(dir, &["fetch", "origin"]).await
}

/// `git checkout <target>` (branch or commit; owned clones only).
pub async fn checkout(runner: &GitRunner, dir: &str, target: &str) -> Result<Vec<u8>, GitError> {
    runner.run(dir, &["checkout", target]).await
}

/// `git apply --whitespace=nowarn <patch>` — first attempt of the
/// `sync_local_patch` flow
pub async fn apply_patch(
    runner: &GitRunner,
    dir: &str,
    patch_path: &str,
) -> Result<Vec<u8>, GitError> {
    runner
        .run(dir, &["apply", "--whitespace=nowarn", patch_path])
        .await
}

/// `git apply --3way --whitespace=nowarn <patch>` — fallback of the
/// `sync_local_patch` flow.
pub async fn apply_patch_three_way(
    runner: &GitRunner,
    dir: &str,
    patch_path: &str,
) -> Result<Vec<u8>, GitError> {
    runner
        .run(dir, &["apply", "--3way", "--whitespace=nowarn", patch_path])
        .await
}

/// Every commit OID reachable from HEAD, oldest first
/// (`git log HEAD --reverse --format=%H`) — history ingest driver.
pub async fn history_oids(runner: &GitRunner, dir: &str) -> Result<Vec<String>, GitError> {
    let out = runner
        .run(dir, &["log", "HEAD", "--reverse", "--format=%H"])
        .await?;
    Ok(parse_lines(&out))
}

/// One `git log --no-walk --stdin --numstat --format=COMMIT|%H|%P|%an|%aI|%s`
/// invocation fed a batch of commit hashes on stdin (history ingest; the
/// caller owns batching, e.g. 500-hash batches).
pub async fn ingest_log_batch(
    runner: &GitRunner,
    dir: &str,
    oids: &[String],
) -> Result<Vec<u8>, GitError> {
    let stdin: String = oids.iter().map(|o| format!("{o}\n")).collect();
    runner
        .run_with_stdin(
            dir,
            &stdin,
            &[
                "log",
                "--no-walk",
                "--stdin",
                "--numstat",
                "--format=COMMIT|%H|%P|%an|%aI|%s",
            ],
        )
        .await
}

/// Commit patch extraction, primary form: `git diff <hash>^ <hash>`.
pub async fn commit_patch(runner: &GitRunner, dir: &str, hash: &str) -> Result<Vec<u8>, GitError> {
    let parent = format!("{hash}^");
    runner.run(dir, &["diff", &parent, hash]).await
}

/// Commit patch extraction, fallback form (root commits):
/// `git show --format= --patch <hash>`.
pub async fn show_commit_patch(
    runner: &GitRunner,
    dir: &str,
    hash: &str,
) -> Result<Vec<u8>, GitError> {
    runner
        .run(dir, &["show", "--format=", "--patch", hash])
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_local_git_smoke() {
        let runner = GitRunner::anonymous();
        let tmp = tempfile::tempdir().unwrap();
        let out = runner
            .run(tmp.path().to_string_lossy().as_ref(), &["version"])
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("git version"));
    }
}
