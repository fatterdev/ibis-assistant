//! Real-git integration test for the batch manager's patch extraction
//!: `git diff <hash>^ <hash>` with the
//! `git show --format= --patch <hash>` fallback for root commits.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use threski_ai::{CommitPatchSource, GitPatchSource};

struct Repo {
    _dir: TempDir,
    path: String,
}

fn run(dir: &str, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Repo {
    fn commit_file(&self, rel: &str, content: &str, msg: &str) -> String {
        let abs = PathBuf::from(&self.path).join(rel);
        std::fs::write(&abs, content).unwrap();
        run(&self.path, &["add", rel]);
        run(&self.path, &["commit", "-q", "-m", msg]);
        run(&self.path, &["rev-parse", "HEAD"]).trim().to_string()
    }
}

fn init_repo() -> Repo {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("patchrepo");
    let path_str = path.to_string_lossy().into_owned();
    std::fs::create_dir_all(&path).unwrap();
    run(&path_str, &["init", "-q"]);
    run(&path_str, &["config", "user.name", "Test"]);
    run(&path_str, &["config", "user.email", "test@example.com"]);
    Repo {
        _dir: dir,
        path: path_str,
    }
}

fn extractor() -> impl CommitPatchSource {
    GitPatchSource
}

#[tokio::test]
async fn extracts_patch_for_regular_commit() {
    let repo = init_repo();
    let first = repo.commit_file("a.txt", "one\n", "first");
    let second = repo.commit_file("a.txt", "one\ntwo\n", "second");

    let patch = extractor()
        .commit_patch(Path::new(&repo.path), &second)
        .await
        .unwrap();
    assert!(patch.contains("diff --git a/a.txt b/a.txt"));
    assert!(patch.contains("+two"));

    // Same-commit idempotence: diff HEAD^ HEAD is deterministic.
    let patch2 = extractor()
        .commit_patch(Path::new(&repo.path), &second)
        .await
        .unwrap();
    assert_eq!(patch, patch2);
    let _ = first;
}

#[tokio::test]
async fn root_commit_falls_back_to_show() {
    let repo = init_repo();
    let root = repo.commit_file("a.txt", "initial\n", "root commit");

    // `git diff <root>^ <root>` fails (no parent) → `git show` fallback.
    let patch = extractor()
        .commit_patch(Path::new(&repo.path), &root)
        .await
        .unwrap();
    assert!(patch.contains("diff --git a/a.txt b/a.txt"));
    assert!(patch.contains("+initial"));
}

#[tokio::test]
async fn unknown_hash_reports_combined_error() {
    let repo = init_repo();
    repo.commit_file("a.txt", "one\n", "first");
    let err = extractor()
        .commit_patch(Path::new(&repo.path), "0".repeat(40).as_str())
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("git diff/show failed in"));
    assert!(msg.contains(" / "));
}
