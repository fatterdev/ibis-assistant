//! Hot-ref code byte cache. Strategy A — the git object read —
//! remains the source of truth: the cache is keyed by the resolved full
//! commit OID + path, both immutable, so a hit is always the exact bytes git
//! would return. The kill-switch (`world_state.code_cache_disabled`) or a
//! degraded (no-DB) state leaves every read on strategy A; correctness never
//! depends on the cache.
//!
//! Port delta (F-002 + PR #35 note): interpolated the blob content
//! through a bespoke C0-escaping helper into the UPSERT statement; the Rust
//! port binds the content as `$content` through the driver's parameter
//! encoder, which is lossless for any valid-UTF-8 string — byte-exact
//! storage without an escape helper.

use threski_db::value::Value;

use crate::error::Result;
use crate::search::SearchService;

/// Hot-ref TTL.
pub(crate) const FILE_AT_COMMIT_TTL: &str = "30d";

/// The `file_at_commit` table (schemaless, OID+path keyed).
pub(crate) const TABLE_FILE_AT_COMMIT: &str = "file_at_commit";

/// Builds the deterministic record id for one cached byte read: oid and path
/// are immutable, so the id is content-stable. The `⟨oid:path⟩` form relies
/// on [`cacheable_file_key`] having vetted the path
///
pub(crate) fn file_at_commit_record_id(oid: &str, path: &str) -> String {
    format!("{TABLE_FILE_AT_COMMIT}:⟨{oid}:{path}⟩")
}

/// Vets the (oid, path) key for safe embedding in a record id: oids are full
/// 40-hex (resolved by the ancestor set), paths must not carry record-id
/// hostiles. Keys that fail the vet bypass the cache and go straight to git
///
pub(crate) fn cacheable_file_key(oid: &str, path: &str) -> bool {
    if oid.len() != 40 {
        return false;
    }
    if path.is_empty() || path.contains(['⟨', '⟩', '\'', '"', '\\', ';', '`']) {
        return false;
    }
    for r in path.chars() {
        if (r as u32) < 0x20 || r as u32 == 0x7f {
            return false;
        }
    }
    true
}

/// Read-through accessor over [`SearchService::git_anchors`] for the as_of
/// code paths. Cache hit serves the stored bytes,
/// miss falls through to git and populates the cache best-effort. Degraded
/// mode (no DB) and the kill-switch never touch the cache.
impl SearchService {
    /// Returns `(content, found)`: content is the byte-exact UTF-8 body when
    /// found (non-UTF-8 blobs are lossy-converted per D-A13 — carried raw
    /// bytes in its string type; those reads never enter the cache).
    pub(crate) async fn cached_file_at_ref(
        &self,
        repo_path: &str,
        oid: &str,
        path: &str,
    ) -> Result<(String, bool)> {
        let anchors = self.git_anchors.as_ref().ok_or_else(|| {
            crate::error::SearchError::other("file at ref requires the git anchor source")
        })?;
        if self.hot_ref_cache_disabled || self.db.is_none() || !cacheable_file_key(oid, path) {
            return match anchors.file_at_ref(repo_path, oid, path).await? {
                Some(bytes) => Ok((String::from_utf8_lossy(&bytes).into_owned(), true)),
                None => Ok((String::new(), false)),
            };
        }
        let db = self.db.as_ref().expect("checked above");
        let record_id = file_at_commit_record_id(oid, path);
        let sql = format!("SELECT content FROM {record_id};");
        if let Ok(outcome) = db.execute(&sql).await {
            if let Some(rows_values) = crate::rows::normalize_rows(&outcome) {
                for row in crate::rows::object_rows(&rows_values) {
                    match crate::rows::field(row, "content") {
                        threski_db::value::Value::None | threski_db::value::Value::Null => continue,
                        other => {
                            return Ok((crate::rows::scalar_string(other), true));
                        }
                    }
                }
            }
        }
        let (content, found, cacheable) = match anchors.file_at_ref(repo_path, oid, path).await? {
            Some(bytes) => match std::str::from_utf8(&bytes) {
                // Valid UTF-8: byte-exact body, eligible for the cache.
                Ok(text) => (text.to_string(), true, true),
                // Git blobs are raw bytes: a non-UTF-8 (legacy-encoded or
                // binary) blob must not enter the cache — bound storage is
                // lossless only for valid UTF-8, and a mangled cache hit
                // would break the byte-exact invariant. Such reads are
                // served lossy-converted per D-A13 and always straight from
                // git (Copilot-review fix f40bf82 parity).
                Err(_) => (String::from_utf8_lossy(&bytes).into_owned(), true, false),
            },
            None => (String::new(), false, false),
        };
        if !found || !cacheable {
            return Ok((content, found));
        }
        let text = content.as_str();
        // Best-effort populate: a failed write only costs the next read a
        // git spawn, never correctness. Content, oid, path and hash all
        // bind (F-002).
        let sql = format!(
            "UPSERT {record_id} SET oid = $oid, path = $path, hash = $hash, content = $content, created_at = time::now();"
        );
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("oid".into(), Value::from_t(oid));
        vars.insert("path".into(), Value::from_t(path));
        vars.insert(
            "hash".into(),
            Value::from_t(super::gitbound::hash_bytes(text.as_bytes())),
        );
        vars.insert("content".into(), Value::from_t(text));
        let _ = db.smart_query(&sql, vars).await;
        Ok((content, found))
    }

    /// Drops cached byte reads older than the TTL (hot refs are recent refs;
    /// the prune bounds storage without touching the correctness path).
    /// Returns the number of pruned rows.
    pub async fn prune_file_at_commit_cache(&self) -> Result<usize> {
        let Some(db) = self.db.as_ref() else {
            return Ok(0);
        };
        if self.hot_ref_cache_disabled {
            return Ok(0);
        }
        let sql = format!(
            // 3.x DELETE no longer returns the dropped rows ( 2.x did):
            // `RETURN AFTER` restores the deleted-row list so the count is
            // the documented contract.
            "DELETE {TABLE_FILE_AT_COMMIT} WHERE created_at < time::now() - {FILE_AT_COMMIT_TTL} RETURN AFTER;"
        );
        let outcome = db.execute(&sql).await.map_err(|e| {
            crate::error::SearchError::other(format!("prune file_at_commit cache: {e}"))
        })?;
        Ok(crate::rows::normalize_rows(&outcome)
            .map(|r| r.len())
            .unwrap_or(0))
    }
}
