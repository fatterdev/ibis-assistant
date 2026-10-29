//! `[degraded]` observability gate
//! (PR #34): the server line fires ONLY when a degraded claim read served
//! >0 claims; zero-claim reads stay silent.
//!
//! The test captured the stdlib `log` writer and asserted on the exact
//! `logger.Info` text; the Rust hook (`threski_server::tools_memory::
//! log_degraded_claim_read`) emits through `tracing`, so the capture is a
//! `tracing` fmt subscriber writing into a shared buffer. Assertion
//! semantics match exactly: one line, exact text, no spam.

use std::sync::{Arc, Mutex};

use threski_server::tools_memory::log_degraded_claim_read;

/// In-memory writer shared with the fmt subscriber.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// the `logger.Init("", "INFO", true)` equivalent for the capture window:
/// an INFO-level fmt subscriber whose writer is the shared buffer. The
/// subscriber is installed as a thread-local default via `with_default`, so
/// nothing leaks into the global subscriber or other tests ( restored the
/// previous writer in a `t.Cleanup`; the Rust scope restores nothing).
fn captured<F: FnOnce()>(f: F) -> String {
    let shared = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let writer = shared.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_target(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = shared.0.lock().unwrap().clone();
    String::from_utf8(bytes).expect("capture is utf-8")
}

/// `TestLogDegradedClaimRead: the >0-claim read emits the exact
/// `[degraded]` line; the two zero-claim reads stay silent.
#[test]
fn degraded_read_line_fires_only_for_nonzero_claims() {
    let out = captured(|| {
        log_degraded_claim_read("as_of_project", "repo", ", ref=abc", 2);
        log_degraded_claim_read("ask_project", "repo", ", ref=abc", 0);
        log_degraded_claim_read("knowledge_diff", "", "", 0);
    });

    // `strings.Contains(out, "[degraded] as_of_project served 2 claim(s)
    // from .threski/claims/ without SurrealDB (project=repo, ref=abc)")`.
    assert!(
        out.contains(
            "[degraded] as_of_project served 2 claim(s) from .threski/claims/ without SurrealDB (project=repo, ref=abc)"
        ),
        "degraded read line missing:\n{out}"
    );
    // zero-claim degraded reads must stay silent.
    assert!(
        !out.contains("ask_project") && !out.contains("knowledge_diff"),
        "zero-claim degraded reads must stay silent:\n{out}"
    );
    // "One line, no spam": exactly one [degraded] marker in the capture.
    assert_eq!(
        out.matches("[degraded]").count(),
        1,
        "expected exactly one [degraded] line, got:\n{out}"
    );
}

/// Boundary: the threshold is >0 — a
/// 1-claim read emits, and the tool/project/detail interpolation is verbatim.
#[test]
fn degraded_line_boundary_one_claim_emits() {
    let out = captured(|| {
        log_degraded_claim_read("knowledge_diff", "demo", "", 1);
    });
    assert!(
        out.contains(
            "[degraded] knowledge_diff served 1 claim(s) from .threski/claims/ without SurrealDB (project=demo)"
        ),
        "1-claim read must emit the line:\n{out}"
    );
    assert_eq!(
        out.matches("[degraded]").count(),
        1,
        "exactly one line expected:\n{out}"
    );
}
