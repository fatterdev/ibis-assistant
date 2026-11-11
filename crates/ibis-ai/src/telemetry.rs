//! F17 — `ai.generate` / `ai.embed` span construction and result recording.
//!
//! The attribute set is a closed whitelist (names, provider, token counts,
//! error kind). Prompt content, responses and provider messages never become
//! attributes.

use ibis_core::telemetry::{
    otel_active, sanitize_span_value, OTEL_STATUS_CODE, SPAN_AI_EMBED, SPAN_AI_GENERATE,
};

use crate::error::AiError;
use crate::types::{Candidate, GenerationConfig};

pub const AI_GENERATE_ATTRIBUTES: &[&str] = &[
    "ai.provider",
    "ai.route_hint",
    "ai.temperature",
    "ai.max_output_tokens",
    "ai.finish_reason",
    "ai.tokens.prompt",
    "ai.tokens.completion",
    "ai.tokens.cached",
    "ai.error.kind",
];

pub const AI_EMBED_ATTRIBUTES: &[&str] = &[
    "ai.provider",
    "ai.embeddings.count",
    "ai.embeddings.dim",
    "ai.error.kind",
];

/// `ai.generate` span: provider and request knobs at creation, outcome and
/// token usage recorded after the call.
pub fn generate_span(provider: &str, config: &GenerationConfig) -> tracing::Span {
    let provider = sanitize_span_value(provider, 64);
    let provider = if provider.is_empty() {
        "unknown".to_string()
    } else {
        provider
    };
    let span = tracing::info_span!(
        SPAN_AI_GENERATE,
        "ai.provider" = provider,
        "ai.route_hint" = tracing::field::Empty,
        "ai.temperature" = config.temperature,
        "ai.max_output_tokens" = config.max_output_tokens,
        "ai.finish_reason" = tracing::field::Empty,
        "ai.tokens.prompt" = tracing::field::Empty,
        "ai.tokens.completion" = tracing::field::Empty,
        "ai.tokens.cached" = tracing::field::Empty,
        "ai.error.kind" = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty,
    );
    let route_hint = sanitize_span_value(&config.route_hint, 64);
    if !route_hint.is_empty() {
        span.record("ai.route_hint", route_hint);
    }
    span
}

/// Same span, created only when OTLP export is active: the default build and
/// config keep `fmt` output free of span scopes.
pub fn active_generate_span(provider: &str, config: &GenerationConfig) -> tracing::Span {
    if otel_active() {
        generate_span(provider, config)
    } else {
        tracing::Span::none()
    }
}

/// `ai.embed` span: the request count at creation, the dimension after a
/// successful call.
pub fn embed_span(provider: &str, count: usize) -> tracing::Span {
    let provider = sanitize_span_value(provider, 64);
    let provider = if provider.is_empty() {
        "unknown".to_string()
    } else {
        provider
    };
    tracing::info_span!(
        SPAN_AI_EMBED,
        "ai.provider" = provider,
        "ai.embeddings.count" = count,
        "ai.embeddings.dim" = tracing::field::Empty,
        "ai.error.kind" = tracing::field::Empty,
        "otel.status_code" = tracing::field::Empty,
    )
}

pub fn active_embed_span(provider: &str, count: usize) -> tracing::Span {
    if otel_active() {
        embed_span(provider, count)
    } else {
        tracing::Span::none()
    }
}

pub fn record_generate_result(span: &tracing::Span, result: &Result<Candidate, AiError>) {
    match result {
        Ok(candidate) => {
            let finish_reason = sanitize_span_value(&candidate.finish_reason, 64);
            if !finish_reason.is_empty() {
                span.record("ai.finish_reason", finish_reason);
            }
            if let Some(usage) = &candidate.usage_metadata {
                // `usage_metadata` absent → token attributes omitted, not zero.
                // Negative provider values are clamped (the attribute is a
                // count, not billing truth).
                span.record("ai.tokens.prompt", usage.prompt_token_count.max(0));
                span.record("ai.tokens.completion", usage.candidates_token_count.max(0));
                if let Some(cached) = usage.cached_content_token_count {
                    span.record("ai.tokens.cached", cached.max(0));
                }
            }
        }
        Err(err) => record_error(span, err),
    }
}

pub fn record_embed_success(span: &tracing::Span, dim: usize) {
    span.record("ai.embeddings.dim", dim);
}

pub fn record_embed_result(span: &tracing::Span, result: &Result<Vec<Vec<f32>>, AiError>) {
    match result {
        Ok(embeddings) => {
            if let Some(first) = embeddings.first() {
                record_embed_success(span, first.len());
            }
        }
        Err(err) => record_error(span, err),
    }
}

pub fn record_error(span: &tracing::Span, err: &AiError) {
    span.record("ai.error.kind", err.kind());
    span.record(OTEL_STATUS_CODE, "ERROR");
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
    use crate::types::{Content, Part, UsageMetadata};

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
    fn generate_attributes_match_whitelist() {
        let spans = capture(|| {
            let config = GenerationConfig {
                route_hint: "local:M".into(),
                temperature: 0.2,
                max_output_tokens: 128,
                ..Default::default()
            };
            let span = generate_span("ollama", &config);
            let candidate = Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![Part {
                        text: "SECRET RESPONSE".into(),
                    }],
                },
                finish_reason: "STOP".into(),
                usage_metadata: Some(UsageMetadata {
                    prompt_token_count: 10,
                    candidates_token_count: 5,
                    total_token_count: 15,
                    cached_content_token_count: Some(2),
                }),
            };
            record_generate_result(&span, &Ok(candidate));
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, SPAN_AI_GENERATE);
        assert_eq!(keys(&spans[0]), expected(AI_GENERATE_ATTRIBUTES));
        assert_eq!(spans[0].fields["ai.provider"], "ollama");
        assert_eq!(spans[0].fields["ai.route_hint"], "local:M");
        assert_eq!(spans[0].fields["ai.finish_reason"], "STOP");
        assert_eq!(spans[0].fields["ai.tokens.prompt"], "10");
        assert_eq!(spans[0].fields["ai.tokens.completion"], "5");
        assert_eq!(spans[0].fields["ai.tokens.cached"], "2");
        assert!(spans[0]
            .fields
            .values()
            .all(|value| !value.contains("SECRET")));
    }

    #[test]
    fn embed_attributes_match_whitelist() {
        let spans = capture(|| {
            let span = embed_span("ollama", 3);
            record_embed_result(&span, &Ok(vec![vec![0.0; 768]; 3]));
        });

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, SPAN_AI_EMBED);
        assert_eq!(keys(&spans[0]), expected(AI_EMBED_ATTRIBUTES));
        assert_eq!(spans[0].fields["ai.embeddings.count"], "3");
        assert_eq!(spans[0].fields["ai.embeddings.dim"], "768");
    }

    #[test]
    fn ai_error_exports_kind_only() {
        let spans = capture(|| {
            let span = generate_span("gemini", &GenerationConfig::default());
            record_generate_result(
                &span,
                &Err(AiError::Transport(
                    "request failed for prompt SECRET PROMPT".into(),
                )),
            );
        });

        assert_eq!(spans[0].fields["ai.error.kind"], "transport");
        assert!(spans[0]
            .fields
            .values()
            .all(|value| !value.contains("SECRET")));
    }

    #[test]
    fn usage_absent_keeps_token_attributes_absent() {
        let spans = capture(|| {
            let span = generate_span("gemini", &GenerationConfig::default());
            let candidate = Candidate {
                finish_reason: "STOP".into(),
                usage_metadata: None,
                ..Default::default()
            };
            record_generate_result(&span, &Ok(candidate));
        });

        assert_eq!(spans[0].fields["ai.tokens.prompt"], "");
        assert_eq!(spans[0].fields["ai.tokens.completion"], "");
        assert_eq!(spans[0].fields["ai.tokens.cached"], "");
    }

    #[test]
    fn ai_error_kind_is_stable() {
        assert_eq!(AiError::ContextOnly.kind(), "context_only");
        assert_eq!(
            AiError::Unsupported {
                provider: "p".into(),
                operation: "op".into()
            }
            .kind(),
            "unsupported"
        );
        assert_eq!(AiError::other("message").kind(), "other");
    }

    #[test]
    fn negative_token_counts_are_clamped() {
        let spans = capture(|| {
            let span = generate_span("gemini", &GenerationConfig::default());
            let candidate = Candidate {
                finish_reason: "STOP".into(),
                usage_metadata: Some(UsageMetadata {
                    prompt_token_count: -5,
                    candidates_token_count: -2,
                    total_token_count: 0,
                    cached_content_token_count: Some(-1),
                }),
                ..Default::default()
            };
            record_generate_result(&span, &Ok(candidate));
        });
        assert_eq!(spans[0].fields["ai.tokens.prompt"], "0");
        assert_eq!(spans[0].fields["ai.tokens.completion"], "0");
        assert_eq!(spans[0].fields["ai.tokens.cached"], "0");
    }

    #[test]
    fn provider_and_finish_reason_are_bounded() {
        let spans = capture(|| {
            let span = generate_span("  \r\n  ", &GenerationConfig::default());
            let candidate = Candidate {
                finish_reason: format!("{}\n{}", "f".repeat(100), "x"),
                ..Default::default()
            };
            record_generate_result(&span, &Ok(candidate));
        });
        assert_eq!(spans[0].fields["ai.provider"], "unknown");
        assert_eq!(spans[0].fields["ai.finish_reason"].chars().count(), 64);
        assert!(!spans[0].fields["ai.finish_reason"].contains('\n'));
    }

    #[test]
    fn active_ai_spans_are_disabled_without_otel() {
        assert!(active_generate_span("ollama", &GenerationConfig::default()).is_disabled());
        assert!(active_embed_span("ollama", 1).is_disabled());
    }
}
