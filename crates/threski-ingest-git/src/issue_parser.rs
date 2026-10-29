//! Ticket-reference extraction from commit subjects.
//!
//! Standard patterns, in fixed precedence order (Azure DevOps first so its
//! spans shadow the generic hash form, then Jira, then `#123`):
//! - Azure DevOps: `AB#123`
//! - Jira: `ABC-123`
//! - Redmine generic: `#123` (provider resolved downstream)
//! - Custom patterns: `"<regex>"` or `"<provider>::<regex>"`, skipped on spans
//!   already claimed by the built-ins.
//!
//! ASCII-vs-Unicode note (E-A09): the built-in patterns are compiled
//! with `unicode(false)` so `\b`/`\d` keep the original (ASCII) semantics exactly;
//! user-provided custom patterns keep the Rust Unicode default ( compiles
//! them with its UTF-8 default as well) and are skipped when they fail to
//! compile.

use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use threski_ticketing::{IssueReference, ProviderName, PROVIDER_AZURE_DEVOPS, PROVIDER_JIRA};

fn ascii_regex(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .unicode(false)
        .build()
        .expect("valid built-in regex")
}

static ADO_ISSUE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| ascii_regex(r"\b([A-Z][A-Z0-9]+)#(\d+)\b"));
static JIRA_ISSUE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| ascii_regex(r"\b([A-Z][A-Z0-9]+-\d+)\b"));
static HASH_ISSUE_PATTERN: LazyLock<Regex> = LazyLock::new(|| ascii_regex(r"#(\d+)"));

/// Parses a commit message and returns the referenced issues with confidence
/// levels (1.0 when the message contains fix/close/resolve anywhere, else 0.5).
pub fn extract_issue_refs(message: &str, custom_patterns: &[&str]) -> Vec<IssueReference> {
    let lower = message.to_lowercase();
    let confidence =
        if lower.contains("fix") || lower.contains("close") || lower.contains("resolve") {
            1.0
        } else {
            0.5
        };

    let mut collector = RefCollector {
        refs: Vec::new(),
        seen: std::collections::HashSet::new(),
        occupied: Vec::new(),
        confidence,
    };

    for caps in ADO_ISSUE_PATTERN.captures_iter(message) {
        let (Some(full), Some(project), Some(id)) = (caps.get(0), caps.get(1), caps.get(2)) else {
            continue;
        };
        collector.record(
            IssueReference {
                provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
                external_id: id.as_str().to_string(),
                external_key: full.as_str().to_string(),
                project_key: project.as_str().to_string(),
                confidence: 0.0,
            },
            (full.start(), full.end()),
        );
    }

    for caps in JIRA_ISSUE_PATTERN.captures_iter(message) {
        let (Some(full), Some(key)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let project = key.as_str().split('-').next().unwrap_or("").to_string();
        collector.record(
            IssueReference {
                provider: ProviderName::from(PROVIDER_JIRA),
                external_id: key.as_str().to_string(),
                external_key: key.as_str().to_string(),
                project_key: project,
                confidence: 0.0,
            },
            (full.start(), full.end()),
        );
    }

    for caps in HASH_ISSUE_PATTERN.captures_iter(message) {
        let (Some(full), Some(id)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        if is_occupied(full.start(), full.end(), &collector.occupied) {
            continue;
        }
        collector.record(
            IssueReference {
                external_id: id.as_str().to_string(),
                external_key: id.as_str().to_string(),
                ..IssueReference::default()
            },
            (full.start(), full.end()),
        );
    }

    for raw in custom_patterns {
        let (provider, expr) = parse_custom_pattern(raw);
        if expr.is_empty() {
            continue;
        }
        let Ok(re) = Regex::new(&expr) else {
            continue;
        };
        for caps in re.captures_iter(message) {
            let Some(full) = caps.get(0) else { continue };
            if is_occupied(full.start(), full.end(), &collector.occupied) {
                continue;
            }
            let mut value = full.as_str().trim().to_string();
            if let Some(group) = caps.get(1) {
                value = group.as_str().trim().to_string();
            }
            collector.record(
                IssueReference {
                    provider: provider.clone(),
                    external_id: value.clone(),
                    external_key: value,
                    ..IssueReference::default()
                },
                (full.start(), full.end()),
            );
        }
    }

    collector.refs
}

/// Dedupe + span-occupancy bookkeeping (the `record` closure).
struct RefCollector {
    refs: Vec<IssueReference>,
    seen: std::collections::HashSet<String>,
    occupied: Vec<(usize, usize)>,
    confidence: f64,
}

impl RefCollector {
    fn record(&mut self, mut issue_ref: IssueReference, span: (usize, usize)) {
        if issue_ref.external_key.is_empty() {
            issue_ref.external_key = issue_ref.external_id.clone();
        }
        if issue_ref.external_id.is_empty() {
            issue_ref.external_id = issue_ref.external_key.clone();
        }
        let key = format!(
            "{}|{}|{}",
            issue_ref.provider, issue_ref.external_key, issue_ref.external_id
        );
        if issue_ref.external_id.is_empty() || self.seen.contains(&key) {
            return;
        }
        issue_ref.confidence = self.confidence;
        self.refs.push(issue_ref);
        self.seen.insert(key);
        if span.1 > span.0 {
            self.occupied.push(span);
        }
    }
}

/// Splits `"<provider>::<regex>"` custom patterns.
fn parse_custom_pattern(raw: &str) -> (ProviderName, String) {
    let raw = raw.trim();
    if raw.is_empty() {
        return (ProviderName::default(), String::new());
    }
    if let Some((provider, expr)) = raw.split_once("::") {
        return (
            ProviderName(provider.trim().to_lowercase()),
            expr.trim().to_string(),
        );
    }
    (ProviderName::default(), raw.to_string())
}

/// Reports whether `[start, end)` overlaps any already-claimed span.
fn is_occupied(start: usize, end: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(s, e)| start < e && end > s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(s: &str) -> ProviderName {
        ProviderName::from(s)
    }

    // Scenario: TestExtractIssueRefs
    #[test]
    fn test_extract_issue_refs() {
        struct Case {
            name: &'static str,
            input: &'static str,
            custom: &'static [&'static str],
            expected: Vec<IssueReference>,
        }
        let cases = vec![
            Case {
                name: "Redmine hash reference",
                input: "This fixes #123",
                custom: &[],
                expected: vec![IssueReference {
                    provider: ProviderName::default(),
                    external_id: "123".into(),
                    external_key: "123".into(),
                    project_key: String::new(),
                    confidence: 1.0,
                }],
            },
            Case {
                name: "Jira reference",
                input: "Fixes ABC-456",
                custom: &[],
                expected: vec![IssueReference {
                    provider: provider(PROVIDER_JIRA),
                    external_id: "ABC-456".into(),
                    external_key: "ABC-456".into(),
                    project_key: "ABC".into(),
                    confidence: 1.0,
                }],
            },
            Case {
                name: "Azure DevOps reference",
                input: "Resolves AB#789",
                custom: &[],
                expected: vec![IssueReference {
                    provider: provider(PROVIDER_AZURE_DEVOPS),
                    external_id: "789".into(),
                    external_key: "AB#789".into(),
                    project_key: "AB".into(),
                    confidence: 1.0,
                }],
            },
            Case {
                name: "Multiple references",
                input: "Fixes #123 and closes ABC-222",
                custom: &[],
                expected: vec![
                    IssueReference {
                        provider: provider(PROVIDER_JIRA),
                        external_id: "ABC-222".into(),
                        external_key: "ABC-222".into(),
                        project_key: "ABC".into(),
                        confidence: 1.0,
                    },
                    IssueReference {
                        provider: ProviderName::default(),
                        external_id: "123".into(),
                        external_key: "123".into(),
                        project_key: String::new(),
                        confidence: 1.0,
                    },
                ],
            },
            Case {
                name: "No issue",
                input: "chore: cleanup",
                custom: &[],
                expected: vec![],
            },
            Case {
                name: "Custom pattern with provider",
                input: "refs TASK-77",
                custom: &[r"jira::(TASK-\d+)"],
                expected: vec![IssueReference {
                    provider: provider(PROVIDER_JIRA),
                    external_id: "TASK-77".into(),
                    external_key: "TASK-77".into(),
                    project_key: "TASK".into(),
                    confidence: 0.5,
                }],
            },
        ];
        for case in cases {
            let got = extract_issue_refs(case.input, case.custom);
            assert_eq!(got, case.expected, "case {}", case.name);
        }
    }

    #[test]
    fn custom_patterns_cover_built_in_spans() {
        // The generic hash ref is already claimed by the ADO span, so the
        // custom pattern must not re-record it.
        let got = extract_issue_refs("Resolves AB#789", &[r"redmine::#(\d+)"]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].provider.as_str(), PROVIDER_AZURE_DEVOPS);

        // Invalid custom regex is skipped silently (parity).
        let got = extract_issue_refs("refs X-1", &[r"jira::([unclosed"]);
        assert!(got.is_empty());
    }

    #[test]
    fn parse_custom_pattern_shapes() {
        let (p, e) = parse_custom_pattern("  jira :: (X-\\d+) ");
        assert_eq!(p.as_str(), "jira");
        assert_eq!(e, r"(X-\d+)");
        let (p, e) = parse_custom_pattern("plain-regex");
        assert!(p.is_empty());
        assert_eq!(e, "plain-regex");
        let (p, e) = parse_custom_pattern("   ");
        assert!(p.is_empty());
        assert!(e.is_empty());
    }
}
