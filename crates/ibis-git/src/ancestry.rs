use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::anchor::GitAnchorSource;
use crate::error::GitError;

/// The materialized ancestor set of a ref R: every commit OID reachable from
/// R, including R itself (the world-state spec). The normative HOLDAt
/// engine: one rev-list per query, never one merge-base per claim.
#[derive(Clone, Debug)]
pub struct AncestorSet {
    ref_oid: String,
    oids: Arc<HashSet<String>>,
}

impl AncestorSet {
    pub fn new(
        ref_oid: impl Into<String>,
        oids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        AncestorSet {
            ref_oid: ref_oid.into(),
            oids: Arc::new(oids.into_iter().map(Into::into).collect()),
        }
    }

    /// Reports whether oid is an ancestor of (or equal to) the set's ref.
    pub fn contains(&self, oid: &str) -> bool {
        self.oids.contains(oid.trim())
    }

    /// The resolved OID the set was built for.
    pub fn ref_oid(&self) -> &str {
        &self.ref_oid
    }

    /// Number of reachable commits (diagnostics/bench).
    pub fn size(&self) -> usize {
        self.oids.len()
    }
}

/// Evaluates the DAG-precise predicate (the world-state spec):
/// HOLDAt(m, R) ⟺ I(m) ∈ AncestorSet(R) ∧ (X(m) = ⊥ ∨ X(m) ∉ AncestorSet(R)).
/// Unanchored claims (no I(m)) never hold at a historical ref; no ancestor
/// set → false.
pub fn hold_at(
    introduced_in_oid: &str,
    invalidated_at_oid: &str,
    ancestors: Option<&AncestorSet>,
) -> bool {
    let Some(ancestors) = ancestors else {
        return false;
    };
    let introduced = introduced_in_oid.trim();
    if introduced.is_empty() {
        return false;
    }
    if !ancestors.contains(introduced) {
        return false;
    }
    let invalidated = invalidated_at_oid.trim();
    if invalidated.is_empty() {
        return true;
    }
    !ancestors.contains(invalidated)
}

const ANCESTOR_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

struct CacheEntry {
    set: AncestorSet,
    expires: Instant,
}

/// Process-local TTL cache keyed by (repo_path, R); invalidated on
/// fetch/sync (the world-state spec). Expired entries are dropped on read.
pub struct AncestorCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    now: Box<dyn Fn() -> Instant + Send + Sync>,
}

impl Default for AncestorCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AncestorCache {
    pub fn new() -> Self {
        AncestorCache {
            entries: Mutex::new(HashMap::new()),
            now: Box::new(Instant::now),
        }
    }

    #[doc(hidden)]
    pub fn with_clock(now: Box<dyn Fn() -> Instant + Send + Sync>) -> Self {
        AncestorCache {
            entries: Mutex::new(HashMap::new()),
            now,
        }
    }

    fn cache_key(repo_path: &str, oid: &str) -> String {
        format!("{}\x00{}", repo_path.trim(), oid.trim())
    }

    pub fn get(&self, repo_path: &str, oid: &str) -> Option<AncestorSet> {
        let mut entries = self.entries.lock().expect("ancestor cache lock");
        let key = Self::cache_key(repo_path, oid);
        match entries.get(&key) {
            Some(entry) if (self.now)() <= entry.expires => Some(entry.set.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, repo_path: &str, oid: &str, set: AncestorSet) {
        let mut entries = self.entries.lock().expect("ancestor cache lock");
        let key = Self::cache_key(repo_path, oid);
        entries.insert(
            key,
            CacheEntry {
                set,
                expires: (self.now)() + ANCESTOR_CACHE_TTL,
            },
        );
    }

    /// Drops cached ancestor sets for a repo (call after fetch/sync/ingest so
    /// bounds derived from stale history cannot leak). An empty repo_path
    /// clears everything.
    pub fn invalidate(&self, repo_path: &str) {
        let mut entries = self.entries.lock().expect("ancestor cache lock");
        if repo_path.trim().is_empty() {
            entries.clear();
            return;
        }
        let prefix = format!("{}\x00", repo_path.trim());
        entries.retain(|key, _| !key.starts_with(&prefix));
    }
}

/// Builds (or fetches from the TTL cache) the ancestor set of R with exactly
/// one rev-list invocation per query.
pub async fn ancestor_set_for(
    anchors: &dyn GitAnchorSource,
    cache: &AncestorCache,
    repo_path: &str,
    reference: &str,
) -> Result<AncestorSet, GitError> {
    if repo_path.trim().is_empty() || reference.trim().is_empty() {
        return Err(GitError::other(
            "ancestor set requires a git anchor source, repo path, and ref",
        ));
    }
    let oid = anchors.resolve_ref(repo_path, reference).await?;
    // World-state the spec: a shallow clone cannot prove ancestry — fail
    // closed with the dedicated shallow_history marker, never empty Σ.
    // (IsShallow errors are swallowed, as originally.)
    if let Ok(true) = anchors.is_shallow(repo_path).await {
        return Err(GitError::ShallowHistory {
            repo_path: repo_path.to_string(),
        });
    }
    if let Some(cached) = cache.get(repo_path, &oid) {
        return Ok(cached);
    }
    let oids = anchors
        .list_ancestors(repo_path, &oid)
        .await
        .map_err(|e| GitError::context(format!("build ancestor set for {oid}"), e))?;
    let set = AncestorSet::new(oid.clone(), oids);
    cache.put(repo_path, &oid, set.clone());
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn put_get_roundtrip_and_expiry() {
        let offset = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let base = base();
        let tick = std::sync::Arc::clone(&offset);
        let cache = AncestorCache::with_clock(Box::new(move || {
            base.checked_add(Duration::from_secs(
                tick.load(std::sync::atomic::Ordering::SeqCst),
            ))
            .unwrap_or(base)
        }));
        let set = AncestorSet::new("oid1", ["oid1", "oid2"]);
        cache.put("repo", "oid1", set.clone());
        assert!(cache.get("repo", "oid1").is_some());
        offset.store(300, std::sync::atomic::Ordering::SeqCst);
        assert!(cache.get("repo", "oid1").is_some());
        offset.store(301, std::sync::atomic::Ordering::SeqCst);
        assert!(cache.get("repo", "oid1").is_none());
    }

    #[test]
    fn invalidate_scopes_and_clears() {
        let cache = AncestorCache::new();
        cache.put("repoA", "o1", AncestorSet::new("o1", ["o1"]));
        cache.put("repoA", "o2", AncestorSet::new("o2", ["o2"]));
        cache.put("repoB", "o1", AncestorSet::new("o1", ["o1"]));
        cache.invalidate("repoA");
        assert!(cache.get("repoA", "o1").is_none());
        assert!(cache.get("repoA", "o2").is_none());
        assert!(cache.get("repoB", "o1").is_some());
        cache.invalidate("");
        assert!(cache.get("repoB", "o1").is_none());
    }

    #[test]
    fn get_rejects_other_oids() {
        let cache = AncestorCache::new();
        cache.put("repo", "o1", AncestorSet::new("o1", ["o1"]));
        assert!(cache.get("repo", "o2").is_none());
        assert!(cache.get("other", "o1").is_none());
    }

    #[test]
    fn hold_at_matrix() {
        let set = AncestorSet::new("r", ["r", "i", "x"]);
        let some = Some(&set);
        // I present, X absent from the ancestor set → holds (invalidated after tip).
        assert!(hold_at("i", "", some));
        assert!(hold_at("i", "after-tip", some));
        // I present but X also present (invalidated before tip) → does not hold.
        assert!(!hold_at("i", "x", some));
        // I not an ancestor → never holds
        assert!(!hold_at("nope", "", some));
        assert!(!hold_at("", "", some));
        assert!(!hold_at("i", "", None));
    }
}
