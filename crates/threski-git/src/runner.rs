use base64::Engine;
use regex::bytes::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::error::GitError;
use crate::types::Credential;

static BASIC_AUTH_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(https?://)([^:]+):([^@]+)(@)").expect("valid static regex"));

const REDACTED: &str = "[REDACTED]";

fn b64(input: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(input.as_bytes())
}

fn token_header(cred: &Credential) -> String {
    match cred.provider.to_lowercase().as_str() {
        "azure_devops" => format!("AUTHORIZATION: bearer {}", cred.token),
        "github" => format!(
            "Authorization: Basic {}",
            b64(&format!("git:{}", cred.token))
        ),
        "gitlab" => format!("Private-Token: {}", cred.token),
        _ => format!(
            "Authorization: Basic {}",
            b64(&format!("git:{}", cred.token))
        ),
    }
}

fn basic_header(cred: &Credential) -> String {
    format!(
        "Authorization: Basic {}",
        b64(&format!("{}:{}", cred.username, cred.token))
    )
}

/// Parses one line of `cat-file --batch` output per requested oid: each
/// response is `<oid> blob <size>\n<content>\n`; missing or non-blob objects
/// are skipped and parsing advances.
pub fn parse_cat_file_batch(requested: &[String], out: &[u8]) -> HashMap<String, String> {
    let mut bodies = HashMap::new();
    let mut buf = out;
    for want in requested {
        let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = &buf[..nl];
        buf = &buf[nl + 1..];
        let fields: Vec<&[u8]> = header
            .split(|&b| b.is_ascii_whitespace())
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() != 3 || fields[1] != b"blob" {
            continue;
        }
        let Some(size) = std::str::from_utf8(fields[2])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        else {
            break;
        };
        if size > buf.len() {
            break;
        }
        bodies.insert(
            want.clone(),
            String::from_utf8_lossy(&buf[..size]).trim().to_string(),
        );
        buf = &buf[size..];
        if buf.first() == Some(&b'\n') {
            buf = &buf[1..];
        }
    }
    bodies
}

/// Splits command output into trimmed, non-empty lines.
pub(crate) fn parse_lines(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn status_text(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => status.to_string(),
    }
}

/// Credential-injecting git command runner.
/// All git access in the product goes through this type — CLI only, never
/// libgit2 (D5). Output (success and failure) is secret-masked before it
/// leaves the runner.
#[derive(Clone)]
pub struct GitRunner {
    cred: Option<Credential>,
}

impl GitRunner {
    pub fn new(cred: Option<Credential>) -> Self {
        Self { cred }
    }

    pub fn anonymous() -> Self {
        Self { cred: None }
    }

    pub fn credential(&self) -> Option<&Credential> {
        self.cred.as_ref()
    }

    /// Runs `git <args>` in `dir`, prepending the credential prelude
    /// (`-c http.extraHeader=…` or a `GIT_SSH_COMMAND` env) exactly like the
    /// Scenario: runner. stdout and stderr are captured; output is masked.
    pub async fn run(&self, dir: &str, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let mut prelude: Vec<String> = Vec::new();
        let mut ssh_command: Option<String> = None;
        let mut key_dir: Option<tempfile::TempDir> = None;

        if let Some(cred) = &self.cred {
            match cred.auth_type.as_str() {
                "token" => {
                    prelude.push("-c".to_string());
                    prelude.push(format!("http.extraHeader={}", token_header(cred)));
                }
                "basic" => {
                    prelude.push("-c".to_string());
                    prelude.push(format!("http.extraHeader={}", basic_header(cred)));
                }
                "ssh" if !cred.ssh_private_key.is_empty() => {
                    // Isolated temp dir; user-scoped %TEMP% has restrictive
                    // ACLs on Windows (same reasoning as the original runner).
                    let tmp = tempfile::tempdir()
                        .map_err(|e| GitError::other(format!("prepare ssh key temp dir: {e}")))?;
                    let key_path = tmp.path().join("id");
                    std::fs::write(&key_path, cred.ssh_private_key.as_bytes())
                        .map_err(|e| GitError::other(format!("write ssh key temp file: {e}")))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &key_path,
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    // accept-new: accept first-time host keys, reject changed ones.
                    // Quoted path: %TEMP% may contain spaces ( bug, unquoted).
                    ssh_command = Some(format!(
                        "ssh -i \"{}\" -o StrictHostKeyChecking=accept-new",
                        key_path.display()
                    ));
                    key_dir = Some(tmp);
                }
                _ => {}
            }
        }

        self.execute(dir, &prelude, args, None, ssh_command, key_dir)
            .await
    }

    /// Runs `git <args>` feeding stdin, without the credential prelude: batch
    /// object checks and note reads are local operations.
    pub async fn run_with_stdin(
        &self,
        dir: &str,
        stdin: &str,
        args: &[&str],
    ) -> Result<Vec<u8>, GitError> {
        self.execute(dir, &[], args, Some(stdin), None, None).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        dir: &str,
        prelude: &[String],
        args: &[&str],
        stdin: Option<&str>,
        ssh_command: Option<String>,
        _key_dir: Option<tempfile::TempDir>,
    ) -> Result<Vec<u8>, GitError> {
        let mut cmd = Command::new("git");
        cmd.args(prelude).args(args);
        if !dir.is_empty() {
            cmd.current_dir(dir);
        }
        if let Some(ssh) = &ssh_command {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        cmd.stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(GitError::Spawn)?;

        let stdin_task = stdin.map(|input| {
            let mut pipe = child.stdin.take().expect("stdin piped");
            let input = input.to_string();
            tokio::spawn(async move {
                // Best-effort like the exec stdin copy: a child that exits
                // before reading stdin yields EPIPE, which is not fatal here.
                let _ = pipe.write_all(input.as_bytes()).await;
                let _ = pipe.flush().await;
                drop(pipe);
            })
        });

        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        let (read_out, read_err, waited, _stdin_done) = tokio::join!(
            stdout_pipe.read_to_end(&mut stdout_buf),
            stderr_pipe.read_to_end(&mut stderr_buf),
            child.wait(),
            async {
                if let Some(task) = stdin_task {
                    let _ = task.await;
                }
            }
        );
        read_out.map_err(GitError::Spawn)?;
        read_err.map_err(GitError::Spawn)?;
        let status = waited.map_err(GitError::Spawn)?;

        // stdout first, then stderr: git writes payloads to stdout and
        // diagnostics to stderr, so parsers see the same content 's
        // interleaved CombinedOutput produced.
        let mut combined = stdout_buf;
        combined.extend_from_slice(&stderr_buf);
        let sanitized = self.mask_secrets(&combined);

        if status.success() {
            return Ok(sanitized);
        }
        Err(GitError::Command {
            status: status_text(&status),
            output: String::from_utf8_lossy(&sanitized).into_owned(),
        })
    }

    /// Redacts credentials embedded in URLs and every secret derived from the
    /// active credential (token, its base64 forms, SSH key lines) from
    /// command output. Mirrors `maskSecrets exactly — masking operates at
    /// the byte level ( strings carry raw bytes), so unmatched spans survive
    /// verbatim and non-UTF-8 output is never mangled by a lossy conversion
    /// (D-A13: `file_at_ref` consumers need exact bytes for UTF-8 validation,
    /// Scenario: f40bf82).
    pub fn mask_secrets(&self, input: &[u8]) -> Vec<u8> {
        let mut out = BASIC_AUTH_URL
            .replace_all(input, &b"${1}[REDACTED]:[REDACTED]${4}"[..])
            .into_owned();

        let Some(cred) = &self.cred else {
            return out;
        };

        let mut secrets: Vec<Vec<u8>> = Vec::new();
        if !cred.token.is_empty() {
            secrets.push(cred.token.as_bytes().to_vec());
            secrets.push(b64(&format!("git:{}", cred.token)).into_bytes());
            if !cred.username.is_empty() {
                secrets.push(b64(&format!("{}:{}", cred.username, cred.token)).into_bytes());
            }
        }
        if !cred.ssh_private_key.is_empty() {
            for line in cred.ssh_private_key.split('\n') {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.contains("BEGIN") && !trimmed.contains("END") {
                    secrets.push(trimmed.as_bytes().to_vec());
                }
            }
        }

        for secret in secrets {
            if secret.len() > 4 {
                out = replace_bytes(&out, &secret, REDACTED.as_bytes());
            }
        }

        out
    }
}

/// Sequential non-overlapping byte substitution (`strings.Replace single
/// needle semantics; the pre-port Rust `str::replace` on a lossy string
/// behaved identically).
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&haystack[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_header_per_provider() {
        let mut cred = Credential::token_credential("t", "github", "tok123");
        assert_eq!(
            token_header(&cred),
            format!("Authorization: Basic {}", b64("git:tok123"))
        );
        cred.provider = "azure_devops".to_string();
        assert_eq!(token_header(&cred), "AUTHORIZATION: bearer tok123");
        cred.provider = "gitlab".to_string();
        assert_eq!(token_header(&cred), "Private-Token: tok123");
        cred.provider = "generic".to_string();
        assert_eq!(
            token_header(&cred),
            format!("Authorization: Basic {}", b64("git:tok123"))
        );
        // provider matching is case-insensitive (strings.ToLower)
        cred.provider = "AZURE_DEVOPS".to_string();
        assert_eq!(token_header(&cred), "AUTHORIZATION: bearer tok123");
    }

    #[test]
    fn basic_header_encodes_user_token() {
        let cred = Credential {
            username: "user".to_string(),
            token: "pass".to_string(),
            ..Default::default()
        };
        assert_eq!(
            basic_header(&cred),
            format!("Authorization: Basic {}", b64("user:pass"))
        );
    }

    #[test]
    fn mask_secrets_masks_ssh_key_lines_but_not_markers() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nabcDEFkey123\n-----END OPENSSH PRIVATE KEY-----\n";
        let cred = Credential {
            ssh_private_key: key.to_string(),
            ..Default::default()
        };
        let runner = GitRunner::new(Some(cred));
        let masked =
            runner.mask_secrets(b"error: abcDEFkey123 near -----BEGIN OPENSSH PRIVATE KEY-----");
        let text = String::from_utf8(masked).unwrap();
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("abcDEFkey123"));
        assert!(text.contains("BEGIN OPENSSH PRIVATE KEY"));
    }

    #[test]
    fn short_secrets_are_not_replaced() {
        let cred = Credential::token_credential("t", "generic", "ab");
        let runner = GitRunner::new(Some(cred));
        let masked = runner.mask_secrets(b"token ab here");
        assert_eq!(masked, b"token ab here");
    }

    #[test]
    fn anonymous_runner_still_masks_url_credentials() {
        let runner = GitRunner::anonymous();
        let masked = runner.mask_secrets(b"from https://user:pass@host/x.git");
        let text = String::from_utf8(masked).unwrap();
        assert!(!text.contains("user:pass"));
        assert!(text.contains("https://[REDACTED]:[REDACTED]@host/x.git"));
    }

    // Byte-exact masking: strings carry raw bytes, so unmasked spans of
    // non-UTF-8 output must survive verbatim (D-A13 contract; the lossy
    // String round-trip this crate used before mangled them).
    #[test]
    fn mask_secrets_preserves_non_utf8_bytes() {
        let runner = GitRunner::anonymous();
        let raw = b"before \xff\xfe\x81 after";
        assert_eq!(runner.mask_secrets(raw), raw.to_vec());

        let cred = Credential {
            token: "tok12345".to_string(),
            ..Default::default()
        };
        let runner = GitRunner::new(Some(cred));
        assert_eq!(runner.mask_secrets(raw), raw.to_vec());
    }

    #[test]
    fn mask_secrets_masks_span_but_keeps_surrounding_raw_bytes() {
        let runner = GitRunner::anonymous();
        let raw = b"\xff\xfe https://user:pass@host/x.git \x81\x82".to_vec();
        let mut expected = b"\xff\xfe".to_vec();
        expected.extend_from_slice(b" https://[REDACTED]:[REDACTED]@host/x.git \x81\x82");
        assert_eq!(runner.mask_secrets(&raw), expected);
    }

    #[test]
    fn parse_cat_file_batch_roundtrip() {
        let requested = vec!["a1".to_string(), "b2".to_string(), "c3".to_string()];
        let mut out = b"a1 blob 5\nhello\n\n".to_vec();
        out.extend_from_slice(b"c3 missing\n");
        let bodies = parse_cat_file_batch(&requested, &out);
        assert_eq!(bodies.get("a1").map(String::as_str), Some("hello"));
        assert!(!bodies.contains_key("b2"));
        assert!(!bodies.contains_key("c3"));
    }
}
