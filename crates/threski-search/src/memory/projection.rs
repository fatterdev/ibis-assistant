//! `.threski/memory` projection: regenerate
//! markdown files per memory kind from SurrealDB. The projection output is
//! fully regenerable and never read back automatically — `memory_import` is
//! the only path that reads memory data back into SurrealDB (and only when a
//! human explicitly runs it).

use std::path::Path;

use super::first_non_empty;

use crate::error::Result;
use crate::search::SearchService;

const MEMORY_PROJECTION_GITIGNORE: &str = "observation/\nworking_state/\n";

/// Regenerates `<repoRoot>/.threski/memory/<kind>/*.md` from SurrealDB
/// SurrealDB is always the source of truth.
pub async fn write_memory_projection(
    service: &SearchService,
    repo_root: &str,
    project_name: &str,
) -> Result<()> {
    let root = Path::new(repo_root).join(".threski").join("memory");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| crate::SearchError::other(format!("create memory projection dir: {e}")))?;
    tokio::fs::write(root.join(".gitignore"), MEMORY_PROJECTION_GITIGNORE)
        .await
        .map_err(|e| crate::SearchError::other(format!("write projection .gitignore: {e}")))?;

    // The projection is regenerable and often git-tracked: team memories only.
    let items = service
        .memory_export(
            &crate::ActorContext::LOCAL,
            &super::export::MemoryExportParams {
                project_name: project_name.to_string(),
                status: "current".to_string(),
                visibility: "team".to_string(),
                ..Default::default()
            },
        )
        .await?
        .0;

    for m in &items {
        let kind_dir = root.join(safe_dir_name(&m.kind));
        tokio::fs::create_dir_all(&kind_dir)
            .await
            .map_err(|e| crate::SearchError::other(format!("create kind dir: {e}")))?;
        let slug = safe_dir_name(&first_non_empty(&[&m.title, &m.id]));
        let id_suffix = m.id.strip_prefix("memory:").unwrap_or(&m.id);
        let path = kind_dir.join(format!("{}-{}.md", slug, safe_dir_name(id_suffix)));
        let body = format!(
            "# {}\n\n- kind: {}\n- status: {}\n- id: {}\n\n{}\n",
            first_non_empty(&[&m.title, &m.id]),
            m.kind,
            m.status,
            m.id,
            m.content
        );
        tokio::fs::write(path, body)
            .await
            .map_err(|e| crate::SearchError::other(format!("write projection file: {e}")))?;
    }
    Ok(())
}

/// `safeDirName with one deliberate improvement (D15/D17): runs of
/// non-alphanumeric characters collapse to a single `-`, so record-id-derived
/// slugs render clean (`memory:⟨rule1⟩` → `memory-rule1`, not `memory--rule1`).
/// The projection is regenerated and never read back, so the shape is ours.
pub(crate) fn safe_dir_name(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut pending_dash = false;
    for r in s.chars() {
        if r.is_ascii_lowercase() || r.is_ascii_digit() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(r);
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        return "untitled".to_string();
    }
    let runes: Vec<char> = out.chars().collect();
    if runes.len() > 60 {
        runes[..60].iter().collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_dir_name_sanitizes() {
        assert_eq!(safe_dir_name("English only"), "english-only");
        assert_eq!(safe_dir_name("memory:⟨rule1⟩"), "memory-rule1");
        assert_eq!(safe_dir_name("  "), "untitled");
        assert_eq!(safe_dir_name(&"x".repeat(80)), "x".repeat(60));
    }
}
