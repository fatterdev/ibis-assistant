//! Integration tests over a REAL git repository in a tempdir: anchors,
//! notes plumbing, path-scoped claim commits, secret masking, and the
//! owned-clone sync/ingest command shapes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use threski_git::{
    ancestor_set_for, detect_provider, get_host, GitAnchorRunner, GitAnchorSource, GitClaimWriter,
    GitCommitter, GitError, GitNotesRunner, GitNotesSource, GitRunner, NOTES_REF,
};

fn str_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn run_ok(runner: &GitRunner, dir: &str, args: &[&str]) -> String {
    let out = runner
        .run(dir, args)
        .await
        .unwrap_or_else(|e| panic!("git {args:?} in {dir} failed: {e}"));
    String::from_utf8_lossy(&out).into_owned()
}

struct Repo {
    _dir: TempDir,
    path: String,
}

impl Repo {
    fn s(&self) -> &str {
        &self.path
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    fn parent(&self) -> String {
        str_of(self._dir.path())
    }

    async fn run(&self, args: &[&str]) -> String {
        run_ok(&GitRunner::anonymous(), &self.path, args).await
    }

    async fn head(&self) -> String {
        self.run(&["rev-parse", "HEAD"]).await.trim().to_string()
    }

    async fn branch(&self) -> String {
        self.run(&["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .trim()
            .to_string()
    }

    async fn commit_file(&self, rel: &str, content: &str, msg: &str) -> String {
        let abs = self.root().join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        self.run(&["add", rel]).await;
        self.run(&["commit", "-q", "-m", msg]).await;
        self.head().await
    }
}

async fn init_repo(name: &str) -> Repo {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    let repo_path = str_of(&path);
    let r = GitRunner::anonymous();
    run_ok(&r, &repo_path, &["init", "-q"]).await;
    run_ok(&r, &repo_path, &["config", "user.name", "Test"]).await;
    run_ok(
        &r,
        &repo_path,
        &["config", "user.email", "test@example.com"],
    )
    .await;
    run_ok(&r, &repo_path, &["config", "core.autocrlf", "false"]).await;
    Repo {
        _dir: dir,
        path: repo_path,
    }
}

/// Clones inherit global core.autocrlf; pin it so worktree bytes are
/// deterministic for content assertions.
async fn pin_autocrlf(repo_path: &str) {
    run_ok(
        &GitRunner::anonymous(),
        repo_path,
        &["config", "core.autocrlf", "false"],
    )
    .await;
}

// --- anchors over a real repo ---------------------------------------------

#[tokio::test]
async fn anchor_source_basic_facts() {
    let repo = init_repo("repo").await;
    repo.commit_file("src/a.txt", "alpha\n", "add a").await;

    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    let head = anchors.rev_parse_head(repo.s()).await.unwrap();
    assert_eq!(head, repo.head().await);
    assert_eq!(head.len(), 40);

    let branch = anchors.head_branch(repo.s()).await.unwrap();
    assert_eq!(branch, repo.branch().await);
    assert!(!branch.is_empty() && branch != "HEAD");

    assert!(!anchors.is_shallow(repo.s()).await.unwrap());
    assert!(anchors.is_clean(repo.s()).await.unwrap());

    std::fs::write(repo.root().join("dirty.txt"), "x").unwrap();
    assert!(!anchors.is_clean(repo.s()).await.unwrap());
}

#[tokio::test]
async fn resolve_ref_peels_and_verifies() {
    let repo = init_repo("repo").await;
    let head = repo.commit_file("f.txt", "one\n", "c1").await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    assert_eq!(anchors.resolve_ref(repo.s(), "HEAD").await.unwrap(), head);
    assert_eq!(anchors.resolve_ref(repo.s(), &head).await.unwrap(), head);
    let short = &head[..7];
    assert_eq!(anchors.resolve_ref(repo.s(), short).await.unwrap(), head);

    repo.run(&["branch", "feature"]).await;
    assert_eq!(
        anchors.resolve_ref(repo.s(), "feature").await.unwrap(),
        head
    );

    let err = anchors
        .resolve_ref(repo.s(), "no-such-ref")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("resolve ref no-such-ref"), "{err}");
    assert!(anchors.resolve_ref(repo.s(), "   ").await.is_err());
}

#[tokio::test]
async fn work_and_head_file_hashes_match_sha256() {
    use sha2::{Digest, Sha256};

    let repo = init_repo("repo").await;
    repo.commit_file("src/a.txt", "alpha\n", "add a").await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    let work = anchors
        .work_file_hash(repo.s(), "src/a.txt")
        .await
        .unwrap()
        .unwrap();
    let head = anchors
        .head_file_hash(repo.s(), "src/a.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(work, head);

    let expected: String = Sha256::digest(b"alpha\n")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(work, expected);

    assert!(anchors
        .work_file_hash(repo.s(), "missing.txt")
        .await
        .unwrap()
        .is_none());
    assert!(anchors
        .head_file_hash(repo.s(), "missing.txt")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn list_tree_and_file_at_ref() {
    let repo = init_repo("repo").await;
    repo.commit_file("src/a.txt", "alpha\n", "a").await;
    repo.commit_file("docs/b.md", "# doc\n", "b").await;
    let head = repo.head().await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    let tree = anchors.list_tree(repo.s(), &head).await.unwrap();
    assert!(tree.contains(&"src/a.txt".to_string()));
    assert!(tree.contains(&"docs/b.md".to_string()));

    let bytes = anchors
        .file_at_ref(repo.s(), &head, "src/a.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"alpha\n");
    assert!(anchors
        .file_at_ref(repo.s(), &head, "nope.txt")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn verify_commits_batch_check() {
    let repo = init_repo("repo").await;
    let head = repo.commit_file("f.txt", "x\n", "c").await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    let bogus = "0".repeat(40);
    let missing = anchors
        .verify_commits(repo.s(), &[head.clone(), bogus.clone()])
        .await
        .unwrap();
    assert!(!missing.contains(&head));
    assert!(missing.contains(&bogus));

    assert!(anchors
        .verify_commits(repo.s(), &[])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn list_ancestors_covers_history() {
    let repo = init_repo("repo").await;
    let c1 = repo.commit_file("f.txt", "1\n", "c1").await;
    repo.commit_file("f.txt", "2\n", "c2").await;
    let c3 = repo.commit_file("f.txt", "3\n", "c3").await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    let oids = anchors.list_ancestors(repo.s(), &c3).await.unwrap();
    assert_eq!(oids.len(), 3);
    assert_eq!(oids[0], c3);
    assert!(oids.contains(&c1));
}

#[tokio::test]
async fn file_history_includes_deleted_paths() {
    let repo = init_repo("repo").await;
    let c1 = repo.commit_file("gone.txt", "v1\n", "c1").await;
    let c2 = repo.commit_file("gone.txt", "v2\n", "c2").await;
    std::fs::remove_file(repo.root().join("gone.txt")).unwrap();
    repo.run(&["add", "gone.txt"]).await;
    repo.run(&["commit", "-q", "-m", "c3 delete"]).await;
    let c3 = repo.head().await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());

    let history = anchors.file_history(repo.s(), "gone.txt").await.unwrap();
    assert_eq!(
        history,
        vec![c3, c2, c1],
        "newest first, including the deletion"
    );
}

#[tokio::test]
async fn merge_base_and_diff_paths() {
    let repo = init_repo("repo").await;
    let base = repo.commit_file("base.txt", "b\n", "base").await;
    let default_branch = repo.branch().await;

    repo.run(&["checkout", "-q", "-b", "side"]).await;
    let side_head = repo.commit_file("side.txt", "s\n", "side").await;
    repo.run(&["checkout", "-q", &default_branch]).await;
    let main_head = repo.commit_file("main.txt", "m\n", "main").await;

    let anchors = GitAnchorRunner::new(GitRunner::anonymous());
    let mb = anchors
        .merge_base(repo.s(), &main_head, &side_head)
        .await
        .unwrap();
    assert_eq!(mb, base);

    let changed = anchors
        .diff_paths(repo.s(), &base, &main_head)
        .await
        .unwrap();
    assert_eq!(changed, vec!["main.txt".to_string()]);
}

#[tokio::test]
async fn ancestry_cache_over_real_repo() {
    let repo = init_repo("repo").await;
    repo.commit_file("f.txt", "1\n", "c1").await;
    repo.commit_file("f.txt", "2\n", "c2").await;
    let anchors = GitAnchorRunner::new(GitRunner::anonymous());
    let cache = threski_git::AncestorCache::new();
    let tip = repo.head().await;

    let set = ancestor_set_for(&anchors, &cache, repo.s(), &tip)
        .await
        .unwrap();
    assert_eq!(set.size(), 2);
    assert_eq!(set.ref_oid(), tip);

    // Same resolved oid → cache hit, same content.
    let again = ancestor_set_for(&anchors, &cache, repo.s(), &tip)
        .await
        .unwrap();
    assert_eq!(again.size(), 2);

    // A new commit is a new resolved oid → fresh set includes it.
    repo.commit_file("f.txt", "3\n", "c3").await;
    let fresh = ancestor_set_for(&anchors, &cache, repo.s(), "HEAD")
        .await
        .unwrap();
    assert_eq!(fresh.size(), 3);
    assert_eq!(fresh.ref_oid(), repo.head().await);

    // Invalidation drops everything for the repo; queries recompute.
    cache.invalidate(repo.s());
    repo.commit_file("f.txt", "4\n", "c4").await;
    let after = ancestor_set_for(&anchors, &cache, repo.s(), "HEAD")
        .await
        .unwrap();
    assert_eq!(after.size(), 4);
}

#[tokio::test]
async fn shallow_clone_fails_ancestry_closed() {
    let origin = init_repo("origin").await;
    origin.commit_file("f.txt", "1\n", "c1").await;
    origin.commit_file("f.txt", "2\n", "c2").await;

    let clone_dir = TempDir::new().unwrap();
    let dest = clone_dir.path().join("shallow");
    let r = GitRunner::anonymous();
    // Local-path clones ignore --depth; the file:// transport honors it.
    let origin_url = format!("file:///{}", origin.s().replace('\\', "/"));
    run_ok(
        &r,
        str_of(clone_dir.path()).as_str(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            origin_url.as_str(),
            str_of(&dest).as_str(),
        ],
    )
    .await;
    let dest_path = str_of(&dest);

    let anchors = GitAnchorRunner::new(GitRunner::anonymous());
    assert!(anchors.is_shallow(&dest_path).await.unwrap());

    let cache = threski_git::AncestorCache::new();
    let err = ancestor_set_for(&anchors, &cache, &dest_path, "HEAD")
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::ShallowHistory { .. }), "{err}");
}

// --- notes plumbing --------------------------------------------------------

#[tokio::test]
async fn notes_list_missing_ref_is_empty_not_error() {
    let repo = init_repo("repo").await;
    repo.commit_file("f.txt", "x\n", "c").await;
    let notes = GitNotesRunner::new(GitRunner::anonymous());
    assert!(notes
        .notes_list(repo.s(), NOTES_REF)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn notes_add_force_overwrite_remove_roundtrip() {
    let repo = init_repo("repo").await;
    let c1 = repo.commit_file("f.txt", "1\n", "c1").await;
    let c2 = repo.commit_file("f.txt", "2\n", "c2").await;

    let notes = GitNotesRunner::new(GitRunner::anonymous());
    notes
        .notes_add(repo.s(), NOTES_REF, &c2, "claim=c1\nclaim=c2")
        .await
        .unwrap();
    let list = notes.notes_list(repo.s(), NOTES_REF).await.unwrap();
    assert_eq!(
        list.get(&c2).map(String::as_str),
        Some("claim=c1\nclaim=c2")
    );

    // add is force: overwrites the note content.
    notes
        .notes_add(repo.s(), NOTES_REF, &c2, "claim=c3")
        .await
        .unwrap();
    let list = notes.notes_list(repo.s(), NOTES_REF).await.unwrap();
    assert_eq!(list.get(&c2).map(String::as_str), Some("claim=c3"));

    // A second object gets its own note; both are listed.
    notes
        .notes_add(repo.s(), NOTES_REF, &c1, "claim=c1")
        .await
        .unwrap();
    let list = notes.notes_list(repo.s(), NOTES_REF).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.get(&c1).map(String::as_str), Some("claim=c1"));

    // Remove then re-remove: absent remove is a no-op.
    notes.notes_remove(repo.s(), NOTES_REF, &c2).await.unwrap();
    let list = notes.notes_list(repo.s(), NOTES_REF).await.unwrap();
    assert_eq!(list.len(), 1);
    notes.notes_remove(repo.s(), NOTES_REF, &c2).await.unwrap();
    notes.notes_remove(repo.s(), NOTES_REF, &c1).await.unwrap();
    assert!(notes
        .notes_list(repo.s(), NOTES_REF)
        .await
        .unwrap()
        .is_empty());
}

// --- claim commit plumbing -------------------------------------------------

#[tokio::test]
async fn commit_claim_is_path_scoped_and_returns_head() {
    let repo = init_repo("repo").await;
    repo.commit_file("work.txt", "v1\n", "work").await;
    // Unrelated dirty file that must never be swept into the claim commit.
    std::fs::write(repo.root().join("work.txt"), "v2 dirty\n").unwrap();

    let before = repo.head().await;
    let committer = GitCommitter::new(GitRunner::anonymous());
    let mut writes = HashMap::new();
    writes.insert(".threski/claims/c1.md".to_string(), b"claim one".to_vec());
    let oid = committer
        .commit_claim(repo.s(), "chore(claims): add c1", &writes, &[])
        .await
        .unwrap();

    assert_ne!(oid, before);
    assert_eq!(oid, repo.head().await);

    let names = repo
        .run(&["show", "--name-only", "--format=", "HEAD"])
        .await;
    assert!(names.contains(".threski/claims/c1.md"));
    assert!(
        !names.contains("work.txt"),
        "unrelated dirty file swept in: {names}"
    );

    let status = repo.run(&["status", "--porcelain"]).await;
    assert!(
        status.contains("work.txt"),
        "unrelated dirty file lost: {status}"
    );
    assert_eq!(
        std::fs::read(repo.root().join(".threski/claims/c1.md")).unwrap(),
        b"claim one"
    );
}

#[tokio::test]
async fn commit_claim_with_deletes_and_empty_writes() {
    let repo = init_repo("repo").await;
    repo.commit_file("base.txt", "b\n", "base").await;
    let committer = GitCommitter::new(GitRunner::anonymous());

    // Empty writes: HEAD returned unchanged, no commit created.
    let before = repo.head().await;
    let oid = committer
        .commit_claim(repo.s(), "nothing", &HashMap::new(), &[])
        .await
        .unwrap();
    assert_eq!(oid, before);

    // Land a claim, then withdraw it via deletes.
    let mut writes = HashMap::new();
    writes.insert(".threski/claims/c1.md".to_string(), b"claim one".to_vec());
    committer
        .commit_claim(repo.s(), "chore(claims): add c1", &writes, &[])
        .await
        .unwrap();
    assert!(repo.root().join(".threski/claims/c1.md").exists());

    let head_before_delete = repo.head().await;
    let oid = committer
        .commit_claim(
            repo.s(),
            "chore(claims): remove c1",
            &HashMap::new(),
            &[".threski/claims/c1.md".to_string()],
        )
        .await
        .unwrap();
    assert_ne!(oid, head_before_delete);
    assert!(!repo.root().join(".threski/claims/c1.md").exists());

    let anchors = GitAnchorRunner::new(GitRunner::anonymous());
    assert!(
        anchors
            .file_at_ref(repo.s(), &oid, ".threski/claims/c1.md")
            .await
            .unwrap()
            .is_none(),
        "claim file still present at new HEAD"
    );
}

#[tokio::test]
async fn failed_commit_leaves_no_staged_residue() {
    let repo = init_repo("repo").await;
    repo.commit_file("base.txt", "b\n", "base").await;

    // Deterministic commit failure: a pre-commit hook that exits 1.
    let hook = repo.root().join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let committer = GitCommitter::new(GitRunner::anonymous());
    let mut writes = HashMap::new();
    writes.insert(".threski/claims/c1.md".to_string(), b"x".to_vec());
    let err = committer
        .commit_claim(repo.s(), "chore(claims): add c1", &writes, &[])
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("git command failed:"), "{err}");

    // The runner reset -q -- <paths> our paths: nothing left staged.
    let cached = repo.run(&["diff", "--cached", "--name-only"]).await;
    assert!(
        cached.trim().is_empty(),
        "staged residue after failed commit: {cached}"
    );
    // The claim file is still on disk (a retry can land it).
    assert!(repo.root().join(".threski/claims/c1.md").exists());

    // Fix the failure and retry succeeds.
    std::fs::remove_file(&hook).unwrap();
    let oid = committer
        .commit_claim(repo.s(), "chore(claims): add c1", &writes, &[])
        .await
        .unwrap();
    assert_eq!(oid, repo.head().await);
}

#[tokio::test]
async fn set_commit_identity_configures_local_config() {
    let repo = init_repo("repo").await;
    let committer = GitCommitter::new(GitRunner::anonymous());
    committer
        .set_commit_identity(repo.s(), "Threski", "threski@example.com")
        .await
        .unwrap();
    assert_eq!(repo.run(&["config", "user.name"]).await.trim(), "Threski");
    assert_eq!(
        repo.run(&["config", "user.email"]).await.trim(),
        "threski@example.com"
    );
}

// --- secret masking end-to-end ---------------------------------------------

#[tokio::test]
async fn runner_masks_token_in_success_output() {
    let repo = init_repo("repo").await;
    let token = "ghp_superSecretToken12345";
    repo.run(&[
        "remote",
        "add",
        "origin",
        &format!("https://oauth2:{token}@github.com/org/repo.git"),
    ])
    .await;

    let cred = threski_git::Credential::token_credential("github.com", "github", token);
    let runner = GitRunner::new(Some(cred));
    let out = runner.run(repo.s(), &["remote", "-v"]).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(
        !text.contains(token),
        "token leaked in success output: {text}"
    );
    assert!(text.contains("[REDACTED]"));
}

#[tokio::test]
async fn runner_masks_token_in_error_output() {
    let repo = init_repo("repo").await;
    let token = "superSecretToken12345";
    repo.run(&[
        "remote",
        "add",
        "origin",
        &format!("https://oauth2:{token}@127.0.0.1:1/x.git"),
    ])
    .await;

    let cred = threski_git::Credential::token_credential("127.0.0.1", "generic", token);
    let runner = GitRunner::new(Some(cred));
    let err = runner
        .run(repo.s(), &["fetch", "origin"])
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        !text.contains(token),
        "token leaked in error output: {text}"
    );
    assert!(err.command_output().is_some());
}

#[test]
fn provider_and_host_helpers() {
    assert_eq!(detect_provider("https://github.com/a/b"), "github");
    assert_eq!(detect_provider("https://gitlab.com/a/b"), "gitlab");
    assert_eq!(detect_provider("https://dev.azure.com/a/b"), "azure_devops");
    assert_eq!(detect_provider("https://example.com/a"), "generic");
    assert_eq!(
        get_host("git@github.com:org/repo.git").as_deref(),
        Some("github.com")
    );
    assert_eq!(
        get_host("https://oauth2:t@github.com/a").as_deref(),
        Some("github.com")
    );
}

// --- owned-clone sync + ingest command shapes ------------------------------

#[tokio::test]
async fn owned_clone_sync_flow() {
    use threski_git::sync;

    let origin = init_repo("origin").await;
    let dir = origin.parent();
    let r = GitRunner::anonymous();
    let c1 = origin.commit_file("file.txt", "v1\n", "c1").await;
    let default_branch = origin.branch().await;
    let c2 = origin.commit_file("file.txt", "v2\n", "c2").await;
    origin.commit_file("new.txt", "added\n", "c3").await;
    origin.run(&["branch", "feature"]).await;
    origin.run(&["checkout", "-q", "feature"]).await;
    let feature_head = origin.commit_file("feat.txt", "f\n", "feat").await;
    origin.run(&["checkout", "-q", &default_branch]).await;

    // clone into dest (HEAD = origin default branch tip = c3).
    let dest = origin._dir.path().join("dest");
    sync::clone_repo(&r, &dir, origin.s(), str_of(&dest).as_str(), None)
        .await
        .unwrap();
    let dest_path = str_of(&dest);
    let dest_head = run_ok(&r, &dest_path, &["rev-parse", "HEAD"]).await;
    assert_eq!(dest_head.trim(), origin.head().await);
    pin_autocrlf(&dest_path).await;

    // clone -b feature pins the branch.
    let dest2 = origin._dir.path().join("dest2");
    sync::clone_repo(
        &r,
        &dir,
        origin.s(),
        str_of(&dest2).as_str(),
        Some("feature"),
    )
    .await
    .unwrap();
    let dest2_head = run_ok(&r, str_of(&dest2).as_str(), &["rev-parse", "HEAD"]).await;
    assert_eq!(dest2_head.trim(), feature_head);
    pin_autocrlf(str_of(&dest2).as_str()).await;
    sync::reset_hard(&r, str_of(&dest2).as_str()).await.unwrap();

    // clean -fd removes untracked files (owned clones only).
    std::fs::write(dest.join("untracked.txt"), "junk").unwrap();
    sync::clean_untracked(&r, &dest_path).await.unwrap();
    assert!(!dest.join("untracked.txt").exists());

    // reset --hard restores tracked files (owned clones only).
    std::fs::write(dest.join("file.txt"), "dirty\n").unwrap();
    sync::reset_hard(&r, &dest_path).await.unwrap();
    assert_eq!(std::fs::read(dest.join("file.txt")).unwrap(), b"v2\n");

    // ensure_remote_origin adds then rewrites origin.
    let dest3 = origin._dir.path().join("dest3");
    std::fs::create_dir_all(&dest3).unwrap();
    let dest3_path = str_of(&dest3);
    run_ok(&r, &dest3_path, &["init", "-q"]).await;
    sync::ensure_remote_origin(&r, &dest3_path, origin.s())
        .await
        .unwrap();
    let url = run_ok(&r, &dest3_path, &["remote", "get-url", "origin"]).await;
    assert_eq!(url.trim(), origin.s());
    sync::ensure_remote_origin(&r, &dest3_path, "https://example.com/repo.git")
        .await
        .unwrap();
    let url = run_ok(&r, &dest3_path, &["remote", "get-url", "origin"]).await;
    assert_eq!(url.trim(), "https://example.com/repo.git");

    // fetch origin picks up new commits from origin.
    origin.commit_file("later.txt", "l\n", "later").await;
    sync::fetch_origin(&r, &dest_path).await.unwrap();

    // checkout branch target.
    sync::checkout(&r, str_of(&dest2).as_str(), &default_branch)
        .await
        .unwrap();

    // history_oids is oldest-first and feeds ingest_log_batch.
    let oids = sync::history_oids(&r, origin.s()).await.unwrap();
    assert_eq!(oids[0], c1);
    assert_eq!(*oids.last().unwrap(), origin.head().await);
    let raw = sync::ingest_log_batch(&r, origin.s(), &oids).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    for oid in &oids {
        assert!(
            text.contains(&format!("COMMIT|{oid}|")),
            "missing {oid} in numstat output"
        );
    }
    assert!(text.contains("|Test|"), "author field missing: {text}");

    // commit patch extraction: diff <h>^ <h> and show --format= --patch.
    let patch = sync::commit_patch(&r, origin.s(), &c2).await.unwrap();
    let patch_text = String::from_utf8_lossy(&patch);
    assert!(patch_text.contains("file.txt"), "{patch_text}");
    assert!(patch_text.contains("+v2"));

    let root_patch = sync::show_commit_patch(&r, origin.s(), &c1).await.unwrap();
    assert!(String::from_utf8_lossy(&root_patch).contains("file.txt"));
    // Root commits have no parent: diff form fails, show form is the fallback.
    assert!(sync::commit_patch(&r, origin.s(), &c1).await.is_err());

    // apply --3way lands the v1→v2 patch on the clone, then plain apply.
    run_ok(&r, &dest_path, &["checkout", "-q", "-f", &c1]).await;
    std::fs::write(dest.join("threski-local.patch"), &patch).unwrap();
    sync::apply_patch_three_way(&r, &dest_path, "threski-local.patch")
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest.join("file.txt")).unwrap(), b"v2\n");

    run_ok(&r, &dest_path, &["checkout", "-q", "-f", &c1]).await;
    std::fs::write(dest.join("threski-local.patch"), &patch).unwrap();
    sync::apply_patch(&r, &dest_path, "threski-local.patch")
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest.join("file.txt")).unwrap(), b"v2\n");
}
