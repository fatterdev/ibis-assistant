//! Cost tracker: increments `tokens_used` on the
//! related entity and logs a blended-cost debug line.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::AiError;
use crate::types::estimate_tokens;

/// Persistence seam for token accounting. SQL
/// contract for the db-lane adapter:
/// `UPDATE <record_id> SET tokens_used = (tokens_used OR 0) + <total>;`
#[async_trait]
pub trait CostStore: Send + Sync {
    async fn increment_tokens(&self, record_id: &str, total: u64) -> Result<(), AiError>;
}

pub struct CostTracker {
    store: Option<Arc<dyn CostStore>>,
}

impl CostTracker {
    pub fn new(store: Option<Arc<dyn CostStore>>) -> Self {
        CostTracker { store }
    }

    pub async fn record_usage(
        &self,
        related_entity_id: &str,
        prompt_tokens: u64,
        candidates_tokens: u64,
    ) {
        let total = prompt_tokens + candidates_tokens;
        if total == 0 {
            return;
        }
        if let Some(store) = &self.store {
            if let Err(e) = store.increment_tokens(related_entity_id, total).await {
                tracing::error!("Failed to record token usage on {related_entity_id}: {e}");
            }
        }
        // Blended $0.15 per 1M tokens (input+output simplification,
        // ).
        let cost = total as f64 / 1_000_000.0 * 0.15;
        if cost > 0.0001 {
            tracing::debug!(
                "Tokens used: {} (Cost: ~${cost:.5}) for {}",
                total,
                related_entity_id
            );
        }
    }

    /// Estimates tokens for an embedding request (same `len/3` heuristic as
    /// the rate limiter) and records it.
    pub async fn record_embedding_usage(&self, related_entity_id: &str, text: &str) {
        self.record_usage(related_entity_id, estimate_tokens(text) as u64, 0)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeStore {
        calls: AtomicU64,
        last: std::sync::Mutex<(String, u64)>,
        fail: bool,
    }

    impl FakeStore {
        fn new(fail: bool) -> Self {
            FakeStore {
                calls: AtomicU64::new(0),
                last: std::sync::Mutex::new((String::new(), 0)),
                fail,
            }
        }
    }

    #[async_trait]
    impl CostStore for FakeStore {
        async fn increment_tokens(&self, record_id: &str, total: u64) -> Result<(), AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = (record_id.to_string(), total);
            if self.fail {
                Err(AiError::other("db down"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn records_total_tokens() {
        let store = Arc::new(FakeStore::new(false));
        let tracker = CostTracker::new(Some(store.clone() as Arc<dyn CostStore>));
        tracker.record_usage("system:search_agentic", 120, 80).await;
        assert_eq!(store.calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.last.lock().unwrap().0, "system:search_agentic");
        assert_eq!(store.last.lock().unwrap().1, 200);
    }

    #[tokio::test]
    async fn zero_total_is_skipped() {
        let store = Arc::new(FakeStore::new(false));
        let tracker = CostTracker::new(Some(store.clone() as Arc<dyn CostStore>));
        tracker.record_usage("x", 0, 0).await;
        assert_eq!(store.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn store_failure_is_logged_not_raised() {
        let store = Arc::new(FakeStore::new(true));
        let tracker = CostTracker::new(Some(store.clone() as Arc<dyn CostStore>));
        tracker.record_usage("x", 5, 5).await;
        assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn embedding_usage_uses_estimate() {
        let store = Arc::new(FakeStore::new(false));
        let tracker = CostTracker::new(Some(store.clone() as Arc<dyn CostStore>));
        tracker
            .record_embedding_usage("system:search_vector", "aaaaaa")
            .await; // 6/3+1 = 3
        assert_eq!(store.last.lock().unwrap().1, 3);
        tracker
            .record_embedding_usage("system:search_vector", "")
            .await;
        assert_eq!(store.calls.load(Ordering::SeqCst), 1); // estimate 0 → skipped
    }

    #[tokio::test]
    async fn nil_store_is_noop() {
        let tracker = CostTracker::new(None);
        tracker.record_usage("x", 10, 10).await;
        tracker.record_embedding_usage("x", "text").await;
    }
}
