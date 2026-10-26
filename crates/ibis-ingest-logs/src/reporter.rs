//! Markdown report generation beside the analyzed log
//!: `report_<logfile>_<ts>.md`.
//!
//! Improvement over `generate_report` returns both the path and the file
//! content so the tailer does not have to read the report back from disk
//!

use std::path::Path;

use crate::analyzer::{display_or, SemanticError};
use crate::error::Result;

pub struct Reporter {
    pub cfg: Option<std::sync::Arc<ibis_core::config::Config>>,
}

impl Reporter {
    pub fn new(cfg: Option<std::sync::Arc<ibis_core::config::Config>>) -> Self {
        Reporter { cfg }
    }

    /// Writes the report beside the log file (falling back to `logs_root`)
    /// and returns `(path, content)` (`GenerateReport returned the path).
    pub async fn generate_report(
        &self,
        project: &str,
        log_file_name: &str,
        anomalies: &[SemanticError],
        cost: f64,
    ) -> Result<(String, String)> {
        let report_name = format!(
            "report_{}_{}.md",
            Path::new(log_file_name)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            chrono::Local::now().format("%Y%m%d%H%M%S")
        );
        // Write beside the log file when possible; fall back to logs_root.
        let mut dir = Path::new(log_file_name)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if dir.is_empty() || dir == "." {
            dir = self
                .cfg
                .as_ref()
                .map(|c| c.logs_root.clone())
                .unwrap_or_default();
        }
        tokio::fs::create_dir_all(&dir).await?;
        let report_path = Path::new(&dir).join(&report_name);

        let mut b = String::new();
        b.push_str(&format!(
            "# Log Analysis Report\n\n**Project:** {project}\n**Log File:** {}\n**Report Date:** {}\n\n## Overview\n- **Errors Detected (Current Batch):** {}\n- **Estimated AI Token Cost:** ~${cost:.4}\n\n",
            Path::new(log_file_name)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            // Scenario: time.RFC822: "02 Jan 06 15:04 MST"; %z renders +0200 — the
            // zone abbreviation is not reproducible portably.
            chrono::Local::now().format("%d %b %y %H:%M %z"),
            anomalies.len(),
        ));

        if !anomalies.is_empty() {
            b.push_str("## Detected Anomalies\n\n");
            for (i, a) in anomalies.iter().enumerate() {
                b.push_str(&format!(
                    "### {}. {}\n\n",
                    i + 1,
                    display_or(&a.category, "Uncategorized")
                ));
                b.push_str(&format!("- **Severity:** {}\n", a.severity));
                let mut file_disp = display_or(&a.file, "(unknown)").to_string();
                if a.line > 0 {
                    file_disp = format!("{file_disp}:{}", a.line);
                }
                b.push_str(&format!("- **File:** {file_disp}\n"));
                if !a.symbol.is_empty() {
                    b.push_str(&format!("- **Symbol:** {}\n", a.symbol));
                }
                b.push_str(&format!(
                    "- **Cause:** {}\n",
                    display_or(&a.cause, "(not determined)")
                ));
                if !a.baseline.is_empty() {
                    b.push_str(&format!("- **Baseline:** {}\n", a.baseline));
                }
                b.push_str(&format!(
                    "- **Template:** `{}`\n\n",
                    escape_inline_code(display_or(&a.template, &a.category))
                ));
                if !a.stack_trace.trim().is_empty() {
                    b.push_str("**Stack / Context:**\n\n```\n");
                    b.push_str(a.stack_trace.trim());
                    b.push_str("\n```\n\n");
                }
                if !a.supersedes.is_empty() {
                    b.push_str(&format!(
                        "- **Supersedes:** {}\n\n",
                        a.supersedes.join(", ")
                    ));
                }
            }
        } else {
            b.push_str("*No semantic anomalies were attached to this report.*\n");
        }

        let content = b;
        tokio::fs::write(&report_path, &content).await?;
        tracing::info!("Generated report at: {}", report_path.display());
        Ok((report_path.display().to_string(), content))
    }
}

fn escape_inline_code(s: &str) -> String {
    s.replace('`', "'")
}
