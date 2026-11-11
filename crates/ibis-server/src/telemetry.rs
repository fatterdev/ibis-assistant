//! F17 — `mcp.tool` / `memory.consolidate` span construction and recording.
//!
//! Attribute sets are closed whitelists: tool name/annotation, project, actor,
//! runtime mode, outcome; task id/recoveries, pass counts and outcome. No
//! arguments, no tool output, no error messages.

use serde_json::{Map, Value};
use tracing::Span;

use ibis_core::telemetry::{
    otel_active, sanitize_actor, sanitize_project, sanitize_span_value, OTEL_STATUS_CODE,
    SPAN_MCP_TOOL, SPAN_MEMORY_CONSOLIDATE,
};

pub const MCP_TOOL_ATTRIBUTES: &[&str] = &[
    "mcp.tool.name",
    "mcp.tool.read_only",
    "ibis.project",
    "ibis.actor",
    "ibis.runtime_mode",
    "mcp.tool.outcome",
];

pub const MEMORY_CONSOLIDATE_ATTRIBUTES: &[&str] = &[
    "ibis.project",
    "ibis.task.id",
    "ibis.task.recoveries",
    "ibis.items",
    "ibis.summaries",
    "ibis.deferred",
    "ibis.fallbacks",
    "ibis.failed",
    "ibis.outcome",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok,
    Error,
    Panic,
}

impl ToolOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolOutcome::Ok => "ok",
            ToolOutcome::Error => "error",
            ToolOutcome::Panic => "panic",
        }
    }

    fn is_error(self) -> bool {
        matches!(self, ToolOutcome::Error | ToolOutcome::Panic)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassOutcome {
    Completed,
    Failed,
    Requeued,
}

impl PassOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            PassOutcome::Completed => "completed",
            PassOutcome::Failed => "failed",
            PassOutcome::Requeued => "requeued",
        }
    }
}

/// The only tool argument that reaches telemetry: `project_name`.
pub fn project_from_arguments(args: Option<&Map<String, Value>>) -> Option<String> {
    args.and_then(|args| args.get("project_name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A tool name is exported only when it belongs to the 58-tool registry;
/// arbitrary client strings become a single bounded value.
pub fn tool_span_name(name: &str) -> &str {
    if crate::registry::annotation_of(name).is_some() {
        name
    } else {
        "<unknown>"
    }
}

pub fn tool_read_only(name: &str) -> bool {
    crate::registry::annotation_of(name)
        .map(|annotation| annotation.read_only)
        .unwrap_or(false)
}

pub fn tool_span(name: &str, read_only: bool, project: Option<&str>, runtime_mode: &str) -> Span {
    let name = sanitize_span_value(name, 64);
    let runtime_mode = sanitize_span_value(runtime_mode, 32);
    let runtime_mode = if runtime_mode.is_empty() {
        "unknown".to_string()
    } else {
        runtime_mode
    };
    let span = tracing::info_span!(
        SPAN_MCP_TOOL,
        "mcp.tool.name" = name,
        "mcp.tool.read_only" = read_only,
        "ibis.project" = tracing::field::Empty,
        "ibis.actor" = tracing::field::Empty,
        "ibis.runtime_mode" = runtime_mode,
        "mcp.tool.outcome" = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty,
    );
    if let Some(project) = project {
        let project = sanitize_project(project);
        if !project.is_empty() {
            span.record("ibis.project", project);
        }
    }
    span
}

/// Same span, created only when OTLP export is active: the default build and
/// config keep `fmt` output free of span scopes.
pub fn active_tool_span(
    name: &str,
    read_only: bool,
    project: Option<&str>,
    runtime_mode: &str,
) -> Span {
    if otel_active() {
        tool_span(name, read_only, project, runtime_mode)
    } else {
        Span::none()
    }
}

/// Records the request actor on the current `mcp.tool` span (called from
/// `dispatch`, inside the instrumented tool future).
pub fn record_tool_actor(actor: Option<&str>) {
    let Some(actor) = actor else {
        return;
    };
    let actor = sanitize_actor(actor);
    if !actor.is_empty() {
        Span::current().record("ibis.actor", actor);
    }
}

pub fn record_tool_outcome(span: &Span, outcome: ToolOutcome) {
    span.record("mcp.tool.outcome", outcome.as_str());
    if outcome.is_error() {
        span.record(OTEL_STATUS_CODE, "ERROR");
    }
}

pub fn memory_consolidate_span(project: &str) -> Span {
    tracing::info_span!(
        SPAN_MEMORY_CONSOLIDATE,
        "ibis.project" = sanitize_project(project),
        "ibis.task.id" = tracing::field::Empty,
        "ibis.task.recoveries" = tracing::field::Empty,
        "ibis.items" = tracing::field::Empty,
        "ibis.summaries" = tracing::field::Empty,
        "ibis.deferred" = tracing::field::Empty,
        "ibis.fallbacks" = tracing::field::Empty,
        "ibis.failed" = tracing::field::Empty,
        "ibis.outcome" = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty,
    )
}

pub fn active_memory_consolidate_span(project: &str) -> Span {
    if otel_active() {
        memory_consolidate_span(project)
    } else {
        Span::none()
    }
}

pub fn record_task(span: &Span, task_id: &str, recoveries: u64) {
    span.record("ibis.task.id", task_id);
    span.record("ibis.task.recoveries", recoveries);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassStats {
    pub items: u64,
    pub summaries: u64,
    pub deferred: u64,
    pub fallbacks: u64,
    pub failed: u64,
}

pub fn record_pass_stats(span: &Span, stats: &PassStats) {
    span.record("ibis.items", stats.items);
    span.record("ibis.summaries", stats.summaries);
    span.record("ibis.deferred", stats.deferred);
    span.record("ibis.fallbacks", stats.fallbacks);
    span.record("ibis.failed", stats.failed);
}

pub fn record_pass_outcome(span: &Span, outcome: PassOutcome) {
    span.record("ibis.outcome", outcome.as_str());
    if outcome == PassOutcome::Failed {
        span.record(OTEL_STATUS_CODE, "ERROR");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fmt;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::Attributes;
    use tracing::{Id, Subscriber};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::registry::LookupSpan;

    use super::*;
    use crate::registry::annotation_of;

    #[derive(Debug, Clone, Default)]
    struct CapturedSpan {
        name: String,
        fields: BTreeMap<String, String>,
    }

    #[derive(Debug, Default)]
    struct CaptureState {
        spans: Vec<CapturedSpan>,
        index: HashMap<u64, usize>,
    }

    #[derive(Debug, Clone, Default)]
    struct Capture {
        state: Arc<Mutex<CaptureState>>,
    }

    struct Values<'a>(&'a mut BTreeMap<String, String>);

    impl Visit for Values<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    impl<S> Layer<S> for Capture
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            let span = ctx.span(id).expect("span in context");
            let mut fields = BTreeMap::new();
            for field in span.metadata().fields().iter() {
                fields.insert(field.name().to_string(), String::new());
            }
            attrs.record(&mut Values(&mut fields));
            let mut state = self.state.lock().unwrap();
            let index = state.spans.len();
            state.spans.push(CapturedSpan {
                name: span.metadata().name().to_string(),
                fields,
            });
            state.index.insert(id.into_u64(), index);
        }

        fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _ctx: Context<'_, S>) {
            let mut state = self.state.lock().unwrap();
            if let Some(index) = state.index.get(&id.into_u64()).copied() {
                values.record(&mut Values(&mut state.spans[index].fields));
            }
        }
    }

    fn capture(run: impl FnOnce()) -> Vec<CapturedSpan> {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, run);
        let spans = capture.state.lock().unwrap().spans.clone();
        spans
    }

    fn expected(attributes: &[&str]) -> BTreeSet<String> {
        attributes
            .iter()
            .copied()
            .chain(std::iter::once(OTEL_STATUS_CODE))
            .map(str::to_string)
            .collect()
    }

    fn keys(span: &CapturedSpan) -> BTreeSet<String> {
        span.fields.keys().cloned().collect()
    }

    #[test]
    fn tool_attributes_match_whitelist() {
        let spans = capture(|| {
            let span = tool_span("memory_search", true, Some("proj"), "personal");
            let _guard = span.enter();
            record_tool_actor(Some("fabio"));
            record_tool_outcome(&span, ToolOutcome::Ok);
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "mcp.tool");
        assert_eq!(keys(&spans[0]), expected(MCP_TOOL_ATTRIBUTES));
        assert_eq!(spans[0].fields["mcp.tool.name"], "memory_search");
        assert_eq!(spans[0].fields["mcp.tool.read_only"], "true");
        assert_eq!(spans[0].fields["ibis.project"], "proj");
        assert_eq!(spans[0].fields["ibis.actor"], "fabio");
        assert_eq!(spans[0].fields["ibis.runtime_mode"], "personal");
        assert_eq!(spans[0].fields["mcp.tool.outcome"], "ok");
    }

    #[test]
    fn tool_attributes_never_contain_content() {
        let args = serde_json::json!({
            "project_name": "proj",
            "text": "SECRET TEXT",
            "content": "SECRET CONTENT",
            "query": "SECRET QUERY",
        });
        let map = args.as_object().unwrap();
        let project = project_from_arguments(Some(map));
        assert_eq!(project.as_deref(), Some("proj"));

        let spans = capture(|| {
            let span = tool_span("memory_add", false, project.as_deref(), "server");
            record_tool_outcome(&span, ToolOutcome::Error);
        });
        assert!(spans[0]
            .fields
            .values()
            .all(|value| !value.contains("SECRET")));
    }

    #[test]
    fn project_is_sanitized_and_capped() {
        assert_eq!(sanitize_project("  a\r\nb\tc  "), "abc");
        assert_eq!(sanitize_project(&"p".repeat(300)).chars().count(), 120);

        let spans = capture(|| {
            let _span = tool_span("init_project", false, Some("  a\nb  "), "personal");
        });
        assert_eq!(spans[0].fields["ibis.project"], "ab");
    }

    #[test]
    fn read_only_comes_from_the_annotations_table() {
        assert!(annotation_of("memory_search").unwrap().read_only);
        assert!(!annotation_of("memory_add").unwrap().read_only);
        assert_eq!(
            annotation_of("memory_search").unwrap().name,
            "memory_search"
        );
    }

    #[test]
    fn unknown_tool_names_are_bounded() {
        assert_eq!(tool_span_name("memory_search"), "memory_search");
        assert_eq!(tool_span_name("../../etc/passwd"), "<unknown>");
        assert_eq!(tool_span_name(&"x".repeat(200)), "<unknown>");
        assert!(tool_read_only("memory_search"));
        assert!(!tool_read_only("../../etc/passwd"));
    }

    #[test]
    fn empty_runtime_mode_is_unknown() {
        let spans = capture(|| {
            let _span = tool_span("memory_search", true, None, "  ");
        });
        assert_eq!(spans[0].fields["ibis.runtime_mode"], "unknown");
    }

    #[test]
    fn outcome_error_sets_otel_status() {
        let spans = capture(|| {
            let span = tool_span("memory_add", false, None, "personal");
            record_tool_outcome(&span, ToolOutcome::Panic);
        });
        assert_eq!(spans[0].fields["mcp.tool.outcome"], "panic");
        assert_eq!(spans[0].fields[OTEL_STATUS_CODE], "ERROR");
    }

    #[test]
    fn memory_consolidate_attributes_match_whitelist() {
        let spans = capture(|| {
            let span = memory_consolidate_span("proj");
            record_task(&span, "task:00112233445566778899aabbccddeeff", 2);
            record_pass_stats(
                &span,
                &PassStats {
                    items: 7,
                    summaries: 3,
                    deferred: 1,
                    fallbacks: 2,
                    failed: 0,
                },
            );
            record_pass_outcome(&span, PassOutcome::Completed);
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "memory.consolidate");
        assert_eq!(keys(&spans[0]), expected(MEMORY_CONSOLIDATE_ATTRIBUTES));
        assert_eq!(spans[0].fields["ibis.project"], "proj");
        assert_eq!(
            spans[0].fields["ibis.task.id"],
            "task:00112233445566778899aabbccddeeff"
        );
        assert_eq!(spans[0].fields["ibis.task.recoveries"], "2");
        assert_eq!(spans[0].fields["ibis.items"], "7");
        assert_eq!(spans[0].fields["ibis.summaries"], "3");
        assert_eq!(spans[0].fields["ibis.deferred"], "1");
        assert_eq!(spans[0].fields["ibis.fallbacks"], "2");
        assert_eq!(spans[0].fields["ibis.failed"], "0");
        assert_eq!(spans[0].fields["ibis.outcome"], "completed");
    }

    #[test]
    fn failed_pass_sets_otel_status() {
        let spans = capture(|| {
            let span = memory_consolidate_span("proj");
            record_pass_outcome(&span, PassOutcome::Failed);
        });
        assert_eq!(spans[0].fields["ibis.outcome"], "failed");
        assert_eq!(spans[0].fields[OTEL_STATUS_CODE], "ERROR");
    }

    /// Default build/config: the production constructors return a no-op span,
    /// so the `fmt` layer sees no span scopes (pre-F17 log lines unchanged).
    #[test]
    fn active_spans_are_disabled_without_otel() {
        assert!(active_tool_span("memory_search", true, None, "personal").is_disabled());
        assert!(active_memory_consolidate_span("proj").is_disabled());
    }
}
