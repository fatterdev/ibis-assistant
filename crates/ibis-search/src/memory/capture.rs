//! Raw observation capture: intentionally small
//! input surface; classification and consolidation happen separately.
//!
//! E15.5 bridge: an optional `citations` list (the `AgenticCitation`
//! evidence of an agentic ask) is rendered into a stable, bounded
//! `EVIDENCE:` block inside the observation `content`. Still one memory row
//! per capture — the evidence is text, not a separate table/edge.

use std::collections::HashSet;

use super::add::AddMemoryParams;
use crate::agentic_trust::{
    bounded_excerpt, AgenticCitation, CAPTURE_EVIDENCE_EXCERPT_RUNES, CAPTURE_EVIDENCE_MAX_ENTRIES,
};
use crate::error::Result;
use crate::search::SearchService;

/// `CaptureObservationParams.
#[derive(Debug, Clone, Default)]
pub struct CaptureObservationParams {
    pub project_name: String,
    pub text: String,
    pub source: String,
    pub session_id: String,
    pub owner: String,
    /// Optional structured evidence from an agentic ask (E15.5). Empty =
    /// byte-identical behavior to a plain text capture.
    pub citations: Vec<AgenticCitation>,
}

/// Parses the optional `citations_json` MCP argument (E15.5): absent/empty
/// → `Vec::new()`; invalid JSON → an `invalid citations_json: ...` error
/// without any partial write.
pub fn parse_citations_json(raw: &str) -> std::result::Result<Vec<AgenticCitation>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(raw).map_err(|e| format!("invalid citations_json: {e}"))
}

/// Renders the capture content: `text` plus the bounded evidence block.
/// Without renderable citations the text is byte-identical (the block never
/// leaves a dangling `EVIDENCE:` header).
pub(crate) fn render_capture_content(text: &str, citations: &[AgenticCitation]) -> String {
    if citations.is_empty() {
        return text.to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut overflow = 0usize;
    for citation in citations {
        // Drop citations with source_id, path and commit_hash all empty:
        // nothing identifies the source.
        if citation.source_id.trim().is_empty()
            && citation.path.trim().is_empty()
            && citation.commit_hash.trim().is_empty()
        {
            continue;
        }
        // Same dedup key as the citation catalog: source_id when non-empty,
        // else source_kind|path|commit_hash.
        let key = if !citation.source_id.trim().is_empty() {
            citation.source_id.trim().to_string()
        } else {
            format!(
                "{}|{}|{}",
                citation.source_kind, citation.path, citation.commit_hash
            )
        };
        if !seen.insert(key) {
            continue;
        }
        if lines.len() >= CAPTURE_EVIDENCE_MAX_ENTRIES {
            overflow += 1;
            continue;
        }
        lines.push(render_evidence_line(citation));
    }
    if overflow > 0 {
        tracing::warn!(
            "capture_observation: {overflow} citation(s) beyond the {CAPTURE_EVIDENCE_MAX_ENTRIES}-entry EVIDENCE cap"
        );
    }
    if lines.is_empty() {
        return text.to_string();
    }
    let mut content =
        String::with_capacity(text.len() + 16 + lines.iter().map(|l| l.len() + 1).sum::<usize>());
    content.push_str(text);
    content.push_str("\n\nEVIDENCE:\n");
    content.push_str(&lines.join("\n"));
    content
}

/// One stable evidence row: `- [kind] path @ commit score=… id=… excerpt="…"`
/// with empty fields omitted (`citation_id` is ask-local and never saved).
fn render_evidence_line(citation: &AgenticCitation) -> String {
    let mut line = String::from("- [");
    line.push_str(&escape_evidence_field(
        if citation.source_kind.trim().is_empty() {
            "unknown"
        } else {
            citation.source_kind.trim()
        },
    ));
    line.push(']');
    if !citation.path.trim().is_empty() {
        line.push(' ');
        line.push_str(&escape_evidence_field(citation.path.trim()));
    }
    if !citation.commit_hash.trim().is_empty() {
        line.push_str(" @ ");
        line.push_str(&escape_evidence_field(citation.commit_hash.trim()));
    }
    let score = if citation.score.is_finite() {
        citation.score
    } else {
        0.0
    };
    line.push_str(&format!(" score={score:.2}"));
    if !citation.source_id.trim().is_empty() {
        line.push_str(" id=");
        line.push_str(&escape_evidence_field(citation.source_id.trim()));
    }
    let excerpt = bounded_excerpt(&citation.excerpt, CAPTURE_EVIDENCE_EXCERPT_RUNES);
    if !excerpt.is_empty() {
        line.push_str(" excerpt=\"");
        line.push_str(&escape_evidence_field(&excerpt));
        line.push('"');
    }
    line
}

/// Escapes a value interpolated into the single-line EVIDENCE row so it can
/// never break the stable block shape: backslash, double quote and control
/// characters are escaped; everything else is kept verbatim.
fn escape_evidence_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

impl SearchService {
    /// `CaptureObservation: records a raw, reusable observation with
    /// fixed lifecycle, visibility, confidence, and provenance fields.
    pub async fn capture_observation(
        &self,
        actor_ctx: &crate::ActorContext,
        params: &CaptureObservationParams,
    ) -> Result<String> {
        let mut source = params.source.trim().to_lowercase();
        if source.is_empty() {
            source = "agent".to_string();
        }
        match source.as_str() {
            "agent" | "tool" | "user" => {}
            other => {
                return Err(crate::error::SearchError::other(format!(
                    "observation source must be agent, tool, or user, got {other:?}"
                )));
            }
        }

        let confidence = 0.5;
        self.add_memory(
            actor_ctx,
            &mut AddMemoryParams {
                project_name: params.project_name.clone(),
                kind: "observation".to_string(),
                content: render_capture_content(&params.text, &params.citations),
                status: "current".to_string(),
                visibility: "team".to_string(),
                confidence: Some(confidence),
                owner: params.owner.clone(),
                provenance_agent: source,
                provenance_session: params.session_id.clone(),
                provenance_tool: "capture_observation".to_string(),
                ..Default::default()
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_ai, FakeDb};
    use std::sync::Arc;

    fn citation(
        source_id: &str,
        kind: &str,
        path: &str,
        commit: &str,
        score: f64,
        excerpt: &str,
    ) -> AgenticCitation {
        AgenticCitation {
            citation_id: "S1".to_string(),
            source_id: source_id.to_string(),
            source_kind: kind.to_string(),
            path: path.to_string(),
            commit_hash: commit.to_string(),
            score,
            excerpt: excerpt.to_string(),
        }
    }

    #[test]
    fn capture_observation_renders_evidence_block() {
        let citations = vec![
            citation(
                "file_chunk:abc",
                "file_chunk",
                "src/retry.rs",
                "abc1234",
                0.87,
                "fn retry(...)",
            ),
            citation("memory:mem_1", "memory", "", "", 0.72, "team truth"),
        ];
        let content = render_capture_content("observed the retry loop", &citations);
        assert!(content.starts_with("observed the retry loop\n\nEVIDENCE:\n"));
        assert!(content.contains("EVIDENCE:"));
        for citation in &citations {
            assert!(content.contains(&citation.source_id), "{content}");
        }
        assert!(content.contains("src/retry.rs @ abc1234 score=0.87"));
        assert!(content.contains("excerpt=\"fn retry(...)\""));
        assert!(content.contains("- [memory] score=0.72 id=memory:mem_1 excerpt=\"team truth\""));
        // citation_id is ask-local: never saved.
        assert!(!content.contains("[S1]"), "{content}");
    }

    #[test]
    fn capture_observation_without_citations_is_unchanged() {
        assert_eq!(render_capture_content("plain text", &[]), "plain text");
        assert_eq!(
            render_capture_content("plain text", &[AgenticCitation::default()]),
            "plain text",
            "unidentifiable citations leave the text byte-identical"
        );
    }

    #[test]
    fn capture_observation_rejects_invalid_citations_json() {
        assert!(parse_citations_json("").expect("empty is fine").is_empty());
        assert!(parse_citations_json("   ")
            .expect("blank is fine")
            .is_empty());
        let err = parse_citations_json("{not json").expect_err("broken JSON rejected");
        assert!(err.starts_with("invalid citations_json:"), "{err}");
    }

    #[test]
    fn capture_observation_caps_evidence_at_20() {
        let citations: Vec<AgenticCitation> = (1..=25)
            .map(|i| {
                citation(
                    &format!("file_chunk:{i}"),
                    "file_chunk",
                    &format!("src/f{i}.rs"),
                    "",
                    0.5,
                    "e",
                )
            })
            .collect();
        let content = render_capture_content("text", &citations);
        assert_eq!(content.matches("- [").count(), CAPTURE_EVIDENCE_MAX_ENTRIES);
        assert!(content.contains("src/f20.rs"));
        assert!(!content.contains("src/f21.rs"));
    }

    #[tokio::test]
    async fn capture_observation_does_not_create_extra_memory_rows() {
        let fake = Arc::new(FakeDb::default());
        let service = SearchService::new(Some(fake.clone()), Some(fake_ai()));
        let params = CaptureObservationParams {
            project_name: "proj".to_string(),
            text: "observed".to_string(),
            citations: vec![
                citation("file_chunk:a", "file_chunk", "src/a.rs", "", 0.8, "one"),
                citation("memory:m1", "memory", "", "", 0.7, "two"),
            ],
            ..Default::default()
        };
        let id = service
            .capture_observation(&crate::ActorContext::LOCAL, &params)
            .await
            .expect("capture");
        assert!(id.starts_with("memory:"));
        let calls = fake.smart_calls.lock().unwrap().clone();
        let creates = calls
            .iter()
            .filter(|sql| sql.contains("CREATE memory:"))
            .count();
        assert_eq!(creates, 1, "exactly one memory row per capture: {calls:?}");
    }

    #[test]
    fn capture_observation_escapes_evidence_fields() {
        let citations = vec![
            citation(
                "file_chunk:a\"b\\c",
                "file_chunk",
                "src/we\"ird\\path.rs",
                "abc1234",
                0.5,
                "he said \"hi\" and C:\\tmp\\x",
            ),
            citation(
                "file_chunk:tab\tid",
                "file_chunk",
                "src/tab.rs",
                "",
                0.5,
                "e",
            ),
        ];
        let content = render_capture_content("text", &citations);
        assert!(
            content.contains(r#"excerpt="he said \"hi\" and C:\\tmp\\x""#),
            "{content}"
        );
        assert!(content.contains(r#"id=file_chunk:a\"b\\c"#), "{content}");
        assert!(content.contains(r#"src/we\"ird\\path.rs"#), "{content}");
        assert!(
            content.contains(r"id=file_chunk:tab\tid"),
            "control characters are escaped: {content}"
        );
        // The block stays one line per evidence entry.
        let evidence = content.split("EVIDENCE:\n").nth(1).expect("evidence block");
        assert_eq!(evidence.lines().count(), 2, "{content}");
        assert!(!evidence.contains('\t'), "{content}");
    }

    #[test]
    fn capture_observation_evidence_excerpt_is_bounded() {
        let long = "y".repeat(300);
        let citations = vec![citation(
            "file_chunk:a",
            "file_chunk",
            "src/a.rs",
            "",
            0.5,
            &long,
        )];
        let content = render_capture_content("text", &citations);
        let truncated = format!("{}...", "y".repeat(CAPTURE_EVIDENCE_EXCERPT_RUNES));
        assert!(
            content.contains(&format!("excerpt=\"{truncated}\"")),
            "{content}"
        );
        assert!(
            !content.contains(&"y".repeat(CAPTURE_EVIDENCE_EXCERPT_RUNES + 1)),
            "truncated at the cap: {content}"
        );
    }
}
