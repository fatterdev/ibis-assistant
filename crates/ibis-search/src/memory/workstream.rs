//! Workstream browse (F4 §A.3): the read-only surface behind the
//! `memory_workstreams` tool and the `memory_list(workstream=…)` filter.
//! Workstream rows live on the `workstream` table and memories attach via
//! the `part_of_workstream` edge (`in` = memory, `out` = workstream); no
//! writer exists yet, so an empty store degrades to an empty browse.

use std::collections::HashMap;

use super::{bind, require_memory_record_id, ES_PART_OF_WORKSTREAM};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// One workstream browse row: the workstream plus how many of its member
/// memories are current (the number the Workstreams view shows on the root).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkstreamSummary {
    pub id: String,
    pub name: String,
    pub status: String,
    pub current_count: usize,
}

impl SearchService {
    /// Browse rows for the Workstreams view roots: every `workstream` row
    /// with its current-member count, ordered by most recently touched
    /// (max member `updated_at` through the edge; untouched rows last).
    /// Counts honor the caller's visibility (team + own personal).
    pub async fn list_workstreams(
        &self,
        actor_ctx: &crate::ActorContext,
    ) -> Result<Vec<WorkstreamSummary>> {
        let t0 = std::time::Instant::now();
        let db = self.require_db()?;
        let outcome = db
            .execute("SELECT id, name, status FROM workstream;")
            .await
            .map_err(|e| SearchError::other(format!("list workstreams: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            return Ok(Vec::new());
        };
        let mut workstreams: Vec<WorkstreamSummary> = Vec::new();
        for row in rows::object_rows(&rows_values) {
            workstreams.push(WorkstreamSummary {
                id: rows::record_id(rows::field(row, "id")),
                name: rows::scalar_string(rows::field(row, "name")),
                status: rows::scalar_string(rows::field(row, "status")),
                current_count: 0,
            });
        }
        if workstreams.is_empty() {
            tracing::info!("memory_workstreams: done in {:?} workstreams=0", t0.elapsed());
            return Ok(workstreams);
        }

        // Members: one edge row per attached memory (`in` = memory, `out` =
        // workstream). Malformed rows are skipped (graph robustness), the
        // memory ids are validated before they reach interpolated SQL.
        let edge_outcome = db
            .execute(&format!("SELECT in, out FROM {ES_PART_OF_WORKSTREAM};"))
            .await
            .map_err(|e| SearchError::other(format!("list workstream members: {e}")))?;
        let mut members_of: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(edge_rows) = rows::normalize_rows(&edge_outcome) {
            for row in rows::object_rows(&edge_rows) {
                let memory_id = rows::record_id(rows::field(row, "in"));
                let workstream_id = rows::record_id(rows::field(row, "out"));
                if workstream_id.is_empty() || require_memory_record_id(&memory_id).is_err() {
                    continue;
                }
                members_of
                    .entry(memory_id)
                    .or_default()
                    .push(workstream_id);
            }
        }
        let member_ids: Vec<String> = members_of
            .keys()
            .filter(|id| !id.is_empty())
            .cloned()
            .collect();
        if member_ids.is_empty() {
            workstreams.sort_by(|a, b| a.id.cmp(&b.id));
            tracing::info!(
                "memory_workstreams: done in {:?} workstreams={}",
                t0.elapsed(),
                workstreams.len()
            );
            return Ok(workstreams);
        }
        let id_list = member_ids
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        let actor = actor_ctx.resolve_trusted_actor("");
        let (vis_filter, vis_bindings) = super::memory_visibility_filter("", &actor)?;
        let vis = bind(vis_bindings);

        // Current-member counts: same currentness predicate as the tree's
        // `current` browse, so the root count matches the visible children.
        let currentness = super::temporal::memory_currentness_sql();
        let count_sql = format!(
            "SELECT id FROM memory WHERE id IN [{id_list}] AND {vis_filter} AND {currentness};"
        );
        let count_outcome = db
            .smart_query(&count_sql, vis.clone())
            .await
            .map_err(|e| SearchError::other(format!("count workstream members: {e}")))?;
        let mut counts: HashMap<String, usize> = HashMap::new();
        if let Some(count_rows) = rows::normalize_rows(&count_outcome) {
            for row in rows::object_rows(&count_rows) {
                let memory_id = rows::record_id(rows::field(row, "id"));
                if let Some(targets) = members_of.get(&memory_id) {
                    for workstream_id in targets {
                        *counts.entry(workstream_id.clone()).or_default() += 1;
                    }
                }
            }
        }

        // Touch ordering: latest member update wins, superseded members too.
        let touch_sql = format!(
            "SELECT id, updated_at FROM memory WHERE id IN [{id_list}] AND {vis_filter};"
        );
        let touch_outcome = db
            .smart_query(&touch_sql, vis)
            .await
            .map_err(|e| SearchError::other(format!("touch workstream members: {e}")))?;
        let mut last_touched: HashMap<String, String> = HashMap::new();
        if let Some(touch_rows) = rows::normalize_rows(&touch_outcome) {
            for row in rows::object_rows(&touch_rows) {
                let memory_id = rows::record_id(rows::field(row, "id"));
                let updated_at = rows::scalar_string(rows::field(row, "updated_at"));
                let Some(targets) = members_of.get(&memory_id) else {
                    continue;
                };
                for workstream_id in targets {
                    let entry = last_touched.entry(workstream_id.clone()).or_default();
                    if updated_at.as_str() > entry.as_str() {
                        *entry = updated_at.clone();
                    }
                }
            }
        }

        for workstream in &mut workstreams {
            workstream.current_count = counts.get(&workstream.id).copied().unwrap_or(0);
        }
        workstreams.sort_by(|a, b| {
            let a_touched = last_touched.get(&a.id).map(String::as_str).unwrap_or("");
            let b_touched = last_touched.get(&b.id).map(String::as_str).unwrap_or("");
            b_touched.cmp(a_touched).then_with(|| a.id.cmp(&b.id))
        });
        tracing::info!(
            "memory_workstreams: done in {:?} workstreams={}",
            t0.elapsed(),
            workstreams.len()
        );
        Ok(workstreams)
    }

    /// Resolves the `memory_list(workstream=…)` argument (name or id) to a
    /// validated `workstream:…` record id. Explicit ids pass through
    /// (normalized, table-checked); names resolve via a bound case-insensitive
    /// lookup, and an unknown name errors instead of silently returning zero
    /// rows.
    pub(crate) async fn resolve_workstream_id(&self, arg: &str) -> Result<String> {
        let arg = arg.trim();
        if ibis_db::ids::is_safe_record_id(arg) && arg.split(':').next() == Some("workstream") {
            return Ok(ibis_db::ids::format_record_id("workstream", arg));
        }
        if arg.is_empty() {
            return Err(SearchError::other("workstream name or id is required"));
        }
        let db = self.require_db()?;
        let lowered = arg.to_lowercase();
        let sql =
            "SELECT id FROM workstream WHERE string::lowercase(name OR '') = $ws_name LIMIT 1;";
        let outcome = db
            .smart_query(
                sql,
                bind(vec![(
                    "ws_name".into(),
                    ibis_db::value::Value::from_t(lowered),
                )]),
            )
            .await
            .map_err(|e| SearchError::other(format!("resolve workstream: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            return Err(SearchError::other(format!("workstream not found: {arg:?}")));
        };
        let id = rows::object_rows(&rows_values)
            .first()
            .map(|row| rows::record_id(rows::field(row, "id")))
            .unwrap_or_default();
        if id.is_empty() {
            return Err(SearchError::other(format!("workstream not found: {arg:?}")));
        }
        if !ibis_db::ids::is_safe_record_id(&id)
            || id.split(':').next() != Some("workstream")
        {
            return Err(SearchError::other(
                "workstream lookup returned an invalid id",
            ));
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_ai, obj, rows_of, FakeDb};
    use ibis_db::value::Value;

    fn workstream_service() -> (std::sync::Arc<FakeDb>, SearchService) {
        let fake = std::sync::Arc::new(FakeDb::new(vec![
            (
                "FROM workstream",
                rows_of(vec![
                    obj(vec![
                        ("id", Value::from_t("workstream:⟨alpha⟩")),
                        ("name", Value::from_t("alpha")),
                        ("status", Value::from_t("active")),
                    ]),
                    obj(vec![
                        ("id", Value::from_t("workstream:⟨beta⟩")),
                        ("name", Value::from_t("beta")),
                        ("status", Value::from_t("active")),
                    ]),
                ]),
            ),
            (
                "FROM part_of_workstream",
                rows_of(vec![
                    obj(vec![
                        ("in", Value::from_t("memory:⟨mem_1⟩")),
                        ("out", Value::from_t("workstream:⟨alpha⟩")),
                    ]),
                    obj(vec![
                        ("in", Value::from_t("memory:⟨mem_2⟩")),
                        ("out", Value::from_t("workstream:⟨alpha⟩")),
                    ]),
                    obj(vec![
                        ("in", Value::from_t("memory:⟨mem_3⟩")),
                        ("out", Value::from_t("workstream:⟨beta⟩")),
                    ]),
                ]),
            ),
            // The current-member count query carries the currentness
            // predicate; the fake keys on it so only current rows come back.
            (
                "valid_until > time::now()",
                rows_of(vec![
                    obj(vec![("id", Value::from_t("memory:⟨mem_1⟩"))]),
                    obj(vec![("id", Value::from_t("memory:⟨mem_3⟩"))]),
                ]),
            ),
            // The touch query matches the plain member fetch: every member.
            (
                "FROM memory",
                rows_of(vec![
                    obj(vec![
                        ("id", Value::from_t("memory:⟨mem_1⟩")),
                        ("updated_at", Value::from_t("2026-11-03T10:00:00Z")),
                    ]),
                    obj(vec![
                        ("id", Value::from_t("memory:⟨mem_2⟩")),
                        ("updated_at", Value::from_t("2026-11-02T10:00:00Z")),
                    ]),
                    obj(vec![
                        ("id", Value::from_t("memory:⟨mem_3⟩")),
                        ("updated_at", Value::from_t("2026-11-01T10:00:00Z")),
                    ]),
                ]),
            ),
        ]));
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        (fake, svc)
    }

    /// F4 §A.3 contract: rows carry the current-member count and order by
    /// most recently touched (superseded members keep the parent warm).
    #[tokio::test]
    async fn list_workstreams_counts_current_members_and_orders_by_touch() {
        let (fake, svc) = workstream_service();
        let rows = svc
            .list_workstreams(&crate::ActorContext::LOCAL)
            .await
            .expect("workstream browse");
        assert_eq!(
            rows,
            vec![
                WorkstreamSummary {
                    id: "workstream:⟨alpha⟩".into(),
                    name: "alpha".into(),
                    status: "active".into(),
                    current_count: 1,
                },
                WorkstreamSummary {
                    id: "workstream:⟨beta⟩".into(),
                    name: "beta".into(),
                    status: "active".into(),
                    current_count: 1,
                },
            ],
            "alpha touched most recently (2026-11-03 via a superseded member) leads"
        );

        let calls = fake.calls();
        let count_sql = calls
            .iter()
            .find(|q| q.contains("valid_until > time::now()"))
            .expect("currentness count query");
        assert!(
            count_sql.contains("SELECT id FROM memory WHERE id IN ["),
            "counts filter members by id: {count_sql}"
        );
        let touch_sql = calls
            .iter()
            .find(|q| q.contains("FROM memory") && !q.contains("valid_until"))
            .expect("touch query");
        assert!(
            touch_sql.contains("updated_at"),
            "touch ordering needs member timestamps: {touch_sql}"
        );
    }

    /// An empty store (no writer yet) degrades to an empty browse without
    /// touching the edge or member queries.
    #[tokio::test]
    async fn list_workstreams_empty_store_returns_empty_without_member_queries() {
        let fake = std::sync::Arc::new(FakeDb::new(vec![(
            "FROM workstream",
            rows_of(vec![]),
        )]));
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        let rows = svc
            .list_workstreams(&crate::ActorContext::LOCAL)
            .await
            .expect("empty browse");
        assert!(rows.is_empty());
        assert_eq!(fake.calls().len(), 1, "only the workstream list query ran");
    }

    /// Degraded mode (no DB) fails closed like every other DB read.
    #[tokio::test]
    async fn list_workstreams_without_db_fails_closed() {
        let svc = SearchService::default();
        let err = svc
            .list_workstreams(&crate::ActorContext::LOCAL)
            .await
            .expect_err("database not connected");
        assert!(matches!(err, crate::error::SearchError::DatabaseNotConnected));
    }

    /// The name lookup binds the lowercased name (F-002) and the returned
    /// record id is table-checked before interpolation.
    #[tokio::test]
    async fn resolve_workstream_id_by_name_binds_the_lookup() {
        let fake = std::sync::Arc::new(FakeDb::new(vec![(
            "FROM workstream",
            rows_of(vec![obj(vec![(
                "id",
                Value::from_t("workstream:⟨auth_refactor⟩"),
            )])]),
        )]));
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        let id = svc
            .resolve_workstream_id("Auth Refactor")
            .await
            .expect("name resolves");
        assert_eq!(id, "workstream:⟨auth_refactor⟩");

        let sql = fake.smart_calls.lock().unwrap()[0].clone();
        assert!(
            sql.contains("string::lowercase(name OR '') = $ws_name"),
            "lookup must match the name case-insensitively: {sql}"
        );
        let vars = fake.smart_vars.lock().unwrap()[0].clone();
        assert_eq!(
            vars.get("ws_name"),
            Some(&Value::from_t("auth refactor")),
            "user value rides a bound variable"
        );
    }

    /// An unknown name errors (browse roots need the feedback) instead of
    /// silently filtering to zero rows.
    #[tokio::test]
    async fn resolve_workstream_id_unknown_name_errors() {
        let fake = std::sync::Arc::new(FakeDb::new(vec![(
            "FROM workstream",
            rows_of(vec![]),
        )]));
        let svc = SearchService::new(Some(fake), Some(fake_ai()));
        let err = svc
            .resolve_workstream_id("nope")
            .await
            .expect_err("unknown workstream");
        assert!(
            err.to_string().contains("workstream not found"),
            "error names the miss: {err}"
        );
    }

    /// Explicit ids (wrapped or bare) normalize to the canonical wrapped
    /// form without a lookup query.
    #[tokio::test]
    async fn resolve_workstream_id_passes_ids_through_normalized() {
        let fake = std::sync::Arc::new(FakeDb::default());
        let svc = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        for arg in ["workstream:⟨auth⟩", "workstream:auth"] {
            let id = svc.resolve_workstream_id(arg).await.expect("id accepted");
            assert_eq!(id, "workstream:⟨auth⟩", "bare keys wrap: {arg}");
        }
        assert!(
            fake.calls().is_empty(),
            "id args must not pay a lookup query"
        );
    }
}