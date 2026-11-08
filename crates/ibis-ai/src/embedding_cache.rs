//! B6 — persistent (L2) embedding cache seam.
//!
//! The in-process [`crate::ollama::OllamaProvider`] map is L1 (cap + wholesale
//! clear); this seam adds an L2 store (SurrealDB `embedding_cache` table via
//! the `ibis-server` adapter) that survives process restarts and re-indexes.
//! Best-effort by contract: a missing or failing store degrades to the HTTP
//! path — exactly like [`crate::cost::CostStore`] for token accounting.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Persistent embedding cache seam: exact-key lookups, no ANN. `get` returns
/// `None` on miss **or** on any store failure (callers never branch on
/// errors); `put` is fire-and-forget. `prune` drops entries older than the
/// TTL and returns the pruned row count (default: no-op).
#[async_trait]
pub trait EmbeddingCacheStore: Send + Sync {
    async fn get(&self, key: &str) -> Option<Vec<f32>>;
    async fn put(&self, key: &str, embedding: Vec<f32>);
    async fn prune(&self, ttl_secs: u64) -> usize {
        let _ = ttl_secs;
        0
    }
}

/// `{model}:{dim}:{sha256(text)}` — model and dimension inside the key so a
/// model/dimension switch can never read stale vectors. Embeddings are
/// deterministic per (model, dim, text), so no namespace/owner dimension.
pub fn embedding_cache_key(model: &str, dim: usize, text: &str) -> String {
    let sum = Sha256::digest(text.as_bytes());
    let hash: String = sum.iter().map(|b| format!("{b:02x}")).collect();
    format!("{model}:{dim}:{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_includes_model_and_dim() {
        let a = embedding_cache_key("nomic-embed-text", 768, "hello");
        let b = embedding_cache_key("nomic-embed-code", 768, "hello");
        let c = embedding_cache_key("nomic-embed-text", 3584, "hello");
        let d = embedding_cache_key("nomic-embed-text", 768, "hello");
        // Model and dim are part of the key: switching either never reads a
        // stale vector.
        assert_ne!(a, b);
        assert_ne!(a, c);
        // Same (model, dim, text) → same key.
        assert_eq!(a, d);
        // Shape: `{model}:{dim}:{64 hex chars}`.
        let parts: Vec<&str> = a.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "nomic-embed-text");
        assert_eq!(parts[1], "768");
        assert_eq!(parts[2].len(), 64);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }
}