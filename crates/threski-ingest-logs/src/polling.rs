//! Remote log polling: per-source owned tokio tasks that
//! pull log files over FTP / SFTP / local-or-UNC paths on a configurable
//! interval, deduplicate them by content SHA-256 (`log_processed_file`), run
//! them through the analyzer, and optionally archive (rename) the remote
//! file.
//!
//! D1: the embedded SMB client is dropped — SMB shares are transparently
//! reachable as UNC paths through the local filesystem client (Windows) or
//! mounts (Unix); the `smb` protocol value now produces a typed error naming
//! the fallback.
//!
//! Hardening vs (findings G-A02/G-A04/G-A12): dedup and processed-file SQL
//! binds variables; invalid glob patterns are rejected with a warning instead
//! of silently matching nothing; every client operation has a timeout.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use threski_db::executor::{vars, Executor};
use threski_db::ids::{format_record_id, sanitize_id};
use threski_db::schema::names as schema;
use threski_db::value::SurrealValue;

use crate::analyzer::{read_capped, LogAnalyzer, MAX_LOG_INGEST_BYTES};
use crate::error::{LogsError, Result};
use crate::notifier::Notifier;

/// Default poll interval when `poll_interval` is empty/invalid
///
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Per-operation remote-IO timeout (FTP dial 10s; SSH dial 10s).
const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Remote filesystem seam
// ---------------------------------------------------------------------------

/// One directory entry (files only are ever processed).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Abstract remote log source: list, capped read, archive rename.
/// Implemented for local/UNC paths ([`LocalFs`]), FTP ([`FtpFs`]) and
/// SFTP ([`SftpFs`]); fakes drive the tests ( had no seam — lane C's
/// narrow-trait precedent, C-A16).
#[async_trait]
pub trait RemoteFs: Send + Sync {
    async fn list_dir(&self, dir: &str) -> Result<Vec<RemoteEntry>>;
    /// Reads up to `max` bytes; returns `(data, truncated)`.
    async fn read_file_capped(&self, path: &str, max: usize) -> Result<(Vec<u8>, bool)>;
    async fn rename(&self, from: &str, to: &str) -> Result<()>;
    async fn mkdir_all(&self, dir: &str) -> Result<()>;
    /// Best-effort archive-dir preparation before rename. the SFTP path calls
    /// `MkdirAll` (error ignored); FTP/SMB do not — the default is a no-op.
    async fn prepare_archive(&self, _dir: &str) -> Result<()> {
        Ok(())
    }
}

/// Local filesystem (and Windows UNC shares, per D1).
pub struct LocalFs;

#[async_trait]
impl RemoteFs for LocalFs {
    async fn list_dir(&self, dir: &str) -> Result<Vec<RemoteEntry>> {
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| LogsError::other(format!("readdir {dir}: {e}")))?;
        while let Some(entry) = rd.next_entry().await.map_err(LogsError::Io)? {
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            out.push(RemoteEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
            });
        }
        Ok(out)
    }

    async fn read_file_capped(&self, path: &str, max: usize) -> Result<(Vec<u8>, bool)> {
        let f = tokio::fs::File::open(path)
            .await
            .map_err(|e| LogsError::other(format!("open {path}: {e}")))?;
        read_capped(f, max)
            .await
            .map_err(|e| LogsError::other(format!("read {path}: {e}")))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        tokio::fs::rename(from, to)
            .await
            .map_err(|e| LogsError::other(format!("rename {from} → {to}: {e}")))
    }

    async fn mkdir_all(&self, dir: &str) -> Result<()> {
        tokio::fs::create_dir_all(dir).await.map_err(LogsError::Io)
    }
}

/// FTP client (`jlaffaye/ftp → `suppaftp`, tokio backend). One dial per
/// poll pass, reused across list/read/rename likewise (finding G-A13); the
/// stream is invalidated on failure and dropped (TCP close) with the pass.
pub struct FtpFs {
    pub host: String,
    pub port: i64,
    pub user: String,
    pub password: String,
    conn: tokio::sync::Mutex<Option<suppaftp::tokio::AsyncFtpStream>>,
}

impl FtpFs {
    pub fn new(host: String, port: i64, user: String, password: String) -> Self {
        FtpFs {
            host,
            port,
            user,
            password,
            conn: tokio::sync::Mutex::new(None),
        }
    }

    async fn connect(&self) -> Result<suppaftp::tokio::AsyncFtpStream> {
        let addr = join_host_port(&self.host, self.port, 21);
        let mut stream = tokio::time::timeout(
            REMOTE_TIMEOUT,
            suppaftp::tokio::AsyncFtpStream::connect(&addr),
        )
        .await
        .map_err(|_| LogsError::ftp("dial timed out"))?
        .map_err(|e| LogsError::ftp(format!("FTP dial failed: {e}")))?;
        tokio::time::timeout(
            REMOTE_TIMEOUT,
            stream.login(self.user.as_str(), self.password.as_str()),
        )
        .await
        .map_err(|_| LogsError::ftp("login timed out"))?
        .map_err(|e| LogsError::ftp(format!("FTP login failed: {e}")))?;
        Ok(stream)
    }

    /// Reused (or freshly dialed) connection guard; the session is
    /// invalidated on any op failure so the next op reconnects.
    async fn stream(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<suppaftp::tokio::AsyncFtpStream>>> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        Ok(guard)
    }
}

/// Extracts the file name from a raw FTP LIST line (UNIX `ls -l`,
/// Windows DOS, or bare-name listings).
pub fn extract_listed_name(line: &str) -> Option<String> {
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Windows DOS style: "MM-DD-YY HH:MM(AM) <NAME>" — field 2 is either a
    // size or "<DIR>"; the name starts after it (3 fields skipped).
    if fields.len() >= 3
        && fields[0].matches('-').count() == 2
        && fields[0].len() >= 8
        && fields[0]
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        if fields.len() > 3 {
            return Some(fields[3..].join(" "));
        }
        return None;
    }
    // UNIX ls -l style: permissions ... nlink owner group size [month day time] name.
    if fields.len() >= 9 && fields[0].len() == 10 {
        return Some(fields[8..].join(" "));
    }
    // Single-token (mangled or MLSD-less) listing.
    if fields.len() == 1 && !fields[0].starts_with("total") {
        return Some(fields[0].to_string());
    }
    None
}

#[async_trait]
impl RemoteFs for FtpFs {
    async fn list_dir(&self, dir: &str) -> Result<Vec<RemoteEntry>> {
        let mut guard = self.stream().await?;
        let stream = guard.as_mut().expect("ftp session populated");
        let lines = match tokio::time::timeout(REMOTE_TIMEOUT, stream.list(Some(dir))).await {
            Ok(Ok(lines)) => lines,
            Ok(Err(e)) => {
                *guard = None;
                return Err(LogsError::ftp(format!("FTP list failed: {e}")));
            }
            Err(_) => {
                *guard = None;
                return Err(LogsError::ftp("list timed out"));
            }
        };
        let mut out = Vec::new();
        for line in lines {
            let Some(name) = extract_listed_name(&line) else {
                continue;
            };
            let is_dir = line.contains("<DIR>");
            out.push(RemoteEntry { name, is_dir });
        }
        Ok(out)
    }

    async fn read_file_capped(&self, path: &str, max: usize) -> Result<(Vec<u8>, bool)> {
        let mut guard = self.stream().await?;
        let stream = guard.as_mut().expect("ftp session populated");
        // Streaming read capped in memory ( used resp.Read + ReadCapped).
        let res = tokio::time::timeout(REMOTE_TIMEOUT, async {
            let mut transfer = stream
                .retr_as_stream(path)
                .await
                .map_err(|e| LogsError::ftp(format!("FTP retrieve failed for {path}: {e}")))?;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut truncated = false;
            loop {
                match tokio::io::AsyncReadExt::read(&mut transfer, &mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let remaining = max.saturating_sub(buf.len());
                        if n <= remaining {
                            buf.extend_from_slice(&chunk[..n]);
                        } else {
                            buf.extend_from_slice(&chunk[..remaining]);
                            truncated = true;
                            break;
                        }
                    }
                    Err(e) => {
                        return Err(LogsError::ftp(format!("FTP read failed for {path}: {e}")));
                    }
                }
            }
            // Reads the pending completion reply (suppaftp TransferStream::finish).
            transfer.finish().await.map_err(|e| {
                LogsError::ftp(format!("FTP transfer finalize failed for {path}: {e}"))
            })?;
            Ok::<(Vec<u8>, bool), LogsError>((buf, truncated))
        })
        .await;
        match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                *guard = None;
                Err(e)
            }
            Err(_) => {
                *guard = None;
                Err(LogsError::ftp("retrieve timed out"))
            }
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut guard = self.stream().await?;
        let stream = guard.as_mut().expect("ftp session populated");
        match tokio::time::timeout(REMOTE_TIMEOUT, stream.rename(from, to)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                *guard = None;
                Err(LogsError::ftp(format!(
                    "FTP rename/archive failed for {from}: {e}"
                )))
            }
            Err(_) => {
                *guard = None;
                Err(LogsError::ftp("rename timed out"))
            }
        }
    }

    async fn mkdir_all(&self, _dir: &str) -> Result<()> {
        Ok(()) // FTP archives are placed via rename into an existing dir.
    }
}

/// SFTP client (`pkg/sftp + `golang.org/x/crypto/ssh` → `russh` +
/// `russh-sftp`). Password auth; the configured `known_host_key` pins the
/// server key, otherwise verification is skipped with
/// a loud warning. One SSH dial per poll
/// pass, reused across list/read/rename likewise (finding G-A13).
pub struct SftpFs {
    pub host: String,
    pub port: i64,
    pub user: String,
    pub password: String,
    pub known_host_key: String,
    pub source_name: String,
    session: tokio::sync::Mutex<Option<SftpConn>>,
}

/// Bundled SSH connection: the `Handle` must outlive the session or russh
/// closes the transport (never read directly — load-bearing lifetime anchor).
struct SftpConn {
    #[allow(dead_code)]
    handle: russh::client::Handle<HostKeyHandler>,
    sftp: russh_sftp::client::SftpSession,
}

struct HostKeyHandler {
    known: Option<String>,
    source_name: String,
}

impl russh::client::Handler for HostKeyHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> impl Future<Output = std::result::Result<bool, Self::Error>> + Send {
        // Purely synchronous key comparison — a ready future sidesteps the
        // RPITIT lifetime/Send capture pitfalls of `async fn` impls.
        let Some(expected) = &self.known else {
            tracing::warn!(
                "SFTP source '{}': known_host_key not configured, host key verification disabled. Set known_host_key in config for production use.",
                self.source_name
            );
            return std::future::ready(Ok(true));
        };
        let server_fp = match server_public_key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => {
                key.fingerprint(ssh_hash_alg())
            }
            russh::keys::PublicKeyOrCertificate::Certificate(cert) => {
                cert.public_key().fingerprint(ssh_hash_alg())
            }
        };
        let verdict = match russh::keys::PublicKey::from_openssh(expected) {
            Ok(pinned) => {
                let ok = pinned.fingerprint(ssh_hash_alg()) == server_fp;
                if !ok {
                    tracing::error!(
                        "SFTP source '{}': server host key does not match known_host_key",
                        self.source_name
                    );
                }
                ok
            }
            Err(e) => {
                tracing::error!(
                    "SFTP source '{}': invalid known_host_key ({e})",
                    self.source_name
                );
                false
            }
        };
        std::future::ready(Ok(verdict))
    }
}

fn ssh_hash_alg() -> russh::keys::HashAlg {
    russh::keys::HashAlg::Sha256
}

impl SftpFs {
    pub fn new(
        host: String,
        port: i64,
        user: String,
        password: String,
        known_host_key: String,
        source_name: String,
    ) -> Self {
        SftpFs {
            host,
            port,
            user,
            password,
            known_host_key,
            source_name,
            session: tokio::sync::Mutex::new(None),
        }
    }

    /// Reused (or freshly dialed) session guard; invalidated on op failure.
    async fn sftp(&self) -> Result<tokio::sync::MutexGuard<'_, Option<SftpConn>>> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(self.open_session().await?);
        }
        Ok(guard)
    }

    async fn open_session(&self) -> Result<SftpConn> {
        let addr = join_host_port(&self.host, self.port, 22);
        let config = Arc::new(russh::client::Config {
            inactivity_timeout: Some(Duration::from_secs(600)),
            ..Default::default()
        });
        let handler = HostKeyHandler {
            known: if self.known_host_key.trim().is_empty() {
                None
            } else {
                Some(self.known_host_key.trim().to_string())
            },
            source_name: self.source_name.clone(),
        };
        let mut handle = tokio::time::timeout(
            REMOTE_TIMEOUT,
            russh::client::connect(config, addr, handler),
        )
        .await
        .map_err(|_| LogsError::sftp("SSH dial timed out"))?
        .map_err(|e| LogsError::sftp(format!("SSH dial failed: {e}")))?;
        let authenticated = tokio::time::timeout(
            REMOTE_TIMEOUT,
            handle.authenticate_password(self.user.as_str(), self.password.as_str()),
        )
        .await
        .map_err(|_| LogsError::sftp("SSH auth timed out"))?
        .map_err(|e| LogsError::sftp(format!("SSH auth failed: {e}")))?;
        if !authenticated.success() {
            return Err(LogsError::sftp("SSH auth rejected"));
        }
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| LogsError::sftp(format!("channel open failed: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| LogsError::sftp(format!("sftp subsystem failed: {e}")))?;
        let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| LogsError::sftp(format!("SFTP client init failed: {e}")))?;
        Ok(SftpConn { handle, sftp })
    }
}

#[async_trait]
impl RemoteFs for SftpFs {
    async fn list_dir(&self, dir: &str) -> Result<Vec<RemoteEntry>> {
        let mut guard = self.sftp().await?;
        let conn = guard.as_mut().expect("sftp session populated");
        let entries = match tokio::time::timeout(REMOTE_TIMEOUT, conn.sftp.read_dir(dir)).await {
            Ok(Ok(entries)) => entries,
            Ok(Err(e)) => {
                *guard = None;
                return Err(LogsError::sftp(format!("SFTP readdir failed: {e}")));
            }
            Err(_) => {
                *guard = None;
                return Err(LogsError::sftp("readdir timed out"));
            }
        };
        let mut out = Vec::new();
        for e in entries {
            let is_dir = e.file_type().is_dir();
            out.push(RemoteEntry {
                name: e.file_name(),
                is_dir,
            });
        }
        Ok(out)
    }

    async fn read_file_capped(&self, path: &str, max: usize) -> Result<(Vec<u8>, bool)> {
        let mut guard = self.sftp().await?;
        let conn = guard.as_mut().expect("sftp session populated");
        let file = match tokio::time::timeout(REMOTE_TIMEOUT, conn.sftp.open(path)).await {
            Ok(Ok(file)) => file,
            Ok(Err(e)) => {
                *guard = None;
                return Err(LogsError::sftp(format!("SFTP open failed for {path}: {e}")));
            }
            Err(_) => {
                *guard = None;
                return Err(LogsError::sftp("open timed out"));
            }
        };
        let mut file = file;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 32768];
        let mut truncated = false;
        loop {
            let n = match tokio::time::timeout(
                REMOTE_TIMEOUT,
                tokio::io::AsyncReadExt::read(&mut file, &mut chunk),
            )
            .await
            {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    *guard = None;
                    return Err(LogsError::sftp(format!("SFTP read failed for {path}: {e}")));
                }
                Err(_) => {
                    *guard = None;
                    return Err(LogsError::sftp("read timed out"));
                }
            };
            if n == 0 {
                break;
            }
            let remaining = max.saturating_sub(buf.len());
            if n <= remaining {
                buf.extend_from_slice(&chunk[..n]);
            } else {
                buf.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
        }
        Ok((buf, truncated))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut guard = self.sftp().await?;
        let conn = guard.as_mut().expect("sftp session populated");
        match tokio::time::timeout(REMOTE_TIMEOUT, conn.sftp.rename(from, to)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                *guard = None;
                Err(LogsError::sftp(format!(
                    "SFTP rename/archive failed for {from}: {e}"
                )))
            }
            Err(_) => {
                *guard = None;
                Err(LogsError::sftp("rename timed out"))
            }
        }
    }

    async fn mkdir_all(&self, dir: &str) -> Result<()> {
        let mut guard = self.sftp().await?;
        let conn = guard.as_mut().expect("sftp session populated");
        // Best effort: probe the dir and create the leaf if missing
        // (russh-sftp create_dir); polling continues on failure.
        let probe = tokio::time::timeout(REMOTE_TIMEOUT, conn.sftp.canonicalize(dir)).await;
        let _ = probe.map_err(|_| LogsError::sftp("mkdir probe timed out"));
        let _ = tokio::time::timeout(REMOTE_TIMEOUT, conn.sftp.create_dir(dir)).await;
        Ok(())
    }

    async fn prepare_archive(&self, dir: &str) -> Result<()> {
        self.mkdir_all(dir).await
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Owns one polling task per enabled source.
pub struct PollingManager {
    pub cfg: Arc<threski_core::config::Config>,
    pub db: Option<Arc<dyn Executor>>,
    pub ai: Option<Arc<threski_ai::Client>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// DB row projection for the processed-file dedup query.
#[derive(Debug, SurrealValue)]
struct ProcessedFileRow {
    #[allow(dead_code)]
    id: Option<String>,
}

impl PollingManager {
    pub fn new(
        cfg: Arc<threski_core::config::Config>,
        db: Option<Arc<dyn Executor>>,
        ai: Option<Arc<threski_ai::Client>>,
    ) -> Self {
        PollingManager {
            cfg,
            db,
            ai,
            shutdown_tx: None,
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Spawns one task per enabled source. Requires a DB —
    /// polling is disabled (warn) without one, matching
    pub fn start(&mut self) {
        if self.db.is_none() {
            tracing::warn!("PollingManager: database is not connected. Polling disabled.");
            return;
        }
        let (tx, _) = watch::channel(false);
        let mut handles = Vec::new();
        for source in &self.cfg.log_ingestion.polling {
            if !source.enabled {
                continue;
            }
            let source = source.clone();
            let db = self.db.clone().unwrap();
            let ai = self.ai.clone();
            let cfg = self.cfg.clone();
            let mut rx = tx.subscribe();
            let handle = tokio::spawn(async move {
                let interval = parse_go_duration(&source.poll_interval).unwrap_or_else(|| {
                    tracing::warn!(
                        "PollingManager: invalid poll interval '{}' for source {}, using 15m",
                        source.poll_interval,
                        source.name
                    );
                    DEFAULT_POLL_INTERVAL
                });

                tracing::info!(
                    "Starting polling client for source: {} (protocol: {}, interval: {:?})",
                    source.name,
                    source.protocol,
                    interval
                );
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                if let Err(err) = poll_source(&source, &db, ai.as_ref(), &cfg).await {
                    tracing::error!(
                        "PollingManager: error polling source {}: {err}",
                        source.name
                    );
                }

                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            if let Err(err) = poll_source(&source, &db, ai.as_ref(), &cfg).await {
                                tracing::error!(
                                    "PollingManager: error polling source {}: {err}",
                                    source.name
                                );
                            }
                        }
                        changed = rx.changed() => {
                            if changed.is_err() || *rx.borrow() {
                                return;
                            }
                        }
                    }
                }
            });
            handles.push(handle);
        }
        self.shutdown_tx = Some(tx);
        *self.tasks.lock().unwrap() = handles;
    }

    /// Cancels all source tasks and joins them.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        let handles: Vec<tokio::task::JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().unwrap());
        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }
        tracing::info!("PollingManager stopped.");
    }
}

/// Dispatches one poll pass by protocol.
async fn poll_source(
    source: &threski_core::config::PollingSource,
    db: &Arc<dyn Executor>,
    ai: Option<&Arc<threski_ai::Client>>,
    cfg: &Arc<threski_core::config::Config>,
) -> Result<()> {
    tracing::debug!("PollingManager: polling source {}...", source.name);
    let pattern = GlobPattern::compile(&source.file_pattern).map_err(|e| {
        tracing::warn!(
            "PollingManager: invalid file_pattern '{}' for source {}: {e}",
            source.file_pattern,
            source.name
        );
        e
    });
    // Invalid patterns match nothing.
    let pattern = pattern.ok();

    match source.protocol.to_lowercase().as_str() {
        "ftp" => {
            let fs = FtpFs::new(
                source.host.clone(),
                source.port,
                source.user.clone(),
                source.password.clone(),
            );
            poll_fs(source, &fs, pattern.as_ref(), db, ai, cfg).await
        }
        "sftp" => {
            let fs = SftpFs::new(
                source.host.clone(),
                source.port,
                source.user.clone(),
                source.password.clone(),
                source.known_host_key.clone(),
                source.name.clone(),
            );
            poll_fs(source, &fs, pattern.as_ref(), db, ai, cfg).await
        }
        // D1: SMB dropped — UNC/native mounts are handled by the local client.
        "file" | "local" | "unc" => {
            let fs = LocalFs;
            poll_fs(source, &fs, pattern.as_ref(), db, ai, cfg).await
        }
        "smb" => Err(LogsError::polling(
            "SMB remote polling was dropped (design decision D1). Use a UNC path \
             (\\\\server\\share\\logs) with protocol 'file' or a mounted path instead.",
        )),
        other => Err(LogsError::polling(format!("unsupported protocol: {other}"))),
    }
}

/// Shared poll loop over any [`RemoteFs`] (the bodies of `pollFTP /
/// `pollSFTP` / `pollSMB` — identical except for the client).
async fn poll_fs(
    source: &threski_core::config::PollingSource,
    fs: &dyn RemoteFs,
    pattern: Option<&GlobPattern>,
    db: &Arc<dyn Executor>,
    ai: Option<&Arc<threski_ai::Client>>,
    cfg: &Arc<threski_core::config::Config>,
) -> Result<()> {
    let entries = fs.list_dir(&source.remote_dir).await?;
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        let matched = pattern.map(|p| p.is_match(&entry.name)).unwrap_or(false);
        if !matched {
            continue;
        }

        let remote_path = join_slash(&source.remote_dir, &entry.name);

        let (body_bytes, truncated) = match fs
            .read_file_capped(&remote_path, MAX_LOG_INGEST_BYTES)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                tracing::error!("Polling read failed for {remote_path}: {err}");
                continue;
            }
        };
        if truncated {
            tracing::warn!(
                "Polling file truncated at {} bytes: {remote_path} (processing capped prefix)",
                MAX_LOG_INGEST_BYTES
            );
        }

        let hash = content_hash(&body_bytes);
        match is_file_processed(db, &hash).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(err) => {
                tracing::error!("Polling dedup check failed for {remote_path}: {err}");
                continue;
            }
        }

        let lines: Vec<String> = String::from_utf8_lossy(&body_bytes)
            .split('\n')
            .map(|s| s.to_string())
            .collect();
        process_log_lines(&source.project_name, &entry.name, &lines, db, ai, cfg).await;

        if !source.archive_dir.is_empty() {
            let archive_path = join_slash(&source.archive_dir, &entry.name);
            // Scenario: SFTP ignored the MkdirAll error before renaming.
            let _ = fs.prepare_archive(&source.archive_dir).await;
            if let Err(err) = fs.rename(&remote_path, &archive_path).await {
                tracing::error!("Polling rename/archive failed for {remote_path}: {err}");
            }
        }

        record_file_processed(db, &remote_path, &hash).await;
    }
    Ok(())
}

/// Returns the SHA256 hex digest of the content — dedup is by content, not
/// path+size, so same-named files with different content are reprocessed
///
pub fn content_hash(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// True when a file with this content hash was already processed
/// Bound variable (F-002).
async fn is_file_processed(db: &Arc<dyn Executor>, hash: &str) -> Result<bool> {
    let sql = format!(
        "SELECT id FROM {} WHERE hash = $hash;",
        schema::LOG_PROCESSED_FILE
    );
    let outcome = db
        .smart_query(&sql, vars([("hash", hash)]))
        .await
        .map_err(|e| LogsError::other(e.to_string()))?;
    let rows = outcome.rows::<ProcessedFileRow>(&sql)?;
    Ok(!rows.is_empty())
}

/// Marks a processed file by content hash.
/// Bound variables (F-002); the record target is a validated hex-hash id.
async fn record_file_processed(db: &Arc<dyn Executor>, path: &str, hash: &str) {
    let id = format_record_id(schema::LOG_PROCESSED_FILE, &sanitize_id(hash));
    // `processed_at` is a typed datetime (D17 R7).
    let processed_at = surrealdb::types::Datetime::from(chrono::Utc::now());
    let sql = format!("UPSERT {id} SET path = $path, hash = $hash, processed_at = $processed_at;");
    if let Err(e) = db
        .smart_query(
            &sql,
            vars([
                ("path", threski_db::value::Value::from_t(path)),
                ("hash", threski_db::value::Value::from_t(hash)),
                (
                    "processed_at",
                    threski_db::value::Value::Datetime(processed_at),
                ),
            ]),
        )
        .await
    {
        tracing::warn!("Failed to record processed file: {e}");
    }
}

/// Cleans, analyzes and notifies for one polled file
///
async fn process_log_lines(
    project: &str,
    file_name: &str,
    lines: &[String],
    db: &Arc<dyn Executor>,
    ai: Option<&Arc<threski_ai::Client>>,
    cfg: &Arc<threski_core::config::Config>,
) {
    let clean_lines: Vec<String> = lines
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if clean_lines.is_empty() {
        return;
    }

    let path = std::path::Path::new(&cfg.logs_root)
        .join(project)
        .join(file_name)
        .display()
        .to_string();
    let analyzer = LogAnalyzer::new_with_project(
        &path,
        project,
        Some(cfg.clone()),
        Some(db.clone()),
        ai.cloned(),
    )
    .await;

    let (anomalies, err) = analyzer.process_batch_sync(&clean_lines).await;
    if let Some(err) = err {
        tracing::error!("Polling log processing error for {project}/{file_name}: {err}");
        return;
    }

    if anomalies.is_empty() {
        return;
    }

    let notifier = Notifier::new(cfg.clone(), Some(db.clone()));
    let mut sb = String::new();
    sb.push_str(&format!(
        "Remote Polling Log Report for Project: {project} (File: {file_name})\r\n\r\n"
    ));
    sb.push_str(&format!(
        "Detected {} errors/anomalies:\r\n\r\n",
        anomalies.len()
    ));
    let mut max_severity = 0i64;
    for e in &anomalies {
        if e.severity > max_severity {
            max_severity = e.severity;
        }
        sb.push_str(&format!("Category: {}\r\n", e.category));
        sb.push_str(&format!("Severity: {}/10\r\n", e.severity));
        if !e.file.is_empty() {
            sb.push_str(&format!("File: {}\r\n", e.file));
        }
        if !e.cause.is_empty() {
            sb.push_str(&format!("Cause: {}\r\n", e.cause));
        }
        sb.push_str("Stack Trace:\r\n");
        sb.push_str(&format!("{}\r\n\r\n", e.stack_trace));
    }
    let _ = notifier
        .queue_notification(project, file_name, anomalies.len(), max_severity, &sb)
        .await;
}

/// `net.JoinHostPort` equivalent with the protocol default port
///
pub fn join_host_port(host: &str, port: i64, default_port: i64) -> String {
    let port = if port == 0 { default_port } else { port };
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Joins remote-dir paths with `/`.
pub fn join_slash(base: &str, name: &str) -> String {
    let base = base.trim_end_matches(['/', '\\']);
    format!("{base}/{name}")
}

/// Parses `time.ParseDuration strings ("300ms", "1.5h", "2h45m").
/// Returns `None` on invalid input (callers fall back to the default).
pub fn parse_go_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^((\d+(\.\d+)?)(ns|us|µs|ms|s|m|h))+$").expect("static regex")
    });
    if !re.is_match(s) {
        return None;
    }
    static SEG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let seg = SEG.get_or_init(|| {
        regex::Regex::new(r"(\d+(?:\.\d+)?)(ns|us|µs|ms|s|m|h)").expect("static regex")
    });
    let mut total_nanos = 0f64;
    for c in seg.captures_iter(s) {
        let value: f64 = c.get(1)?.as_str().parse().ok()?;
        let unit = c.get(2)?.as_str();
        let mult = match unit {
            "ns" => 1.0,
            "us" | "µs" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60.0 * 1_000_000_000.0,
            "h" => 3600.0 * 1_000_000_000.0,
            _ => return None,
        };
        total_nanos += value * mult;
    }
    if total_nanos <= 0.0 {
        return None;
    }
    Some(Duration::from_nanos(total_nanos as u64))
}

/// `filepath.Match`-compatible glob: `*` and `?` do not
/// cross `/`, `[...]` classes, `\` escapes; an invalid pattern is an error
/// ( returned an error that callers swallowed — finding G-A12).
pub struct GlobPattern {
    re: regex::Regex,
}

impl GlobPattern {
    pub fn compile(pattern: &str) -> Result<Self> {
        let mut out = String::from("^");
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '*' => out.push_str("[^/]*"),
                '?' => out.push_str("[^/]"),
                '\\' => {
                    if i + 1 < chars.len() {
                        i += 1;
                        out.push_str(&regex::escape(&chars[i].to_string()));
                    } else {
                        return Err(LogsError::other("glob: trailing backslash"));
                    }
                }
                '[' => {
                    // Character class: copy through the closing ']'.
                    let mut class = String::from("[");
                    let mut j = i + 1;
                    if j < chars.len() && (chars[j] == '^' || chars[j] == '!') {
                        class.push('^');
                        j += 1;
                    }
                    let start = j;
                    // A ']' right after the opening (or negation) is literal.
                    if j < chars.len() && chars[j] == ']' {
                        class.push_str("\\]");
                        j += 1;
                    }
                    let mut closed = false;
                    while j < chars.len() {
                        if chars[j] == ']' {
                            closed = true;
                            break;
                        }
                        let ch = chars[j];
                        match ch {
                            '[' | '\\' => class.push_str(&regex::escape(&ch.to_string())),
                            _ => class.push(ch),
                        }
                        j += 1;
                    }
                    if !closed || j == start {
                        return Err(LogsError::other("glob: missing closing ']'"));
                    }
                    class.push(']');
                    out.push_str(&class);
                    i = j;
                }
                _ => out.push_str(&regex::escape(&c.to_string())),
            }
            i += 1;
        }
        out.push('$');
        let re = regex::Regex::new(&out)
            .map_err(|e| LogsError::other(format!("glob compile failed: {e}")))?;
        Ok(GlobPattern { re })
    }

    pub fn is_match(&self, name: &str) -> bool {
        self.re.is_match(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn join_host_port_matches_reference() {
        assert_eq!(join_host_port("127.0.0.1", 445, 445), "127.0.0.1:445");
        assert_eq!(join_host_port("::1", 0, 22), "[::1]:22");
    }

    #[test]
    fn duration_parsing_cases() {
        assert_eq!(parse_go_duration("10ms"), Some(Duration::from_millis(10)));
        assert_eq!(parse_go_duration("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_go_duration("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_go_duration("2h45m"), Some(Duration::from_secs(9900)));
        assert_eq!(parse_go_duration("300ns"), Some(Duration::from_nanos(300)));
        assert_eq!(parse_go_duration("15"), None, "unitless is invalid");
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("abc"), None);
    }

    #[test]
    fn glob_pattern_semantics() {
        let p = GlobPattern::compile("*.log").unwrap();
        assert!(p.is_match("app.log"));
        assert!(p.is_match("2026-01-01.boot.log"));
        assert!(!p.is_match("logs/app.log"), "* must not cross /");
        assert!(!p.is_match("app.logx"));

        let q = GlobPattern::compile("log-?-2026.txt").unwrap();
        assert!(q.is_match("log-a-2026.txt"));
        assert!(!q.is_match("log-ab-2026.txt"));

        let c = GlobPattern::compile("[abc].log").unwrap();
        assert!(c.is_match("a.log"));
        assert!(!c.is_match("d.log"));

        assert!(
            GlobPattern::compile("a[bc").is_err(),
            "unmatched [ is an error (G-A12)"
        );
        assert!(
            GlobPattern::compile("a\\").is_err(),
            "trailing backslash is an error"
        );
    }

    #[test]
    fn extract_listed_name_cases() {
        assert_eq!(
            extract_listed_name("-rw-r--r-- 1 user group 1234 Jan 01 10:00 app.log"),
            Some("app.log".to_string())
        );
        assert_eq!(
            extract_listed_name("01-01-26  10:00AM                 1234 app.log"),
            Some("app.log".to_string())
        );
        assert_eq!(
            extract_listed_name("01-01-26  10:00AM       <DIR>          logs"),
            Some("logs".to_string())
        );
        assert_eq!(extract_listed_name("total 12"), None);
        assert_eq!(extract_listed_name(""), None);
    }

    #[test]
    fn content_hash_is_sha256_hex() {
        let h = content_hash(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"world"));
    }

    #[test]
    fn join_slash_trims_base() {
        assert_eq!(join_slash("logs/", "a.log"), "logs/a.log");
        assert_eq!(join_slash("logs", "a.log"), "logs/a.log");
        assert_eq!(join_slash("logs\\", "a.log"), "logs/a.log");
    }

    struct MockPollingDb {
        queries: std::sync::Mutex<Vec<String>>,
        processed_hashes: std::sync::Mutex<std::collections::HashSet<String>>,
        upserts: AtomicUsize,
    }

    impl MockPollingDb {
        fn new() -> Self {
            MockPollingDb {
                queries: std::sync::Mutex::new(Vec::new()),
                processed_hashes: std::sync::Mutex::new(std::collections::HashSet::new()),
                upserts: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Executor for MockPollingDb {
        async fn execute(
            &self,
            sql: &str,
        ) -> std::result::Result<threski_db::executor::QueryOutcome, threski_db::DbError> {
            self.queries.lock().unwrap().push(sql.to_string());
            Ok(threski_db::executor::QueryOutcome::Single(
                threski_db::value::Value::None,
            ))
        }

        async fn smart_query(
            &self,
            sql: &str,
            v: threski_db::executor::Vars,
        ) -> std::result::Result<threski_db::executor::QueryOutcome, threski_db::DbError> {
            self.queries.lock().unwrap().push(sql.to_string());
            if sql.contains("SELECT id FROM log_processed_file") {
                let hash = v
                    .get("hash")
                    .and_then(|val| match val {
                        threski_db::value::Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let known = self.processed_hashes.lock().unwrap().contains(&hash);
                if known {
                    let row = ProcessedFileRow {
                        id: Some(hash.clone()),
                    };
                    return Ok(threski_db::executor::QueryOutcome::Single(
                        SurrealValue::into_value(vec![row]),
                    ));
                }
                return Ok(threski_db::executor::QueryOutcome::Single(
                    threski_db::value::Value::None,
                ));
            }
            if sql.contains("UPSERT log_processed_file") {
                self.upserts.fetch_add(1, Ordering::SeqCst);
            }
            Ok(threski_db::executor::QueryOutcome::Single(
                threski_db::value::Value::None,
            ))
        }
    }

    /// In-memory remote FS fake. Keys are FULL remote paths ("logs/app.log");
    /// `list_dir` returns the bare names of entries living directly in `dir`.
    struct FakeFs {
        files: std::sync::Mutex<std::collections::HashMap<String, String>>,
        renamed: std::sync::Mutex<Vec<(String, String)>>,
        read_cap: usize,
    }

    impl FakeFs {
        fn new(files: Vec<(&str, &str)>) -> Self {
            FakeFs {
                files: std::sync::Mutex::new(
                    files
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                renamed: std::sync::Mutex::new(Vec::new()),
                read_cap: usize::MAX,
            }
        }

        fn get(&self, path: &str) -> String {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl RemoteFs for FakeFs {
        async fn list_dir(&self, dir: &str) -> Result<Vec<RemoteEntry>> {
            let prefix = format!("{}/", dir.trim_end_matches('/'));
            Ok(self
                .files
                .lock()
                .unwrap()
                .keys()
                .filter_map(|k| {
                    k.strip_prefix(&prefix).map(|name| RemoteEntry {
                        name: name.to_string(),
                        is_dir: false,
                    })
                })
                .collect())
        }

        async fn read_file_capped(&self, path: &str, max: usize) -> Result<(Vec<u8>, bool)> {
            let content = self.get(path);
            let cap = max.min(self.read_cap);
            if content.len() > cap {
                return Ok((content.into_bytes()[..cap].to_vec(), true));
            }
            Ok((content.into_bytes(), false))
        }

        async fn rename(&self, from: &str, to: &str) -> Result<()> {
            let mut files = self.files.lock().unwrap();
            if let Some(content) = files.remove(from) {
                files.insert(to.to_string(), content);
            }
            self.renamed
                .lock()
                .unwrap()
                .push((from.to_string(), to.to_string()));
            Ok(())
        }

        async fn mkdir_all(&self, _dir: &str) -> Result<()> {
            Ok(())
        }
    }

    fn poll_cfg() -> threski_core::config::Config {
        threski_core::config::Config {
            logs_root: "./logs".into(),
            ..Default::default()
        }
    }

    fn poll_cfg_arc() -> Arc<threski_core::config::Config> {
        Arc::new(poll_cfg())
    }

    fn test_source() -> threski_core::config::PollingSource {
        threski_core::config::PollingSource {
            name: "test-ftp".into(),
            enabled: true,
            protocol: "ftp".into(),
            host: "127.0.0.1".into(),
            port: 21,
            user: "user".into(),
            password: "pass".into(),
            remote_dir: "logs".into(),
            file_pattern: "*.log".into(),
            archive_dir: "archive".into(),
            poll_interval: "10ms".into(),
            project_name: "proj".into(),
            known_host_key: String::new(),
        }
    }

    fn poll_db(db: &Arc<MockPollingDb>) -> Arc<dyn Executor> {
        db.clone() as Arc<dyn Executor>
    }

    #[tokio::test]
    async fn poll_fs_processes_new_files_and_dedups() {
        let db = Arc::new(MockPollingDb::new());
        let content = "ERROR something failed\nINFO ok\n";
        let fs = FakeFs::new(vec![("logs/app.log", content)]);
        let mut source = test_source();
        // Keep the file in place so the second pass observes the same content.
        source.archive_dir = String::new();
        let cfg = poll_cfg_arc();

        poll_fs(
            &source,
            &fs,
            Some(&GlobPattern::compile("*.log").unwrap()),
            &poll_db(&db),
            None,
            &cfg,
        )
        .await
        .expect("poll");

        // First pass: analyzed (analyzer init SQL emitted) + recorded.
        assert!(
            db.upserts.load(Ordering::SeqCst) >= 1,
            "processed-file record must be written"
        );

        // Second pass with the same content: the fake DB now knows the hash.
        let hash = content_hash(content.as_bytes());
        db.processed_hashes.lock().unwrap().insert(hash);

        let before = db.upserts.load(Ordering::SeqCst);
        poll_fs(
            &source,
            &fs,
            Some(&GlobPattern::compile("*.log").unwrap()),
            &poll_db(&db),
            None,
            &cfg,
        )
        .await
        .expect("poll 2");
        assert_eq!(
            db.upserts.load(Ordering::SeqCst),
            before,
            "deduped files must not be recorded again"
        );
    }

    #[tokio::test]
    async fn poll_fs_archives_and_skips_non_matching() {
        let db = Arc::new(MockPollingDb::new());
        let fs = FakeFs::new(vec![(
            "logs/app.log",
            "ERROR connection refused host=x\nERROR connection refused host=y\nERROR connection refused host=z\n",
        )]);
        let source = test_source();
        let cfg = poll_cfg_arc();

        poll_fs(
            &source,
            &fs,
            Some(&GlobPattern::compile("*.log").unwrap()),
            &poll_db(&db),
            None,
            &cfg,
        )
        .await
        .expect("poll");

        let renamed = fs.renamed.lock().unwrap();
        assert_eq!(
            renamed.first().map(|(f, t)| (f.as_str(), t.as_str())),
            Some(("logs/app.log", "archive/app.log")),
            "archive rename must target archive_dir/<name>"
        );
        assert!(
            fs.get("archive/app.log").contains("connection refused"),
            "renamed content must move into the archive"
        );
    }

    #[tokio::test]
    async fn poll_source_rejects_smb_with_d1_error() {
        let db = Arc::new(MockPollingDb::new());
        let mut source = test_source();
        source.protocol = "smb".into();
        let err = super::poll_source(&source, &poll_db(&db), None, &poll_cfg_arc())
            .await
            .expect_err("smb must fail");
        assert!(err.to_string().contains("D1"), "error: {err}");
    }

    #[tokio::test]
    async fn poll_source_rejects_unknown_protocol() {
        let db = Arc::new(MockPollingDb::new());
        let mut source = test_source();
        source.protocol = "gopher".into();
        let err = super::poll_source(&source, &poll_db(&db), None, &poll_cfg_arc())
            .await
            .expect_err("unknown protocol must fail");
        assert!(
            err.to_string().contains("unsupported protocol"),
            "error: {err}"
        );
    }

    #[tokio::test]
    async fn polling_manager_lifecycle() {
        let mut cfg = poll_cfg();
        cfg.log_ingestion.polling = vec![
            threski_core::config::PollingSource {
                name: "test-ftp".into(),
                enabled: true,
                protocol: "ftp".into(),
                host: "127.0.0.1".into(),
                port: 9999, // Unreachable port
                user: "user".into(),
                password: "pass".into(),
                remote_dir: "logs".into(),
                file_pattern: "*.log".into(),
                archive_dir: String::new(),
                poll_interval: "10ms".into(),
                project_name: String::new(),
                known_host_key: String::new(),
            },
            threski_core::config::PollingSource {
                name: "disabled-source".into(),
                enabled: false,
                protocol: "sftp".into(),
                host: "127.0.0.1".into(),
                port: 22,
                user: String::new(),
                password: String::new(),
                remote_dir: String::new(),
                file_pattern: String::new(),
                archive_dir: String::new(),
                poll_interval: "1h".into(),
                project_name: String::new(),
                known_host_key: String::new(),
            },
        ];

        let db = Arc::new(MockPollingDb::new());
        let mut pm = PollingManager::new(Arc::new(cfg), Some(db as Arc<dyn Executor>), None);

        // Start the manager.
        pm.start();

        // Allow some ticks/attempts to run.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Stop the manager (verifies clean shutdown and no deadlock).
        pm.stop().await;
    }

    #[tokio::test]
    async fn polling_manager_without_db_is_disabled() {
        let cfg = poll_cfg_arc();
        let mut pm = PollingManager::new(cfg, None, None);
        pm.start();
        assert!(pm.tasks.lock().unwrap().is_empty());
        pm.stop().await;
    }
}
