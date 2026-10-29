//! Cost accounting over the typed executor.
//!
//! The search crate carries the same adapter privately; this crate needs its
//! own because it builds a tracker for `system:memory_consolidation` and
//! `system:optimizer_raft` usage rows.

use std::sync::Arc;

use async_trait::async_trait;

use threski_ai::{AiError, CostStore};
use threski_db::executor::Executor;

/// `tokens_used +=` adapter over the executor (D17 R6: the `+=` counter idiom
/// replaces the `UPDATE <id> SET tokens_used = (tokens_used OR 0) + n).
pub struct DbCostStore(pub Arc<dyn Executor>);

#[async_trait]
impl CostStore for DbCostStore {
    async fn increment_tokens(&self, record_id: &str, total: u64) -> Result<(), AiError> {
        if !threski_db::ids::is_safe_record_id(record_id) {
            return Ok(());
        }
        let sql = format!("UPDATE {record_id} SET tokens_used += {total};");
        self.0
            .execute(&sql)
            .await
            .map_err(|e| AiError::other(e.to_string()))?;
        Ok(())
    }
}
