//! Scenarios over the ibis-git `GitAnchorSource` trait + TTL cache
//! (fake injection at the trait boundary).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

use ibis_git::{ancestor_set_for, hold_at, AncestorCache, GitAnchorSource, GitError};

/// oid → parent oid chain: r1(root) ← r2 ← r3 ← r4.
fn graph() -> Vec<(String, String)> {
    vec![
        ("r1".to_string(), String::new()),
        ("r2".to_string(), "r1".to_string()),
        ("r3".to_string(), "r2".to_string()),
        ("r4".to_string(), "r3".to_string()),
    ]
}

fn ancestors_of(oid: &str) -> Vec<String> {
    let graph = graph();
    let mut current = oid.to_string();
    let mut out = Vec::new();
    while let Some((_, parent)) = graph.iter().find(|(o, _)| *o == current) {
        out.push(current.clone());
        if parent.is_empty() {
            break;
        }
        current = parent.clone();
    }
    if out.is_empty() {
        out.push(oid.to_string());
    }
    out
}

#[derive(Default)]
struct FakeAnchors {
    list_calls: AtomicUsize,
    shallow: AtomicBool,
    fail_list: AtomicBool,
}

impl FakeAnchors {
    fn calls(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GitAnchorSource for FakeAnchors {
    async fn rev_parse_head(&self, _repo_path: &str) -> Result<String, GitError> {
        Ok("r4".to_string())
    }

    async fn head_branch(&self, _repo_path: &str) -> Result<String, GitError> {
        Ok("main".to_string())
    }

    async fn work_file_hash(
        &self,
        _repo_path: &str,
        _rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        Ok(None)
    }

    async fn head_file_hash(
        &self,
        _repo_path: &str,
        _rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        Ok(None)
    }

    async fn is_clean(&self, _repo_path: &str) -> Result<bool, GitError> {
        Ok(true)
    }

    async fn verify_commits(
        &self,
        _repo_path: &str,
        _oids: &[String],
    ) -> Result<HashSet<String>, GitError> {
        Ok(HashSet::new())
    }

    async fn list_ancestors(&self, _repo_path: &str, oid: &str) -> Result<Vec<String>, GitError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(GitError::other("rev-list failed"));
        }
        Ok(ancestors_of(oid))
    }

    async fn resolve_ref(&self, _repo_path: &str, reference: &str) -> Result<String, GitError> {
        let reference = reference.trim();
        if graph().iter().any(|(o, _)| o == reference) || reference == "main" || reference == "HEAD"
        {
            return Ok(if reference == "main" || reference == "HEAD" {
                "r4".to_string()
            } else {
                reference.to_string()
            });
        }
        Err(GitError::other(format!("resolve ref {reference}")))
    }

    async fn list_tree(&self, _repo_path: &str, _oid: &str) -> Result<Vec<String>, GitError> {
        Ok(Vec::new())
    }

    async fn file_at_ref(
        &self,
        _repo_path: &str,
        _oid: &str,
        _rel_path: &str,
    ) -> Result<Option<Vec<u8>>, GitError> {
        Ok(None)
    }

    async fn file_history(
        &self,
        _repo_path: &str,
        _rel_path: &str,
    ) -> Result<Vec<String>, GitError> {
        Ok(Vec::new())
    }

    async fn merge_base(&self, _repo_path: &str, _a: &str, _b: &str) -> Result<String, GitError> {
        Ok("r1".to_string())
    }

    async fn is_shallow(&self, _repo_path: &str) -> Result<bool, GitError> {
        Ok(self.shallow.load(Ordering::SeqCst))
    }

    async fn diff_paths(
        &self,
        _repo_path: &str,
        _from: &str,
        _to: &str,
    ) -> Result<Vec<String>, GitError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn exactly_one_rev_list_per_query_cache_hits_afterwards() {
    let anchors = FakeAnchors::default();
    let cache = AncestorCache::new();

    let set = ancestor_set_for(&anchors, &cache, "repo", "main")
        .await
        .unwrap();
    assert_eq!(anchors.calls(), 1, "first query must invoke rev-list once");
    assert_eq!(set.ref_oid(), "r4");
    assert_eq!(set.size(), 4);

    for _ in 0..3 {
        let cached = ancestor_set_for(&anchors, &cache, "repo", "main")
            .await
            .unwrap();
        assert_eq!(
            anchors.calls(),
            1,
            "expected exactly one rev-list invocation (cache hits afterwards)"
        );
        assert!(cached.contains("r1"));
        assert!(cached.contains("r4"));
    }

    // A different ref resolving to the same oid shares the cache entry.
    ancestor_set_for(&anchors, &cache, "repo", "HEAD")
        .await
        .unwrap();
    assert_eq!(anchors.calls(), 1);

    // Another repo is a distinct cache key.
    ancestor_set_for(&anchors, &cache, "repo2", "main")
        .await
        .unwrap();
    assert_eq!(anchors.calls(), 2);

    // Invalidation forces exactly one more rev-list for that repo.
    cache.invalidate("repo");
    ancestor_set_for(&anchors, &cache, "repo", "main")
        .await
        .unwrap();
    assert_eq!(anchors.calls(), 3);
    // repo2 untouched by the scoped invalidation.
    ancestor_set_for(&anchors, &cache, "repo2", "main")
        .await
        .unwrap();
    assert_eq!(anchors.calls(), 3);
}

#[tokio::test]
async fn shallow_clone_fails_closed_without_rev_list() {
    let anchors = FakeAnchors::default();
    anchors.shallow.store(true, Ordering::SeqCst);
    let cache = AncestorCache::new();

    let err = ancestor_set_for(&anchors, &cache, "repo", "main")
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::ShallowHistory { .. }), "{err}");
    assert!(err.to_string().contains("shallow_history"));
    assert!(err.to_string().contains("git fetch --unshallow"));
    assert_eq!(
        anchors.calls(),
        0,
        "shallow check must precede the rev-list call"
    );
}

#[tokio::test]
async fn rev_list_failure_is_contextualized() {
    let anchors = FakeAnchors::default();
    anchors.fail_list.store(true, Ordering::SeqCst);
    let cache = AncestorCache::new();

    let err = ancestor_set_for(&anchors, &cache, "repo", "r2")
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("build ancestor set for r2:"),
        "{err}"
    );
}

#[tokio::test]
async fn missing_arguments_rejected() {
    let anchors = FakeAnchors::default();
    let cache = AncestorCache::new();
    for (repo, reference) in [("", "main"), ("repo", ""), ("  ", "main")] {
        let err = ancestor_set_for(&anchors, &cache, repo, reference)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ancestor set requires"), "{err}");
    }
}

#[tokio::test]
async fn hold_at_uses_materialized_set() {
    let anchors = FakeAnchors::default();
    let cache = AncestorCache::new();
    let set = ancestor_set_for(&anchors, &cache, "repo", "main")
        .await
        .unwrap();

    // HOLDAt(m, r4): I(m)=r2 introduced before tip, no invalidation → holds.
    assert!(hold_at("r2", "", Some(&set)));
    // Invalidated after tip (r4 introduced the invalidation) → still holds.
    assert!(hold_at("r2", "r5", Some(&set)));
    // Invalidated within the ancestor set → does not hold.
    assert!(!hold_at("r2", "r3", Some(&set)));
    // Unanchored → never holds at a historical ref.
    assert!(!hold_at("", "", Some(&set)));
    assert!(!hold_at("r2", "", None));
}
