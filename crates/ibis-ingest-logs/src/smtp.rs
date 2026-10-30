//! Minimal hand-rolled SMTP client over tokio. Wire sequence: EHLO, optional
//! STARTTLS upgrade, optional AUTH PLAIN (refused over unencrypted
//! non-loopback connections guard), MAIL FROM, RCPT TO,
//! DATA with dot-stuffing, QUIT.

use std::sync::Arc;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{LogsError, Result};

/// Cap on a single SMTP reply line ( bufio.Scanner: 64 KiB).
const MAX_REPLY_LINE: usize = 64 * 1024;

pub(crate) enum Stream {
    Tcp(tokio::net::TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
    /// Transient placeholder while the TCP stream is being upgraded to TLS.
    Upgrading,
}

impl Stream {
    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Stream::Tcp(s) => s.write_all(buf).await,
            Stream::Tls(s) => s.write_all(buf).await,
            Stream::Upgrading => Err(std::io::Error::other("smtp stream is upgrading")),
        }
    }

    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf).await,
            Stream::Tls(s) => s.read(buf).await,
            Stream::Upgrading => Err(std::io::Error::other("smtp stream is upgrading")),
        }
    }

    fn is_tls(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }
}

pub(crate) struct SmtpConnection {
    stream: Stream,
    read_buf: Vec<u8>,
}

pub(crate) async fn tls_connect(addr: &str, host: &str) -> Result<Stream> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| LogsError::smtp(format!("invalid TLS server name {host:?}: {e}")))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls_config()));
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| LogsError::smtp(format!("TLS handshake failed: {e}")))?;
    Ok(Stream::Tls(Box::new(tls)))
}

fn client_tls_config() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("tls protocol versions");
    builder
        .with_root_certificates(root_store())
        .with_no_client_auth()
}

fn root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.to_vec());
    roots
}

/// True for hosts that may receive credentials without TLS
/// (loopback hosts).
fn is_localhost(host: &str) -> bool {
    matches!(
        host.to_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

impl SmtpConnection {
    pub(crate) fn new(stream: Stream) -> Self {
        SmtpConnection {
            stream,
            read_buf: Vec::new(),
        }
    }

    /// Reads one CRLF-terminated reply line.
    async fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(pos) = self.read_buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.read_buf.drain(..=pos).collect();
                let mut s = String::from_utf8_lossy(&line).into_owned();
                while s.ends_with('\n') || s.ends_with('\r') {
                    s.pop();
                }
                if s.len() > MAX_REPLY_LINE {
                    return Err(LogsError::smtp("smtp reply line too long"));
                }
                return Ok(s);
            }
            let mut chunk = [0u8; 4096];
            let n = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| LogsError::smtp(format!("read failed: {e}")))?;
            if n == 0 {
                return Err(LogsError::smtp("connection closed by server"));
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
            if self.read_buf.len() > MAX_REPLY_LINE * 4 {
                return Err(LogsError::smtp("smtp reply exceeds buffer"));
            }
        }
    }

    /// Reads a (possibly multiline) reply, returning the last line's code.
    async fn read_response(&mut self) -> Result<String> {
        loop {
            let line = self.read_line().await?;
            let code = line.chars().take(3).collect::<String>();
            if line.len() >= 4 && line.as_bytes()[3] == b'-' {
                continue;
            }
            return Ok(format!("{code} {line}"));
        }
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        self.stream
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .map_err(|e| LogsError::smtp(format!("write failed: {e}")))
    }

    /// Upgrades a plaintext connection to TLS after the server's 220 reply
    ///
    pub(crate) async fn starttls(&mut self, host: &str) -> Result<()> {
        self.write_line("STARTTLS").await?;
        let resp = self.read_response().await?;
        if !resp.starts_with("220") {
            return Err(LogsError::smtp(format!("STARTTLS rejected: {resp}")));
        }
        let Stream::Tcp(tcp) = std::mem::replace(&mut self.stream, Stream::Upgrading) else {
            return Err(LogsError::smtp("STARTTLS on non-tcp stream"));
        };
        let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| LogsError::smtp(format!("invalid TLS server name {host:?}: {e}")))?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls_config()));
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| LogsError::smtp(format!("failed to start TLS: {e}")))?;
        self.stream = Stream::Tls(Box::new(tls));
        Ok(())
    }

    /// Runs EHLO → AUTH → MAIL → RCPT → DATA → QUIT (`SendRawEmail body).
    pub(crate) async fn run_session(
        &mut self,
        smtp: &ibis_core::config::SMTPConfig,
        msg: &[u8],
    ) -> Result<()> {
        // Greeting
        let greeting = self.read_response().await?;
        if !greeting.starts_with("220") {
            return Err(LogsError::smtp(format!("unexpected greeting: {greeting}")));
        }

        // EHLO
        self.write_line("EHLO localhost").await?;
        let ehlo = self.read_response().await?;
        if !ehlo.starts_with("250") {
            return Err(LogsError::smtp(format!("EHLO failed: {ehlo}")));
        }

        // AUTH PLAIN — only over TLS or loopback ( smtp.PlainAuth guard).
        if !smtp.user.is_empty() && !smtp.password.is_empty() {
            if !self.stream.is_tls() && !is_localhost(&smtp.host) {
                return Err(LogsError::smtp("unencrypted connection"));
            }
            let payload = base64::engine::general_purpose::STANDARD
                .encode(format!("\0{}\0{}", smtp.user, smtp.password));
            self.write_line(&format!("AUTH PLAIN {payload}")).await?;
            let resp = self.read_response().await?;
            if !resp.starts_with("235") {
                return Err(LogsError::smtp(format!("failed to authenticate: {resp}")));
            }
        }

        // MAIL FROM / RCPT TO / DATA
        self.write_line(&format!("MAIL FROM:<{}>", smtp.from))
            .await?;
        let resp = self.read_response().await?;
        if !resp.starts_with("250") {
            return Err(LogsError::smtp(format!("failed to set MAIL FROM: {resp}")));
        }
        self.write_line(&format!("RCPT TO:<{}>", smtp.to)).await?;
        let resp = self.read_response().await?;
        if !resp.starts_with("250") {
            return Err(LogsError::smtp(format!("failed to set RCPT TO: {resp}")));
        }
        self.write_line("DATA").await?;
        let resp = self.read_response().await?;
        if !resp.starts_with("354") {
            return Err(LogsError::smtp(format!(
                "failed to open DATA writer: {resp}"
            )));
        }

        // Message body with dot-stuffing (RFC 5321).
        let mut body = Vec::with_capacity(msg.len() + 8);
        for line in split_message_lines(msg) {
            if line.starts_with(b".") {
                body.push(b'.');
            }
            body.extend_from_slice(line);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b".\r\n");
        self.stream
            .write_all(&body)
            .await
            .map_err(|e| LogsError::smtp(format!("failed to write message: {e}")))?;
        let resp = self.read_response().await?;
        if !resp.starts_with("250") {
            return Err(LogsError::smtp(format!(
                "failed to close DATA writer: {resp}"
            )));
        }

        // QUIT
        self.write_line("QUIT").await?;
        let _ = self.read_response().await?;
        Ok(())
    }
}

/// Splits the raw message into CRLF-agnostic lines for dot-stuffing.
fn split_message_lines(msg: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in msg.iter().enumerate() {
        if *b == b'\n' {
            let mut end = i;
            if end > start && msg[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(&msg[start..end]);
            start = i + 1;
        }
    }
    if start < msg.len() {
        out.push(&msg[start..]);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! Mock SMTP server for the notifier tests:
    //! records every client command (and DATA lines) for assertions.

    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::tcp::OwnedReadHalf;

    #[derive(Clone)]
    pub(crate) struct MockSmtpServer {
        port: i64,
        commands: Arc<Mutex<Vec<String>>>,
        data: Arc<Mutex<Vec<String>>>,
    }

    async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Option<String> {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            match reader.read(&mut byte).await {
                Ok(0) => return None,
                Ok(_) => {
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                    if line.len() > 64 * 1024 {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
        let s = String::from_utf8_lossy(&line).into_owned();
        Some(s.trim_end_matches(['\r', '\n']).to_string())
    }

    impl MockSmtpServer {
        pub(crate) async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock smtp");
            let port = listener.local_addr().unwrap().port() as i64;
            let commands = Arc::new(Mutex::new(Vec::new()));
            let data = Arc::new(Mutex::new(Vec::new()));

            let cmds = commands.clone();
            let data_store = data.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let cmds = cmds.clone();
                    let data_store = data_store.clone();
                    tokio::spawn(async move {
                        let (reader, mut writer) = stream.into_split();
                        let mut reader = BufReader::new(reader);
                        writer.write_all(b"220 mock-smtp-server\r\n").await.ok();
                        loop {
                            let Some(line) = read_line(&mut reader).await else {
                                return;
                            };
                            let cmd = line.trim().to_string();
                            cmds.lock().unwrap().push(cmd.clone());
                            let upper = cmd.to_uppercase();
                            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                                writer
                                    .write_all(b"250-mock-smtp-server\r\n250 AUTH PLAIN\r\n")
                                    .await
                                    .ok();
                            } else if upper.starts_with("MAIL FROM:") {
                                writer.write_all(b"250 2.1.0 OK\r\n").await.ok();
                            } else if upper.starts_with("RCPT TO:") {
                                writer.write_all(b"250 2.1.5 OK\r\n").await.ok();
                            } else if upper == "DATA" {
                                writer.write_all(b"354 Start mail input\r\n").await.ok();
                                loop {
                                    let Some(data_line) = read_line(&mut reader).await else {
                                        return;
                                    };
                                    let trimmed = data_line.trim().to_string();
                                    if trimmed == "." {
                                        break;
                                    }
                                    if trimmed.is_empty() {
                                        continue;
                                    }
                                    let entry = format!("DATA: {trimmed}");
                                    // the mock pushed DATA lines into the same
                                    // commands channel; `data` additionally keeps
                                    // them for body assertions.
                                    cmds.lock().unwrap().push(entry.clone());
                                    data_store.lock().unwrap().push(entry);
                                }
                                writer.write_all(b"250 2.0.0 OK\r\n").await.ok();
                            } else if upper == "QUIT" {
                                writer.write_all(b"221 2.0.0 Bye\r\n").await.ok();
                                return;
                            } else if upper.starts_with("AUTH PLAIN") {
                                writer
                                    .write_all(b"235 2.7.0 Authentication successful\r\n")
                                    .await
                                    .ok();
                            } else {
                                writer.write_all(b"250 OK\r\n").await.ok();
                            }
                        }
                    });
                }
            });

            MockSmtpServer {
                port,
                commands,
                data,
            }
        }

        pub(crate) fn port(&self) -> i64 {
            self.port
        }

        pub(crate) async fn drain_commands(&self) -> Vec<String> {
            std::mem::take(&mut *self.commands.lock().unwrap())
        }

        pub(crate) async fn drain_data(&self) -> Vec<String> {
            std::mem::take(&mut *self.data.lock().unwrap())
        }
    }
}
