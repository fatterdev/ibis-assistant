//! Memory insights: lightweight clustering over
//! typed memories by kind + primary tag, optionally refined with stored
//! embedding similarity. Designed to stay cheap: no re-embedding of every
//! row; AI labeling is opt-in and local-first.

use super::{
    bind, memory_visibility_filter, repo_record_id, temporal::memory_status_filter_sql, Memory,
    ES_HAS_MEMORY, TABLE_MEMORY,
};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

pub(crate) const DEFAULT_INSIGHT_LIMIT: usize = 200;
pub(crate) const DEFAULT_EMBEDDING_THRESHOLD: f64 = 0.82;

/// Controls lightweight clustering over typed memories
///
#[derive(Debug, Clone, Default)]
pub struct MemoryInsightsParams {
    pub project_name: String,
    pub kinds: Vec<String>,
    pub status: String,
    pub limit: usize,
    pub actor: String,
    /// Enables a second pass that merges same-kind clusters when members are
    /// close in embedding space (local EmbedText not required — stored
    /// embeddings are reused).
    pub use_embeddings: bool,
    /// Optionally asks the AI (bulk_local) for a short cluster label.
    pub label_with_ai: bool,
}

/// One grouped insight over related memories.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MemoryInsightCluster {
    pub key: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    pub label: String,
    pub member_ids: Vec<String>,
    pub member_count: usize,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub sample: String,
}

/// The MCP/API payload for memory_insights.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MemoryInsightsResult {
    #[serde(
        rename = "project_name",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub project_name: String,
    pub clusters: Vec<MemoryInsightCluster>,
    #[serde(rename = "total_memories")]
    pub total: usize,
}

pub(crate) struct InsightRow {
    memory: Memory,
    embedding: Vec<f64>,
}

impl SearchService {
    /// Groups current memories by kind + primary tag, optionally refining
    /// with stored-embedding similarity.
    pub async fn memory_insights(
        &self,
        actor_ctx: &crate::ActorContext,
        params: &MemoryInsightsParams,
    ) -> Result<MemoryInsightsResult> {
        let db = self.require_db()?;
        let mut limit = params.limit;
        if limit == 0 {
            limit = DEFAULT_INSIGHT_LIMIT;
        }
        let limit = limit.min(1000);
        let mut status = params.status.trim().to_string();
        if status.is_empty() {
            status = "current".to_string();
        }

        let actor = actor_ctx.resolve_trusted_actor(&params.actor);
        let (vis_filter, mut bindings) = memory_visibility_filter("", &actor)?;
        let mut where_parts = vec![
            {
                let (sql, more) = memory_status_filter_sql(&status);
                bindings.extend(more);
                sql
            },
            vis_filter,
        ];
        let clean_kinds = super::non_empty_strings(&params.kinds);
        if !clean_kinds.is_empty() {
            where_parts.push("kind IN $kinds".to_string());
            bindings.push((
                "kinds".into(),
                ibis_db::value::Value::from_t(
                    clean_kinds
                        .iter()
                        .map(|k| ibis_db::value::Value::from_t(k.as_str()))
                        .collect::<Vec<ibis_db::value::Value>>(),
                ),
            ));
        }
        let project = params.project_name.trim();
        if !project.is_empty() {
            let repo_id = repo_record_id(project);
            where_parts.push(format!(
                "(project = {repo_id} OR id IN {repo_id}->{ES_HAS_MEMORY}->{TABLE_MEMORY})"
            ));
        }

        let sql = format!(
            "SELECT id, kind, title, string::slice(content, 0, 200) AS content, tags, embedding FROM {TABLE_MEMORY} WHERE {} ORDER BY updated_at DESC LIMIT {limit};",
            where_parts.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("memory insights: {e}")))?;

        let mut insight_rows: Vec<InsightRow> = Vec::new();
        for row in rows::object_rows(&rows::normalize_rows(&outcome).unwrap_or_default()) {
            let mem = super::memory_from_row(row);
            if mem.id.is_empty() {
                continue;
            }
            insight_rows.push(InsightRow {
                memory: mem,
                embedding: float_slice(rows::field(row, "embedding")),
            });
        }

        // Pass 1: bucket by kind + primary tag (or "untagged").
        struct Bucket {
            kind: String,
            tag: String,
            rows: Vec<InsightRow>,
        }
        let mut bucket_map: std::collections::HashMap<String, Bucket> =
            std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for row in insight_rows {
            let tag = row
                .memory
                .tags
                .first()
                .cloned()
                .unwrap_or_else(|| "untagged".to_string());
            let key = format!("{}|{}", row.memory.kind, tag);
            let entry = bucket_map.entry(key.clone()).or_insert_with(|| Bucket {
                kind: row.memory.kind.clone(),
                tag: tag.clone(),
                rows: Vec::new(),
            });
            entry.rows.push(row);
            if !order.contains(&key) {
                order.push(key);
            }
        }

        let mut clusters: Vec<MemoryInsightCluster> = Vec::with_capacity(order.len());
        for key in &order {
            let b = bucket_map.get(key).expect("bucket exists");
            if params.use_embeddings {
                for sub in split_by_embedding(&b.rows, DEFAULT_EMBEDDING_THRESHOLD) {
                    clusters.push(cluster_from_rows(&b.kind, &b.tag, &sub));
                }
            } else {
                let all: Vec<&InsightRow> = b.rows.iter().collect();
                clusters.push(cluster_from_rows(&b.kind, &b.tag, &all));
            }
        }

        clusters.sort_by(|a, b| {
            b.member_count
                .cmp(&a.member_count)
                .then_with(|| a.key.cmp(&b.key))
        });

        if params.label_with_ai {
            if let Some(ai) = &self.ai {
                for cluster in clusters.iter_mut() {
                    if let Ok(label) = self.label_insight_cluster(ai.as_ref(), cluster).await {
                        if !label.is_empty() {
                            cluster.label = label;
                        }
                    }
                }
            }
        }

        Ok(MemoryInsightsResult {
            project_name: project.to_string(),
            clusters,
            total: bucket_map.values().map(|b| b.rows.len()).sum(),
        })
    }

    async fn label_insight_cluster(
        &self,
        ai: &dyn ibis_ai::AIProvider,
        cluster: &MemoryInsightCluster,
    ) -> Result<String> {
        let prompt = format!(
            "Label this memory cluster in at most 6 words. Kind={} tags={} sample={:?} members={}. Reply with the label only.",
            cluster.kind,
            cluster.tags.join(","),
            cluster.sample,
            cluster.member_count,
        );
        let candidate = ai
            .generate_content(
                &[ibis_ai::types::Content {
                    role: "user".to_string(),
                    parts: vec![ibis_ai::types::Part { text: prompt }],
                }],
                ibis_ai::types::GenerationConfig {
                    route_hint: "bulk_local".to_string(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| SearchError::other(format!("{e}")))?;
        if candidate.content.parts.is_empty() {
            return Err(SearchError::other("empty AI label"));
        }
        let mut label = candidate.content.parts[0].text.trim().to_string();
        label = label
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_string();
        if let Some(idx) = label.find('\n') {
            label = label[..idx].trim().to_string();
        }
        let runes: Vec<char> = label.chars().collect();
        if runes.len() > 60 {
            label = runes[..60].iter().collect();
        }
        Ok(label)
    }
}

/// `clusterFromRows.
fn cluster_from_rows(kind: &str, tag: &str, rows: &[&InsightRow]) -> MemoryInsightCluster {
    let mut ids = Vec::with_capacity(rows.len());
    let tags: Vec<String> = if !tag.is_empty() && tag != "untagged" {
        vec![tag.to_string()]
    } else {
        Vec::new()
    };
    let mut sample = String::new();
    let mut label = kind.to_string();
    if !tag.is_empty() && tag != "untagged" {
        label = format!("{kind} / {tag}");
    }
    for (i, row) in rows.iter().enumerate() {
        ids.push(row.memory.id.clone());
        if i == 0 {
            sample = if !row.memory.title.is_empty() {
                row.memory.title.clone()
            } else {
                super::shorten_memory_content(&row.memory.content)
            };
        }
    }
    MemoryInsightCluster {
        key: format!("{kind}|{tag}"),
        kind: kind.to_string(),
        tags,
        label,
        member_ids: ids,
        member_count: rows.len(),
        sample,
    }
}

/// Greedily clusters rows whose cosine similarity is above threshold. Rows
/// without embeddings stay singleton clusters.
pub(crate) fn split_by_embedding(rows: &[InsightRow], threshold: f64) -> Vec<Vec<&InsightRow>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut used = vec![false; rows.len()];
    let mut groups: Vec<Vec<&InsightRow>> = Vec::new();
    for i in 0..rows.len() {
        if used[i] {
            continue;
        }
        let mut group = vec![&rows[i]];
        used[i] = true;
        if rows[i].embedding.is_empty() {
            groups.push(group);
            continue;
        }
        for j in i + 1..rows.len() {
            if used[j] || rows[j].embedding.is_empty() {
                continue;
            }
            if cosine_similarity(&rows[i].embedding, &rows[j].embedding) >= threshold {
                group.push(&rows[j]);
                used[j] = true;
            }
        }
        groups.push(group);
    }
    groups
}

/// `cosineSimilarity (min-length truncation, zero-norm guard).
pub(crate) fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let mut n = a.len();
    if b.len() < n {
        n = b.len();
    }
    if n == 0 {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// `floatSliceFromAny: numeric arrays → Vec<f64> (other shapes → empty).
pub(crate) fn float_slice(value: &ibis_db::value::Value) -> Vec<f64> {
    match value {
        ibis_db::value::Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                ibis_db::value::Value::Number(n) => n.to_f64(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}
