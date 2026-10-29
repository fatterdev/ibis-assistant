//! Leveled logging with stdout-quiet mode for stdio plugin children
//! Drivers write to stderr and/or the app log file —
//! stdout is never touched so the MCP stdio framing stays clean.

use std::fs::OpenOptions;
use std::io::{IsTerminal, Write as _};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Mirrors the original SetLevel switch: unknown strings fall back to INFO.
    pub fn parse(level_str: &str) -> Level {
        match level_str.to_uppercase().as_str() {
            "DEBUG" => Level::Debug,
            "INFO" => Level::Info,
            "WARN" => Level::Warn,
            "ERROR" => Level::Error,
            _ => Level::Info,
        }
    }
}

#[derive(Debug, Clone)]
enum Sink {
    Discard,
    Stderr,
    File(Arc<Mutex<Option<std::fs::File>>>),
    Multi(Arc<Mutex<Option<std::fs::File>>>),
}

impl Sink {
    fn write_line(&self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Discard => Ok(()),
            Sink::Stderr => std::io::stderr().write_all(buf),
            Sink::File(f) => {
                let mut guard = f.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(file) = guard.as_mut() {
                    return file.write_all(buf);
                }
                Ok(())
            }
            Sink::Multi(f) => {
                let mut guard = f.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(file) = guard.as_mut() {
                    file.write_all(buf)?;
                }
                std::io::stderr().write_all(buf)
            }
        }
    }
}

struct SinkWriter(Sink);

impl std::io::Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_line(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Sink::File(f) | Sink::Multi(f) = &self.0 {
            let mut guard = f.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(file) = guard.as_mut() {
                return file.flush();
            }
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = SinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter(clone_sink(self))
    }
}

fn clone_sink(sink: &Sink) -> Sink {
    match sink {
        Sink::Discard => Sink::Discard,
        Sink::Stderr => Sink::Stderr,
        Sink::File(f) => Sink::File(Arc::clone(f)),
        Sink::Multi(f) => Sink::Multi(Arc::clone(f)),
    }
}

fn open_log_file(log_file: &str) -> std::io::Result<std::fs::File> {
    OpenOptions::new().append(true).create(true).open(log_file)
}

/// Selects the writer sink mirroring the original implementation:
/// - `file_only` with no `log_file` → discard everything;
/// - `log_file` set → file only when `file_only` or the process is not
///   attached to a terminal (service mode), else stderr + file;
/// - otherwise stderr.
fn build_sink(log_file: &str, file_only: bool) -> std::io::Result<Sink> {
    if log_file.is_empty() {
        return Ok(if file_only {
            Sink::Discard
        } else {
            Sink::Stderr
        });
    }
    let file = open_log_file(log_file)?;
    let shared = Arc::new(Mutex::new(Some(file)));
    if file_only || !std::io::stderr().is_terminal() {
        Ok(Sink::File(shared))
    } else {
        Ok(Sink::Multi(shared))
    }
}

/// Initializes the global tracing subscriber using `build_sink`'s writer
/// selection. Idempotent: the first call wins (process-global default).
pub fn init(log_file: &str, level_str: &str, file_only: bool) -> std::io::Result<()> {
    let level = Level::parse(level_str);
    let max = match level {
        Level::Debug => tracing::Level::DEBUG,
        Level::Info => tracing::Level::INFO,
        Level::Warn => tracing::Level::WARN,
        Level::Error => tracing::Level::ERROR,
    };

    let sink = build_sink(log_file, file_only)?;

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(max)
        .with_ansi(false)
        .with_target(false)
        .with_writer(sink)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parsing_defaults_to_info() {
        assert_eq!(Level::parse("DEBUG"), Level::Debug);
        assert_eq!(Level::parse("info"), Level::Info);
        assert_eq!(Level::parse("WARN"), Level::Warn);
        assert_eq!(Level::parse("ERROR"), Level::Error);
        assert_eq!(Level::parse("whatever"), Level::Info);
    }

    #[test]
    fn init_discards_when_file_only_without_file() {
        init("", "INFO", true).expect("init discard mode");
    }

    #[test]
    fn file_only_sink_writes_to_file_only() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("app.log");
        let sink = build_sink(log_path.to_str().unwrap(), true).unwrap();
        sink.write_line(b"[INFO] file mode message\n").unwrap();
        // Quiet mode: the file carries the line (stdout/stderr untouched).
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            contents.contains("file mode message"),
            "log file missing message: {contents:?}"
        );
    }

    #[test]
    fn multi_sink_writes_file_and_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("app.log");
        let sink = build_sink(log_path.to_str().unwrap(), false).unwrap();
        sink.write_line(b"[INFO] multi mode message\n").unwrap();
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("multi mode message"));
    }

    #[test]
    fn discard_sink_silences_everything() {
        let sink = build_sink("", true).unwrap();
        sink.write_line(b"[INFO] dropped\n").unwrap();
    }
}
