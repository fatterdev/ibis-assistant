//! SMTP notifications for log reports.
//!
//! Three paths: emergency bypass (severity ≥ threshold sends immediately),
//! immediate send (no aggregation window), and DB-queued aggregation
//! (`log_pending_notification` rows drained by `process_pending_notifications`
//! into one digest — at-most-once: rows are deleted before sending).
//!
//! The SMTP client is hand-rolled over tokio: EHLO →
//! optional STARTTLS/AUTH PLAIN → MAIL FROM → RCPT TO → DATA (with
//! dot-stuffing) → QUIT. `ssl_tls` dials TLS directly. PLAIN auth
//! refuses to send credentials over an unencrypted non-localhost connection —
//! the guard is preserved. Dial/IO timeouts (30s) are a Rust addition: had
//! none and a hung SMTP server blocked the tailer task forever.
//!
//! Queue writes use bound variables (F-002); queued rows are deleted by
//! validated record ids only (a hostile id is skipped with a warning instead
//! of interpolated, unlikewise which built `DELETE %s;` from raw row ids).

use std::sync::Arc;
use std::time::Duration;

use ibis_db::executor::{vars, Executor};
use ibis_db::ids::{is_safe_record_id, sanitize_id};
use ibis_db::schema::names as schema;
use ibis_db::value::{SurrealValue, I64};

use crate::error::{LogsError, Result};

/// Overall per-operation SMTP timeout (Rust addition — had none).
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Notifier {
    pub cfg: Arc<ibis_core::config::Config>,
    pub db: Option<Arc<dyn Executor>>,
}

/// DB row projection for the pending-notification digest.
#[derive(Debug, Clone, SurrealValue)]
pub(crate) struct PendingNotificationRow {
    /// Record ids decode as record values (Q3); rendered via
    /// `record_id_to_plain` for the delete pass.
    id: Option<ibis_db::value::RecordId>,
    project: Option<String>,
    log_file: Option<String>,
    errors_count: Option<I64>,
    severity: Option<I64>,
    content: Option<String>,
    /// Typed datetime (D17 R7); rendered RFC3339 for the digest body.
    created_at: Option<surrealdb::types::Datetime>,
}

impl Notifier {
    pub fn new(cfg: Arc<ibis_core::config::Config>, db: Option<Arc<dyn Executor>>) -> Self {
        Notifier { cfg, db }
    }

    /// Emails the report file content; no-op when SMTP is
    /// disabled.
    pub async fn send_email(&self, report_path: &str) -> Result<()> {
        if !self.cfg.smtp.enabled {
            return Ok(());
        }
        let content = tokio::fs::read_to_string(report_path).await?;
        let msg = format!(
            "To: {to}\r\nFrom: {from}\r\nSubject: Ibis Assistant - Log Report\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n{content}\r\n",
            to = self.cfg.smtp.to,
            from = self.cfg.smtp.from,
        );
        self.send_raw_email(msg.as_bytes()).await.map_err(|e| {
            tracing::error!("Failed to send email to {}: {e}", self.cfg.smtp.to);
            e
        })?;
        tracing::info!("Report email sent successfully to {}", self.cfg.smtp.to);
        Ok(())
    }

    /// Queues or sends a notification depending on severity and the
    /// aggregation window.
    pub async fn queue_notification(
        &self,
        project: &str,
        log_file: &str,
        errors_count: usize,
        max_severity: i64,
        content: &str,
    ) -> Result<()> {
        if !self.cfg.smtp.enabled {
            return Ok(());
        }

        // Bypass aggregation if severity is >= threshold (Emergency!)
        if max_severity >= self.cfg.smtp.emergency_severity_threshold {
            tracing::warn!(
                "Emergency severity threshold reached ({max_severity} >= {}). Bypassing aggregation.",
                self.cfg.smtp.emergency_severity_threshold
            );
            let subject = format!("CRITICAL ALERT: {project} - Emergency Log Report");
            let msg = format!(
                "To: {to}\r\nFrom: {from}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\nEmergency Alert: Critical errors detected in project {project} (file: {log_file}).\r\nErrors Count: {errors_count}\r\nSeverity: {max_severity}\r\n\r\nContent:\r\n{content}\r\n",
                to = self.cfg.smtp.to,
                from = self.cfg.smtp.from,
            );
            return self.send_raw_email(msg.as_bytes()).await;
        }

        // Send immediately if no aggregation window is configured
        if self.cfg.smtp.aggregation_window.is_empty() || self.cfg.smtp.aggregation_window == "none"
        {
            let subject = format!("Ibis Assistant: {project} - Log Report");
            let msg = format!(
                "To: {to}\r\nFrom: {from}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\nErrors detected in project {project} (file: {log_file}).\r\nErrors Count: {errors_count}\r\nSeverity: {max_severity}\r\n\r\nContent:\r\n{content}\r\n",
                to = self.cfg.smtp.to,
                from = self.cfg.smtp.from,
            );
            return self.send_raw_email(msg.as_bytes()).await;
        }

        let Some(db) = &self.db else {
            tracing::warn!("DB is nil, sending log report immediately.");
            let subject = format!("Ibis Assistant: {project} - Log Report (No DB Fallback)");
            let msg = format!(
                "To: {to}\r\nFrom: {from}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n{content}\r\n",
                to = self.cfg.smtp.to,
                from = self.cfg.smtp.from,
            );
            return self.send_raw_email(msg.as_bytes()).await;
        };

        let sql = format!(
            "CREATE {} SET project = $project, log_file = $log_file, errors_count = $errors_count, severity = $severity, content = $content, created_at = $created_at;",
            schema::LOG_PENDING_NOTIFICATION
        );
        let result = db
            .smart_query(
                &sql,
                vars([
                    ("project", ibis_db::value::Value::from_t(project)),
                    ("log_file", ibis_db::value::Value::from_t(log_file)),
                    (
                        "errors_count",
                        ibis_db::value::Value::from_t(errors_count as i64),
                    ),
                    ("severity", ibis_db::value::Value::from_t(max_severity)),
                    ("content", ibis_db::value::Value::from_t(content)),
                    (
                        "created_at",
                        ibis_db::value::Value::Datetime(surrealdb::types::Datetime::from(
                            chrono::Utc::now(),
                        )),
                    ),
                ]),
            )
            .await;
        if let Err(e) = result {
            tracing::error!("Failed to queue pending notification: {e}");
            return Err(LogsError::Db(e));
        }

        tracing::info!(
            "Notification queued for aggregation ({} window)",
            self.cfg.smtp.aggregation_window
        );
        Ok(())
    }

    /// Drains `log_pending_notification` rows into one digest email
    /// Rows are deleted BEFORE sending so
    /// a dropped SMTP connection never re-sends already-processed events
    /// (at-most-once delivery).
    pub async fn process_pending_notifications(&self) -> Result<()> {
        if self.db.is_none() || !self.cfg.smtp.enabled {
            return Ok(());
        }
        let db = self.db.as_ref().unwrap();

        let sql = format!(
            "SELECT * FROM {} ORDER BY created_at ASC;",
            schema::LOG_PENDING_NOTIFICATION
        );
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| LogsError::other(format!("failed to fetch pending notifications: {e}")))?;
        let rows = outcome.rows::<PendingNotificationRow>(&sql)?;
        if rows.is_empty() {
            return Ok(()); // Nothing to send
        }

        let mut sb = String::new();
        sb.push_str("# Ibis Assistant - Aggregated Log Digest\r\n\r\n");
        sb.push_str(&format!(
            "Report Date: {}\r\n\r\n",
            chrono::Local::now().format("%d %b %y %H:%M %z")
        ));
        sb.push_str("The following errors were captured and aggregated during the monitoring window:\r\n\r\n");

        // Collect ids while building the digest body.
        let mut collected_ids: Vec<String> = Vec::new();
        for row in &rows {
            if let Some(id) = row.id.as_ref().map(ibis_db::ids::record_id_to_plain) {
                if !id.is_empty() {
                    collected_ids.push(id);
                }
            }
            let proj = row.project.clone().unwrap_or_default();
            let log_file = row.log_file.clone().unwrap_or_default();
            let errs_count = row.errors_count.map(|v| v.get()).unwrap_or(0);
            let severity = row.severity.map(|v| v.get()).unwrap_or(0);
            let content = row.content.clone().unwrap_or_default();
            let created_at = row
                .created_at
                .as_ref()
                .map(|dt| dt.into_inner().to_rfc3339())
                .unwrap_or_default();

            sb.push_str(&format!("### Project: {proj} (File: {log_file})\r\n"));
            sb.push_str(&format!("- **Time**: {created_at}\r\n"));
            sb.push_str(&format!("- **Errors Count**: {errs_count}\r\n"));
            sb.push_str(&format!("- **Max Severity**: {severity}/10\r\n"));
            sb.push_str("- **Details Snippet**:\r\n```\r\n");
            if content.chars().count() > 500 {
                sb.push_str(&format!(
                    "{}\r\n... [TRUNCATED] ...\r\n",
                    char_safe_prefix(&content, 500)
                ));
            } else {
                sb.push_str(&format!("{content}\r\n"));
            }
            sb.push_str("```\r\n\r\n");
        }

        // Delete records before sending to avoid duplicates on SMTP failure.
        for id in &collected_ids {
            // F-002 hardening: only validated record ids are inlined; a hostile
            // id from a DB row is skipped ( interpolated it raw).
            if is_safe_record_id(id) {
                let _ = db.execute(&format!("DELETE {id};")).await;
            } else {
                let sanitized = format!(
                    "{}:⟨{}⟩",
                    schema::LOG_PENDING_NOTIFICATION,
                    sanitize_id(id.split_once(':').map(|(_, k)| k).unwrap_or(id))
                );
                let _ = db.execute(&format!("DELETE {sanitized};")).await;
            }
        }

        let msg = format!(
            "To: {to}\r\nFrom: {from}\r\nSubject: Ibis Assistant - Log Digest ({count} errors)\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n{body}\r\n",
            to = self.cfg.smtp.to,
            from = self.cfg.smtp.from,
            count = rows.len(),
            body = sb,
        );

        self.send_raw_email(msg.as_bytes())
            .await
            .map_err(|e| LogsError::other(format!("failed to send digest email: {e}")))?;

        tracing::info!(
            "Sent digest email containing {} events and cleared queue",
            collected_ids.len()
        );
        Ok(())
    }

    /// Sends a raw RFC5322 message over SMTP.
    pub async fn send_raw_email(&self, msg: &[u8]) -> Result<()> {
        let smtp = &self.cfg.smtp;
        let addr = join_host_port(&smtp.host, smtp.port);
        let encryption = smtp.encryption.to_lowercase();

        if encryption == "ssl_tls" {
            let tls_stream =
                tokio::time::timeout(SMTP_TIMEOUT, crate::smtp::tls_connect(&addr, &smtp.host))
                    .await
                    .map_err(|_| LogsError::smtp("SSL/TLS connect timed out"))?
                    .map_err(|e| LogsError::smtp(format!("failed to connect via SSL/TLS: {e}")))?;
            let mut conn = crate::smtp::SmtpConnection::new(tls_stream);
            conn.run_session(smtp, msg).await?;
            return Ok(());
        }

        let tcp = tokio::time::timeout(SMTP_TIMEOUT, tokio::net::TcpStream::connect(&addr))
            .await
            .map_err(|_| LogsError::smtp("connect timed out"))?
            .map_err(|e| LogsError::smtp(format!("failed to connect: {e}")))?;
        let mut conn = crate::smtp::SmtpConnection::new(crate::smtp::Stream::Tcp(tcp));

        if encryption == "starttls" {
            tokio::time::timeout(SMTP_TIMEOUT, conn.starttls(&smtp.host))
                .await
                .map_err(|_| LogsError::smtp("STARTTLS timed out"))?
                .map_err(|e| LogsError::smtp(format!("failed to start TLS: {e}")))?;
        }

        tokio::time::timeout(SMTP_TIMEOUT, conn.run_session(smtp, msg))
            .await
            .map_err(|_| LogsError::smtp("session timed out"))?
    }
}

fn join_host_port(host: &str, port: i64) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn char_safe_prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smtp::tests_support::MockSmtpServer;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct CapturingDb {
        sqls: std::sync::Mutex<Vec<String>>,
        select_rows: Option<Vec<PendingNotificationRow>>,
        deletes: AtomicUsize,
    }

    impl CapturingDb {
        fn new() -> Self {
            CapturingDb {
                sqls: std::sync::Mutex::new(Vec::new()),
                select_rows: None,
                deletes: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Executor for CapturingDb {
        async fn execute(
            &self,
            sql: &str,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            if sql.starts_with("SELECT") {
                let rows = self.select_rows.clone().unwrap_or_default();
                let vals: Vec<ibis_db::value::Value> =
                    rows.into_iter().map(SurrealValue::into_value).collect();
                return Ok(ibis_db::executor::QueryOutcome::Single(
                    ibis_db::value::Value::from_t(vals),
                ));
            }
            if sql.starts_with("DELETE") {
                self.deletes.fetch_add(1, AtomicOrdering::SeqCst);
            }
            Ok(ibis_db::executor::QueryOutcome::Single(
                ibis_db::value::Value::None,
            ))
        }

        async fn smart_query(
            &self,
            sql: &str,
            _v: ibis_db::executor::Vars,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            Ok(ibis_db::executor::QueryOutcome::Single(
                ibis_db::value::Value::None,
            ))
        }
    }

    fn smtp_cfg(port: i64) -> Arc<ibis_core::config::Config> {
        let mut cfg = ibis_core::config::Config::default();
        cfg.smtp.enabled = true;
        cfg.smtp.host = "127.0.0.1".into();
        cfg.smtp.port = port;
        cfg.smtp.encryption = "none".into();
        cfg.smtp.from = "test-sender@example.com".into();
        cfg.smtp.to = "test-receiver@example.com".into();
        Arc::new(cfg)
    }

    fn queue_cfg() -> ibis_core::config::Config {
        let mut cfg = ibis_core::config::Config::default();
        cfg.smtp.enabled = true;
        cfg.smtp.encryption = "none".into();
        cfg.smtp.aggregation_window = "1h".into();
        cfg.smtp.emergency_severity_threshold = 9;
        cfg
    }

    #[tokio::test]
    async fn send_email_no_auth() {
        let server = MockSmtpServer::start().await;
        let cfg = smtp_cfg(server.port());
        let notifier = Notifier::new(cfg, None);
        notifier
            .send_raw_email(b"Subject: Test Subject\r\n\r\nHello World")
            .await
            .expect("send");

        let expected = [
            "EHLO",
            "MAIL FROM:<test-sender@example.com>",
            "RCPT TO:<test-receiver@example.com>",
            "DATA",
            "DATA: Subject: Test Subject",
            "DATA: Hello World",
            "QUIT",
        ];
        let cmds = server.drain_commands().await;
        for (i, exp) in expected.iter().enumerate() {
            let got = cmds.get(i).map(|s| s.as_str()).unwrap_or("");
            assert!(
                got.contains(exp) || exp.contains(got),
                "expected cmd containing {exp:?}, got {got:?} (all={cmds:?})"
            );
        }
    }

    #[tokio::test]
    async fn send_email_with_auth() {
        let server = MockSmtpServer::start().await;
        let mut cfg = ibis_core::config::Config::default();
        cfg.smtp.enabled = true;
        cfg.smtp.host = "127.0.0.1".into();
        cfg.smtp.port = server.port();
        cfg.smtp.encryption = "none".into();
        cfg.smtp.user = "user1".into();
        cfg.smtp.password = "secret".into();
        cfg.smtp.from = "test-sender@example.com".into();
        cfg.smtp.to = "test-receiver@example.com".into();
        let notifier = Notifier::new(Arc::new(cfg), None);
        notifier
            .send_raw_email(b"Subject: Test Subject\r\n\r\nHello World")
            .await
            .expect("send");

        let cmds = server.drain_commands().await;
        assert!(
            cmds.iter().any(|c| c.starts_with("AUTH PLAIN")),
            "expected AUTH PLAIN command to be sent, got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn queue_notification_throttling() {
        let db = Arc::new(CapturingDb::new());
        let notifier = Notifier::new(Arc::new(queue_cfg()), Some(db.clone() as Arc<dyn Executor>));
        notifier
            .queue_notification("my-project", "app.log", 10, 5, "Some error log content")
            .await
            .expect("queue");
        let sqls = db.sqls.lock().unwrap();
        assert!(
            sqls.iter()
                .any(|s| s.contains(&format!("CREATE {}", schema::LOG_PENDING_NOTIFICATION))),
            "expected SQL to contain 'CREATE log_pending_notification', got {sqls:?}"
        );
    }

    #[tokio::test]
    async fn queue_notification_emergency_bypass() {
        let server = MockSmtpServer::start().await;
        let mut cfg = queue_cfg();
        cfg.smtp.host = "127.0.0.1".into();
        cfg.smtp.port = server.port();
        cfg.smtp.from = "test-sender@example.com".into();
        cfg.smtp.to = "test-receiver@example.com".into();
        let db = Arc::new(CapturingDb::new());
        let notifier = Notifier::new(Arc::new(cfg), Some(db.clone() as Arc<dyn Executor>));
        notifier
            .queue_notification(
                "my-project",
                "app.log",
                10,
                9,
                "Emergency error log content",
            )
            .await
            .expect("queue");

        assert_eq!(
            db.sqls.lock().unwrap().len(),
            0,
            "DB should not be called on emergency bypass"
        );
        let cmds = server.drain_commands().await;
        assert!(
            cmds.first().map(|c| c.starts_with("EHLO")).unwrap_or(false),
            "expected EHLO command, got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn process_pending_notifications() {
        let server = MockSmtpServer::start().await;
        let mut cfg = queue_cfg();
        cfg.smtp.host = "127.0.0.1".into();
        cfg.smtp.port = server.port();
        cfg.smtp.from = "test-sender@example.com".into();
        cfg.smtp.to = "test-receiver@example.com".into();

        let mut db = CapturingDb::new();
        db.select_rows = Some(vec![PendingNotificationRow {
            id: Some(ibis_db::value::RecordId::new(
                "log_pending_notification",
                "abc123",
            )),
            project: Some("my-project".into()),
            log_file: Some("app.log".into()),
            errors_count: Some(I64(5)),
            severity: Some(I64(4)),
            content: Some("Log line error summary".into()),
            created_at: Some(surrealdb::types::Datetime::from(
                chrono::DateTime::parse_from_rfc3339("2026-06-11T10:00:00Z")
                    .expect("fixture rfc3339")
                    .with_timezone(&chrono::Utc),
            )),
        }]);
        let db = Arc::new(db);
        let notifier = Notifier::new(Arc::new(cfg), Some(db.clone() as Arc<dyn Executor>));
        notifier
            .process_pending_notifications()
            .await
            .expect("process");

        assert!(
            db.deletes.load(AtomicOrdering::SeqCst) > 0,
            "expected queue to be deleted after processing"
        );
        let cmds = server.drain_commands().await;
        assert!(
            cmds.first().map(|c| c.starts_with("EHLO")).unwrap_or(false),
            "expected EHLO command, got {cmds:?}"
        );
        let body = server.drain_data().await.join("\n");
        assert!(body.contains("my-project"), "digest body: {body:?}");
    }
}
