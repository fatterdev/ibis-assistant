//! F17 — stable span names and attribute sanitizers shared by the crates that
//! create spans.
//!
//! Span names are constants: no dynamic value goes into the name. Free-form
//! attribute values (project, actor) are sanitized and capped here so every
//! producer applies the same rule.

pub const SPAN_MCP_TOOL: &str = "mcp.tool";
pub const SPAN_AI_GENERATE: &str = "ai.generate";
pub const SPAN_AI_EMBED: &str = "ai.embed";
pub const SPAN_MEMORY_CONSOLIDATE: &str = "memory.consolidate";

/// `tracing-opentelemetry` special field: it sets the OTel span status and is
/// not exported as a regular attribute.
pub const OTEL_STATUS_CODE: &str = "otel.status_code";

pub const PROJECT_ATTRIBUTE_CAP: usize = 120;
pub const ACTOR_ATTRIBUTE_CAP: usize = 120;

/// Removes control characters (newlines included), trims, and caps the value
/// at `cap` characters. Keeps telemetry to names/ids, never content.
pub fn sanitize_span_value(value: &str, cap: usize) -> String {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().chars().take(cap).collect()
}

pub fn sanitize_project(value: &str) -> String {
    sanitize_span_value(value, PROJECT_ATTRIBUTE_CAP)
}

pub fn sanitize_actor(value: &str) -> String {
    sanitize_span_value(value, ACTOR_ATTRIBUTE_CAP)
}

/// Re-export so crates that only depend on `ibis-core` (e.g. `ibis-ai`) can
/// gate span creation without a direct `ibis-logger` dependency.
pub use ibis_logger::otel_active;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_names_are_stable_constants() {
        assert_eq!(SPAN_MCP_TOOL, "mcp.tool");
        assert_eq!(SPAN_AI_GENERATE, "ai.generate");
        assert_eq!(SPAN_AI_EMBED, "ai.embed");
        assert_eq!(SPAN_MEMORY_CONSOLIDATE, "memory.consolidate");
    }

    #[test]
    fn sanitizer_strips_control_and_caps() {
        assert_eq!(sanitize_span_value("  a\r\nb\tc  ", 120), "abc");
        assert_eq!(
            sanitize_span_value(&"x".repeat(200), 120).chars().count(),
            120
        );
        // Cap counts characters, not bytes.
        assert_eq!(sanitize_span_value(&"è".repeat(200), 64), "è".repeat(64));
    }

    #[test]
    fn project_and_actor_caps_are_120() {
        assert_eq!(sanitize_project(&"p".repeat(300)).chars().count(), 120);
        assert_eq!(sanitize_actor(&"a".repeat(300)).chars().count(), 120);
    }

    #[test]
    fn otel_inactive_by_default() {
        assert!(!otel_active());
    }
}
