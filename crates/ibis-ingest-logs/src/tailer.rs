//! Offset-aware log tailer.
//!
//! Semantics preserved: DB-offset resume (`log_file.offset`), follow + reopen
//! (missing/truncated files are retried; truncation resets to 0), format
//! discovery after 50 lines, process at ≥100 lines with the entry-regex cut
//! (or ≥500 fallback), 2s idle ticker flush, and a report + notification on
//! every flush that produced anomalies.
//!
//! Lifecycle: the tailer runs as an owned tokio task driven by a shutdown
//! `watch` channel (the watcher owns the sender). spawned a detached
//! goroutine for cancellation and never joined it — Rust tasks join on stop
//! (finding G-A09). The custom reader caps per-line bytes (G-A08) and bounds
//! the per-poll read.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use crate::analyzer::LogAnalyzer;
use crate::notifier::Notifier;
use crate::reporter::Reporter;

/// Default file-poll interval (`tail.Config{Poll: true} used ~250ms).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Idle-flush ticker.
pub const DEFAULT_TICKER_INTERVAL: Duration = Duration::from_secs(2);

/// Per-line byte cap — a pathological single line cannot OOM the process
/// (Rust addition; read lines unbounded, finding G-A08).
pub const DEFAULT_MAX_LINE_BYTES: usize = 4 << 20;

/// Per-poll read cap to bound latency/memory spikes between ticks.
const MAX_READ_PER_POLL: usize = 8 << 20;

pub struct Tailer {
    pub path: String,
    pub cfg: Option<Arc<ibis_core::config::Config>>,
    pub db: Option<Arc<dyn ibis_db::executor::Executor>>,
    pub ai: Option<Arc<ibis_ai::Client>>,
    pub analyzer: Arc<LogAnalyzer>,
    /// Test seam: poll interval (default 250ms).
    pub poll_interval: Duration,
    /// Test seam: idle-flush ticker (default 2s).
    pub ticker_interval: Duration,
    /// Per-line cap (default 4 MiB).
    pub max_line_bytes: usize,
}

impl Tailer {
    /// Builds a tailer with an analyzer bound to the explicit project
    /// (`NewTailer inferred the project from the parent directory of the
    /// path when it is not the logs_root basename; the analyzer constructor
    /// performs the same inference when project is empty).
    pub async fn new(
        path: &str,
        project: &str,
        cfg: Option<Arc<ibis_core::config::Config>>,
        db: Option<Arc<dyn ibis_db::executor::Executor>>,
        ai: Option<Arc<ibis_ai::Client>>,
    ) -> Self {
        let analyzer = Arc::new(
            LogAnalyzer::new_with_project(path, project, cfg.clone(), db.clone(), ai.clone()).await,
        );
        Tailer {
            path: path.to_string(),
            cfg,
            db,
            ai,
            analyzer,
            poll_interval: DEFAULT_POLL_INTERVAL,
            ticker_interval: DEFAULT_TICKER_INTERVAL,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }

    /// Runs the tail loop until `shutdown` fires, then flushes the pending
    /// batch, emits a final report and returns.
    ///
    /// C3/F11: read offset and **confirmed** offset are separate. Every batch
    /// carries the byte offset its last line ends at; batches queue in order and
    /// the persisted offset advances only after a batch analysis SUCCEEDS. A
    /// failed batch blocks the queue (later batches cannot make the skip
    /// permanent) and is retried on the next tick/poll.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        // Retrieve the last processed position to ensure idempotency across
        // restarts.
        let mut offset = self.analyzer.get_offset().await;
        tracing::info!(
            "Initializing log tailer for: {} (Resuming from offset: {offset})",
            self.path
        );

        // (line, end_offset after consuming that line).
        let mut batch: Vec<(String, i64)> = Vec::new();
        let mut pending: VecDeque<PendingBatch> = VecDeque::new();
        let mut all_anomalies: Vec<crate::analyzer::SemanticError> = Vec::new();
        let mut discovery_done = false;
        let mut chunk_regex: Option<Regex> = None;
        let mut partial = Vec::<u8>::new();
        let mut oversized_logged = false;
        let mut missing_logged = false;
        let mut ticker = tokio::time::interval(self.ticker_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !batch.is_empty() {
                        let taken = std::mem::take(&mut batch);
                        pending.push_back(PendingBatch {
                            lines: taken.iter().map(|(l, _)| l.clone()).collect(),
                            end_offset: taken.last().map(|(_, e)| *e).unwrap_or(0),
                        });
                    }
                    self.drain_pending(&mut pending, &mut all_anomalies).await;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        // Watcher dropped the sender: treat as shutdown.
                    } else if !*shutdown.borrow() {
                        continue;
                    }
                    if !batch.is_empty() {
                        let taken = std::mem::take(&mut batch);
                        pending.push_back(PendingBatch {
                            lines: taken.iter().map(|(l, _)| l.clone()).collect(),
                            end_offset: taken.last().map(|(_, e)| *e).unwrap_or(0),
                        });
                    }
                    self.drain_pending(&mut pending, &mut all_anomalies).await;
                    self.finalize(&all_anomalies).await;
                    tracing::info!("Context cancelled, stopping tail on: {}", self.path);
                    return;
                }
                _ = tokio::time::sleep(self.poll_interval) => {
                    match self.read_new_lines(&mut offset, &mut partial, &mut batch, &mut oversized_logged, &mut missing_logged).await {
                        ReadOutcome::Lines(lines) => {
                            for text in lines {
                                batch.push(text.clone());

                                // Format discovery after 50 lines ( loop body).
                                if !discovery_done && batch.len() >= 50 {
                                    let joined: Vec<String> =
                                        batch.iter().map(|(l, _)| l.clone()).collect();
                                    let regex_str = self.analyzer.discover_format(&joined).await;
                                    if !regex_str.is_empty() {
                                        if let Ok(r) = Regex::new(&regex_str) {
                                            tracing::info!("Discovered log chunk delimiter: {regex_str}");
                                            chunk_regex = Some(r);
                                        }
                                    }
                                    discovery_done = true;
                                }

                                // Batch-processing decision ( loop body).
                                let mut should_process = false;
                                if batch.len() >= 100 {
                                    if let Some(re) = &chunk_regex {
                                        if re.is_match(text.0.as_str()) || batch.len() >= 500 {
                                            should_process = true;
                                        }
                                    } else {
                                        should_process = true;
                                    }
                                }

                                if should_process {
                                    // A line matching the entry regex starts a
                                    // new entry: process everything before it and
                                    // keep it as the next batch seed.
                                    let end_offset = if chunk_regex.as_ref().map(|re| re.is_match(text.0.as_str())).unwrap_or(false) {
                                        // Head = everything before the seed line:
                                        // its last line is batch[n-2].
                                        let head_end = batch.len()
                                            .checked_sub(2)
                                            .map(|i| batch[i].1)
                                            .unwrap_or(0);
                                        let head: Vec<(String, i64)> =
                                            batch[..batch.len() - 1].to_vec();
                                        batch = vec![text.clone()];
                                        PendingBatch {
                                            lines: head.iter().map(|(l, _)| l.clone()).collect(),
                                            end_offset: head_end,
                                        }
                                    } else {
                                        let taken = std::mem::take(&mut batch);
                                        batch = Vec::new();
                                        PendingBatch {
                                            lines: taken.iter().map(|(l, _)| l.clone()).collect(),
                                            end_offset: taken.last().map(|(_, e)| *e).unwrap_or(0),
                                        }
                                    };
                                    pending.push_back(end_offset);
                                    self.drain_pending(&mut pending, &mut all_anomalies).await;
                                }
                            }
                        }
                        ReadOutcome::Idle => {}
                        ReadOutcome::Gone => {
                            // File missing or unreadable: keep polling (ReOpen
                            // semantics — the file may be rotated back in).
                        }
                    }
                }
            }
        }
    }
}

/// One queued batch with the byte offset its last line ends at (C3/F11).
struct PendingBatch {
    lines: Vec<String>,
    end_offset: i64,
}

impl Tailer {
    /// Processes queued batches strictly in order. A failed batch stops the
    /// drain (it is retried on the next tick/poll and later batches stay
    /// queued behind it) and never advances the persisted offset.
    async fn drain_pending(
        &self,
        pending: &mut VecDeque<PendingBatch>,
        all_anomalies: &mut Vec<crate::analyzer::SemanticError>,
    ) {
        while let Some(front) = pending.front() {
            let end = front.end_offset;
            let lines = front.lines.clone();
            let (anomalies, err) = self.analyzer.process_batch_sync(&lines).await;
            if let Some(err) = err {
                tracing::error!(
                    "Log batch analysis failed for {} (retry queued at offset {end}): {err}",
                    self.path
                );
                break;
            }
            if !anomalies.is_empty() {
                all_anomalies.extend(anomalies.iter().cloned());
                self.emit_report(all_anomalies).await;
            }
            // Success only now: persist the confirmed offset.
            self.analyzer.update_offset(end).await;
            pending.pop_front();
        }
    }

    /// Writes the sidecar markdown report and queues/sends the notification
    ///
    async fn emit_report(&self, anomalies: &[crate::analyzer::SemanticError]) {
        if anomalies.is_empty() {
            return;
        }
        let reporter = Reporter::new(self.cfg.clone());
        let cost = anomalies.len() as f64 * 0.0001;
        let (report_path, content) = match reporter
            .generate_report(&self.analyzer.project, &self.path, anomalies, cost)
            .await
        {
            Ok(pc) => pc,
            Err(err) => {
                tracing::error!("Failed to generate report for {}: {err}", self.path);
                return;
            }
        };
        // The reporter returns the content directly — re-read the file
        // from disk (finding G-A20).
        let notifier = Notifier::new(self.cfg.clone().unwrap_or_default(), self.db.clone());
        let _ = notifier
            .queue_notification(
                &self.analyzer.project,
                &self.path,
                anomalies.len(),
                5,
                &content,
            )
            .await;
        let _ = report_path;
    }

    async fn finalize(&self, anomalies: &[crate::analyzer::SemanticError]) {
        self.emit_report(anomalies).await;
    }

    /// Reads newly appended bytes, returning complete lines with the byte
    /// offset each line ends at (trimmed and capped). Advances `offset` only
    /// past complete lines; a partial trailing line stays buffered for the next
    /// poll. Handles rotation/truncation (size below the resume offset →
    /// restart from 0).
    async fn read_new_lines(
        &self,
        offset: &mut i64,
        partial: &mut Vec<u8>,
        batch: &mut Vec<(String, i64)>,
        oversized_logged: &mut bool,
        missing_logged: &mut bool,
    ) -> ReadOutcome {
        let path = Path::new(&self.path);
        let Ok(meta) = tokio::fs::metadata(path).await else {
            if !*missing_logged {
                tracing::warn!("Tailer: file not readable yet: {}", self.path);
                *missing_logged = true;
            }
            return ReadOutcome::Gone;
        };
        let size = meta.len() as i64;
        if size < *offset + partial.len() as i64 {
            // Truncated or rotated: restart from the beginning.
            tracing::info!(
                "Tailer: file truncated/rotated, restarting from 0: {}",
                self.path
            );
            *offset = 0;
            partial.clear();
            batch.clear();
        }

        let Ok(mut file) = tokio::fs::File::open(path).await else {
            return ReadOutcome::Gone;
        };
        if file
            .seek(std::io::SeekFrom::Start(*offset as u64))
            .await
            .is_err()
        {
            return ReadOutcome::Gone;
        }

        // Read up to the per-poll cap.
        let budget = MAX_READ_PER_POLL - partial.len().min(MAX_READ_PER_POLL);
        let mut buf = vec![0u8; budget];
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return ReadOutcome::Gone,
        };
        if n == 0 {
            return ReadOutcome::Idle;
        }
        partial.extend_from_slice(&buf[..n]);

        // Split complete lines; keep the trailing partial in the buffer. Each
        // line carries the byte offset it ends at (C3/F11).
        let mut lines: Vec<(String, i64)> = Vec::new();
        let mut consumed = 0usize;
        let mut start = 0usize;
        for (i, b) in partial.iter().enumerate() {
            if *b != b'\n' {
                continue;
            }
            let mut line_bytes = &partial[start..i];
            if line_bytes.last() == Some(&b'\r') {
                line_bytes = &line_bytes[..line_bytes.len() - 1];
            }
            consumed = i + 1;
            start = i + 1;
            let line_end = *offset + consumed as i64;
            let text = String::from_utf8_lossy(line_bytes);
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            if line_bytes.len() > self.max_line_bytes {
                if !*oversized_logged {
                    tracing::warn!(
                        "Tailer: line exceeds {} bytes in {}, truncated",
                        self.max_line_bytes,
                        self.path
                    );
                    *oversized_logged = true;
                }
                let capped: String =
                    String::from_utf8_lossy(&line_bytes[..self.max_line_bytes]).into_owned();
                lines.push((capped.trim().to_string(), line_end));
                continue;
            }
            lines.push((text, line_end));
        }
        partial.drain(..consumed);
        *offset += consumed as i64;
        ReadOutcome::Lines(lines)
    }
}

enum ReadOutcome {
    Lines(Vec<(String, i64)>),
    Idle,
    Gone,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::ProgressReporter;
    use std::sync::atomic::AtomicUsize;

    /// Fake AI provider: functional, replies `[]` (no anomalies) so batch
    /// analysis succeeds and offsets advance without network access.
    struct FakeAI {
        reply: String,
    }

    #[async_trait::async_trait]
    impl ibis_ai::AIProvider for FakeAI {
        fn name(&self) -> &str {
            "fake-ai"
        }
        async fn is_functional(&self) -> bool {
            true
        }
        async fn generate_content(
            &self,
            _contents: &[ibis_ai::types::Content],
            _config: ibis_ai::types::GenerationConfig,
        ) -> std::result::Result<ibis_ai::types::Candidate, ibis_ai::AiError> {
            Ok(ibis_ai::types::Candidate {
                content: ibis_ai::types::Content {
                    parts: vec![ibis_ai::types::Part {
                        text: self.reply.clone(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            })
        }
    }

    fn fake_ai() -> Arc<ibis_ai::Client> {
        fake_ai_with_reply("[]")
    }

    fn fake_ai_with_reply(reply: &str) -> Arc<ibis_ai::Client> {
        let provider = Arc::new(FakeAI {
            reply: reply.to_string(),
        }) as Arc<dyn ibis_ai::AIProvider>;
        Arc::new(ibis_ai::Client::new(
            Some(provider.clone()),
            Some(provider),
            None,
        ))
    }

    struct CountingDb {
        offsets: std::sync::Mutex<Vec<i64>>,
        sqls: std::sync::Mutex<Vec<String>>,
    }

    impl CountingDb {
        fn new() -> Self {
            CountingDb {
                offsets: std::sync::Mutex::new(Vec::new()),
                sqls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn last_offset(&self) -> i64 {
            *self.offsets.lock().unwrap().last().unwrap_or(&0)
        }
    }

    #[async_trait::async_trait]
    impl ibis_db::executor::Executor for CountingDb {
        async fn execute(
            &self,
            sql: &str,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            Ok(ibis_db::executor::QueryOutcome::Single(
                ibis_db::value::Value::None,
            ))
        }

        async fn smart_query(
            &self,
            sql: &str,
            v: ibis_db::executor::Vars,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            if sql.contains("SET offset") {
                let n = v
                    .get("offset")
                    .and_then(|val| match val {
                        ibis_db::value::Value::Number(n) => n.to_int(),
                        _ => None,
                    })
                    .unwrap_or(0);
                self.offsets.lock().unwrap().push(n);
            }
            if sql.contains("SELECT offset") {
                // Fresh record: NONE result decodes to empty rows.
                return Ok(ibis_db::executor::QueryOutcome::Single(
                    ibis_db::value::Value::None,
                ));
            }
            Ok(ibis_db::executor::QueryOutcome::Single(
                ibis_db::value::Value::None,
            ))
        }
    }

    struct NoopReporter;
    impl ProgressReporter for NoopReporter {
        fn report(&self, _done: u64, _total: u64, _message: &str) {}
    }

    fn make_tailer(path: &str, db: Arc<CountingDb>) -> Arc<Tailer> {
        make_tailer_with(path, db, DEFAULT_MAX_LINE_BYTES)
    }

    fn make_tailer_with(path: &str, db: Arc<CountingDb>, max_line_bytes: usize) -> Arc<Tailer> {
        let mut analyzer = LogAnalyzer::bare("tailer-test");
        analyzer.path = path.to_string();
        analyzer.log_file_id = format!(
            "log_file:⟨{}⟩",
            ibis_db::ids::sanitize_id(
                Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default()
                    .as_ref()
            )
        );
        analyzer.db = Some(db.clone() as Arc<dyn ibis_db::executor::Executor>);
        analyzer.progress = Some(Arc::new(NoopReporter));
        let ai = fake_ai();
        analyzer.ai = Some(ai.clone());
        Arc::new(Tailer {
            path: path.to_string(),
            cfg: None,
            db: Some(db as Arc<dyn ibis_db::executor::Executor>),
            ai: Some(ai),
            analyzer: Arc::new(analyzer),
            poll_interval: Duration::from_millis(20),
            ticker_interval: Duration::from_millis(120),
            max_line_bytes,
        })
    }

    async fn run_until_shutdown(tailer: Arc<Tailer>, work: impl std::future::Future<Output = ()>) {
        let (tx, rx) = watch::channel(false);
        let runner = tailer.clone();
        let task = tokio::spawn(async move { runner.run(rx).await });
        work.await;
        tx.send(true).ok();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if task.is_finished() {
                break;
            }
        }
        assert!(task.is_finished(), "tailer must stop after shutdown");
    }

    #[tokio::test]
    async fn tails_appended_lines_and_updates_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("app.log");
        std::fs::write(&log, "ERROR first line\n").unwrap();

        let db = Arc::new(CountingDb::new());
        let tailer = make_tailer(log.to_str().unwrap(), db.clone());

        let (tx, rx) = watch::channel(false);
        let runner = tailer.clone();
        let task = tokio::spawn(async move { runner.run(rx).await });

        // Let the first line be consumed.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Append more lines (same prefix → append semantics); second batch processed.
        std::fs::write(&log, "ERROR first line\nERROR second line\nERROR third\n").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        tx.send(true).ok();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(task.is_finished(), "tailer must stop after shutdown");

        let offsets = db.offsets.lock().unwrap();
        assert!(
            offsets.iter().any(|o| *o > 0),
            "offset must be persisted after processing, got {offsets:?}"
        );
        let full = "ERROR first line\nERROR second line\nERROR third\n".len() as i64;
        assert!(
            *offsets.last().unwrap() >= full,
            "final offset must cover all bytes, got {:?}",
            offsets.last()
        );
    }

    #[tokio::test]
    async fn truncation_restarts_from_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("app.log");
        std::fs::write(&log, "line one\nline two\nline three\n").unwrap();

        // Serves offset=24 ("line one\nline two\n") so the tailer resumes from
        // a mid-file position that no longer exists after truncation.
        struct SeededOffsetDb(AtomicUsize);
        #[async_trait::async_trait]
        impl ibis_db::executor::Executor for SeededOffsetDb {
            async fn execute(
                &self,
                sql: &str,
            ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError>
            {
                if sql.contains("SELECT offset") {
                    let mut obj = ibis_db::value::Object::new();
                    obj.insert(
                        "offset",
                        ibis_db::value::Value::Number(ibis_db::value::Number::Int(24)),
                    );
                    return Ok(ibis_db::executor::QueryOutcome::Single(
                        ibis_db::value::Value::Object(obj),
                    ));
                }
                Ok(ibis_db::executor::QueryOutcome::Single(
                    ibis_db::value::Value::None,
                ))
            }
            async fn smart_query(
                &self,
                sql: &str,
                v: ibis_db::executor::Vars,
            ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError>
            {
                if sql.contains("SET offset") {
                    let n = v
                        .get("offset")
                        .and_then(|val| match val {
                            ibis_db::value::Value::Number(n) => n.to_int(),
                            _ => None,
                        })
                        .unwrap_or(0);
                    self.0
                        .store(n as usize, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(ibis_db::executor::QueryOutcome::Single(
                    ibis_db::value::Value::None,
                ))
            }
        }
        let db = Arc::new(SeededOffsetDb(AtomicUsize::new(0)));

        let mut analyzer = LogAnalyzer::bare("tailer-test");
        analyzer.path = log.to_str().unwrap().to_string();
        analyzer.log_file_id = "log_file:⟨app_log⟩".into();
        analyzer.db = Some(db.clone() as Arc<dyn ibis_db::executor::Executor>);
        analyzer.progress = Some(Arc::new(NoopReporter));
        let ai = fake_ai();
        analyzer.ai = Some(ai.clone());
        let tailer = Arc::new(Tailer {
            path: log.to_str().unwrap().to_string(),
            cfg: None,
            db: Some(db.clone() as Arc<dyn ibis_db::executor::Executor>),
            ai: Some(ai),
            analyzer: Arc::new(analyzer),
            poll_interval: Duration::from_millis(20),
            ticker_interval: Duration::from_millis(120),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        });

        assert_eq!(tailer.analyzer.get_offset().await, 24);
        // Truncate far below the recorded offset → restart from 0.
        std::fs::write(&log, "fresh start\n").unwrap();

        run_until_shutdown(tailer, tokio::time::sleep(Duration::from_millis(200))).await;
        // After restart-from-zero the whole file is consumed (12 bytes).
        assert_eq!(db.0.load(std::sync::atomic::Ordering::SeqCst), 12);
    }

    #[tokio::test]
    async fn oversized_line_is_capped_not_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("big.log");
        let big_line = format!("ERROR {}\n", "x".repeat(64));
        std::fs::write(&log, &big_line).unwrap();

        let db = Arc::new(CountingDb::new());
        let tailer = make_tailer_with(log.to_str().unwrap(), db.clone(), 16);
        run_until_shutdown(tailer, tokio::time::sleep(Duration::from_millis(200))).await;
        // The oversized line was capped and the loop survived (offset advanced
        // past the full oversized line).
        assert_eq!(db.last_offset(), big_line.len() as i64);
    }

    /// Fails the first N `UPSERT error_type` persists, then recovers — the
    /// C3/F11 injection seam on top of CountingDb.
    struct FlakyDb {
        inner: CountingDb,
        remaining_failures: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ibis_db::executor::Executor for FlakyDb {
        async fn execute(
            &self,
            sql: &str,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.inner.execute(sql).await
        }

        async fn smart_query(
            &self,
            sql: &str,
            v: ibis_db::executor::Vars,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            if sql.contains("UPSERT error_type") {
                let left = self
                    .remaining_failures
                    .load(std::sync::atomic::Ordering::SeqCst);
                if left > 0 {
                    self.remaining_failures
                        .store(left - 1, std::sync::atomic::Ordering::SeqCst);
                    return Err(ibis_db::DbError::Query {
                        message: "injected flaky failure".into(),
                        duration_ms: 0,
                    });
                }
            }
            self.inner.smart_query(sql, v).await
        }
    }

    /// C3/F11: a restart while the batch is still failed persists nothing —
    /// the next run re-reads from the old offset and processes the batch.
    #[tokio::test]
    async fn restart_before_retry_does_not_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("restart.log");
        let first = "ERROR restart batch line\n";
        std::fs::write(&log, first).unwrap();

        let db = Arc::new(FlakyDb {
            inner: CountingDb::new(),
            // Fail "forever" during the first run (u64-scale budget).
            remaining_failures: AtomicUsize::new(usize::MAX / 2),
        });
        let build_tailer = |analyzer_db: Arc<dyn ibis_db::executor::Executor>,
                            ai: Arc<ibis_ai::Client>|
         -> Arc<Tailer> {
            let mut analyzer = LogAnalyzer::bare("restart-test");
            analyzer.path = log.to_str().unwrap().to_string();
            analyzer.log_file_id = "log_file:⟨restart_log⟩".into();
            analyzer.db = Some(analyzer_db);
            analyzer.progress = Some(Arc::new(NoopReporter));
            analyzer.ai = Some(ai);
            Arc::new(Tailer {
                path: log.to_str().unwrap().to_string(),
                cfg: None,
                db: None,
                ai: None,
                analyzer: Arc::new(analyzer),
                poll_interval: Duration::from_millis(20),
                ticker_interval: Duration::from_millis(120),
                max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            })
        };

        // Run 1: failing persist → shutdown before any retry succeeds.
        {
            let ai = fake_ai_with_reply(
                r#"[{"category":"RESTART","stack_trace":"boom","file":"","line":0,"symbol":"","cause":"c","severity":5,"template":"boom <VAR>","supersedes":[]}]"#,
            );
            let tailer = build_tailer(db.clone() as Arc<dyn ibis_db::executor::Executor>, ai);
            let (tx, rx) = watch::channel(false);
            let runner = tailer.clone();
            let task = tokio::spawn(async move { runner.run(rx).await });
            tokio::time::sleep(Duration::from_millis(250)).await;
            tx.send(true).ok();
            tokio::time::sleep(Duration::from_millis(120)).await;
            assert!(task.is_finished(), "tailer must stop after shutdown");
            let offsets = db.inner.offsets.lock().unwrap();
            assert!(
                offsets.is_empty(),
                "a restart before the retry persists no offset: {offsets:?}"
            );
        }

        // Run 2: fresh tailer on the (now healthy) DB — the batch is
        // re-processed from the unadvanced offset and the offset lands on
        // its end.
        {
            let mut db = db;
            if let Some(inner) = Arc::get_mut(&mut db) {
                inner
                    .remaining_failures
                    .store(0, std::sync::atomic::Ordering::SeqCst);
            } else {
                panic!("run 1's tailer must have dropped its Arc before run 2");
            }
            let ai = fake_ai_with_reply(
                r#"[{"category":"RESTART","stack_trace":"boom","file":"","line":0,"symbol":"","cause":"c","severity":5,"template":"boom <VAR>","supersedes":[]}]"#,
            );
            let tailer = build_tailer(db.clone() as Arc<dyn ibis_db::executor::Executor>, ai);
            let (tx, rx) = watch::channel(false);
            let runner = tailer.clone();
            let task = tokio::spawn(async move { runner.run(rx).await });
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let done = {
                    let offsets = db.inner.offsets.lock().unwrap();
                    offsets.iter().any(|o| *o >= first.len() as i64)
                };
                if done || tokio::time::Instant::now() > deadline {
                    break;
                }
            }
            tx.send(true).ok();
            tokio::time::sleep(Duration::from_millis(120)).await;
            assert!(task.is_finished(), "tailer must stop after shutdown");
            let offsets = db.inner.offsets.lock().unwrap();
            assert!(
                offsets.contains(&(first.len() as i64)),
                "the restarted run re-processes the batch and persists its exact end offset: {offsets:?}"
            );
            assert!(
                offsets.iter().all(|o| *o <= first.len() as i64),
                "nothing beyond the batch was persisted: {offsets:?}"
            );
        }
    }
    #[tokio::test]
    async fn failed_batch_retries_before_offset_advances() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("flaky.log");
        let first = "ERROR first batch line\n";
        std::fs::write(&log, first).unwrap();

        let db = Arc::new(FlakyDb {
            inner: CountingDb::new(),
            remaining_failures: AtomicUsize::new(2), // fail the first 2 attempts
        });
        let mut analyzer = LogAnalyzer::bare("flaky-test");
        analyzer.path = log.to_str().unwrap().to_string();
        analyzer.log_file_id = format!(
            "log_file:⟨{}⟩",
            ibis_db::ids::sanitize_id(
                Path::new(&log)
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default()
                    .as_ref()
            )
        );
        analyzer.db = Some(db.clone() as Arc<dyn ibis_db::executor::Executor>);
        analyzer.progress = Some(Arc::new(NoopReporter));
        // The AI classifies an anomaly so the persist path (and the flaky
        // failure) actually runs — an empty classification would succeed
        // without touching error_type.
        let ai = fake_ai_with_reply(
            r#"[{"category":"FLAKY","stack_trace":"boom","file":"","line":0,"symbol":"","cause":"c","severity":5,"template":"boom <VAR>","supersedes":[]}]"#,
        );
        analyzer.ai = Some(ai.clone());
        let tailer = Arc::new(Tailer {
            path: log.to_str().unwrap().to_string(),
            cfg: None,
            db: Some(db.clone() as Arc<dyn ibis_db::executor::Executor>),
            ai: Some(ai),
            analyzer: Arc::new(analyzer),
            poll_interval: Duration::from_millis(20),
            ticker_interval: Duration::from_millis(120),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        });

        let (tx, rx) = watch::channel(false);
        let runner = tailer.clone();
        let task = tokio::spawn(async move { runner.run(rx).await });

        // Phase 1: wait for the two injected failures to be consumed. While
        // the batch keeps failing, no offset may persist.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let consumed = db
                .remaining_failures
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0;
            if consumed || tokio::time::Instant::now() > deadline {
                break;
            }
        }
        assert!(
            db.remaining_failures
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0,
            "the two injected failures must have been attempted"
        );
        {
            let offsets = db.inner.offsets.lock().unwrap();
            assert!(
                offsets.is_empty(),
                "no offset may persist while the batch fails: {offsets:?}"
            );
        }

        // Phase 2: recover (failures exhausted) → the queued batch 1 retries
        // on the next tick and succeeds; the offset lands exactly at the end
        // of the FIRST batch, and nothing beyond it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let saved = {
                let offsets = db.inner.offsets.lock().unwrap();
                offsets.contains(&(first.len() as i64))
            };
            if saved || tokio::time::Instant::now() > deadline {
                break;
            }
        }
        {
            let offsets = db.inner.offsets.lock().unwrap();
            assert!(
                offsets.contains(&(first.len() as i64)),
                "the failed batch must be retried and its exact end offset saved: {offsets:?}"
            );
            assert!(
                offsets.iter().all(|o| *o <= first.len() as i64),
                "no offset beyond the first batch may be saved before it succeeded: {offsets:?}"
            );
        }

        // Phase 3: append a second batch; its offset persists only after the
        // first one is confirmed.
        let second = "ERROR second batch line\n";
        std::fs::write(&log, format!("{first}{second}")).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let done = {
                let offsets = db.inner.offsets.lock().unwrap();
                offsets
                    .last()
                    .map(|o| *o >= (first.len() + second.len()) as i64)
                    .unwrap_or(false)
            };
            if done || tokio::time::Instant::now() > deadline {
                break;
            }
        }

        tx.send(true).ok();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(task.is_finished(), "tailer must stop after shutdown");
        {
            let offsets = db.inner.offsets.lock().unwrap();
            let full = (first.len() + second.len()) as i64;
            assert!(
                offsets.last().map(|o| *o >= full).unwrap_or(false),
                "final offset covers both batches: {offsets:?}"
            );
            // Monotonic persistence: every save advances, none skips a
            // failed batch.
            for w in offsets.windows(2) {
                assert!(w[1] >= w[0], "offsets must be monotonic: {offsets:?}");
            }
        }
    }
}
