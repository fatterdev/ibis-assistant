//! Ollama lifecycle runner: health probe, on-demand daemon
//! start (`ollama serve` with logs beside the threski executable, ready ≤30s),
//! streaming model pull, binary discovery/install (Windows installer with the
//! exit-code-740 UAC retry), and the [`OllamaProcess`] handle the client uses
//! to stop the daemon.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;

use crate::error::AiError;

/// Health-check HTTP timeout.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// Daemon readiness window (30s).
const READY_WINDOW: Duration = Duration::from_secs(30);
const DEFAULT_API_URL: &str = "http://127.0.0.1:11434";
#[cfg(target_os = "windows")]
const OLLAMA_INSTALLER_URL: &str =
    "https://github.com/ollama/ollama/releases/latest/download/OllamaSetup.exe";
#[cfg(target_os = "linux")]
const OLLAMA_LINUX_URL: &str =
    "https://github.com/ollama/ollama/releases/latest/download/ollama-linux-amd64";
#[cfg(target_os = "macos")]
const OLLAMA_DARWIN_URL: &str =
    "https://github.com/ollama/ollama/releases/latest/download/ollama-darwin";

pub(crate) fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default()
}

/// Handle over the spawned `ollama serve` process. Dropping does NOT kill the
/// daemon (`exec.Cmd semantics); call [`OllamaProcess::stop`].
#[derive(Debug)]
pub struct OllamaProcess {
    child: Mutex<Option<std::process::Child>>,
}

impl OllamaProcess {
    pub fn pid(&self) -> Option<u32> {
        self.lock_child().as_ref().map(|c| c.id())
    }

    fn lock_child(&self) -> std::sync::MutexGuard<'_, Option<std::process::Child>> {
        self.child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Terminates the Ollama daemon process.
    pub async fn stop(&self) -> std::io::Result<()> {
        tracing::info!("Stopping local Ollama daemon...");
        match self.lock_child().take() {
            Some(mut child) => child.kill(),
            None => Ok(()),
        }
    }
}

fn current_exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn ollama_log_path() -> PathBuf {
    current_exe_dir().join("ollama.log")
}

fn ollama_binary_name() -> &'static str {
    if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    }
}

/// Stat-checked candidate paths, in the search order (before PATH):
/// exe dir, `bin/`, then on Windows `%LOCALAPPDATA%\Programs\Ollama` and
/// `%ProgramFiles%\Ollama`.
fn candidate_ollama_paths(
    exe_dir: &Path,
    local_app_data: Option<&str>,
    program_files: Option<&str>,
    windows: bool,
) -> Vec<PathBuf> {
    let name = if windows { "ollama.exe" } else { "ollama" };
    let mut paths = vec![exe_dir.join(name), exe_dir.join("bin").join(name)];
    if windows {
        if let Some(lad) = local_app_data.filter(|s| !s.is_empty()) {
            paths.push(
                Path::new(lad)
                    .join("Programs")
                    .join("Ollama")
                    .join("ollama.exe"),
            );
        }
        if let Some(pf) = program_files.filter(|s| !s.is_empty()) {
            paths.push(Path::new(pf).join("Ollama").join("ollama.exe"));
        }
    }
    paths
}

/// Minimal `exec.LookPath` equivalent over the PATH environment.
fn look_path(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Binary search order: exe dir, `bin/`, `%LOCALAPPDATA%\Programs\Ollama`,
/// `%ProgramFiles%\Ollama`, PATH (Windows); plus the standard unix
/// locations on other platforms.
pub fn find_ollama_exec() -> Result<PathBuf, AiError> {
    let exe_dir = current_exe_dir();
    let windows = cfg!(windows);
    let candidates = candidate_ollama_paths(
        &exe_dir,
        Some(&std::env::var("LOCALAPPDATA").unwrap_or_default()),
        Some(&std::env::var("ProgramFiles").unwrap_or_default()),
        windows,
    );
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
    }
    let name = ollama_binary_name();
    if let Some(path) = look_path(name) {
        return Ok(path);
    }
    if !windows {
        for path in [
            "/usr/local/bin/ollama",
            "/usr/bin/ollama",
            "/bin/ollama",
            "/Applications/Ollama.app/Contents/Resources/ollama",
        ] {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(AiError::other(
        "ollama executable not found in standard paths",
    ))
}

/// Launches `ollama serve` with stdout/stderr appended to `ollama.log` beside
/// the threski executable.
pub async fn start_ollama_daemon() -> Result<OllamaProcess, AiError> {
    let exec_path = ensure_ollama_binary().await?;
    let log_path = ollama_log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let mut cmd = std::process::Command::new(&exec_path);
    cmd.arg("serve");
    match log_file {
        Some(file) => match file.try_clone() {
            Ok(dup) => {
                cmd.stdout(std::process::Stdio::from(file));
                cmd.stderr(std::process::Stdio::from(dup));
            }
            Err(_) => {
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
            }
        },
        None => {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
    }

    tracing::info!(
        "Starting local Ollama daemon: {} serve",
        exec_path.display()
    );
    let child = cmd
        .spawn()
        .map_err(|e| AiError::other(format!("failed to start Ollama process: {e}")))?;
    Ok(OllamaProcess {
        child: Mutex::new(Some(child)),
    })
}

#[derive(Debug, Default)]
pub struct EnsureOllamaOutcome {
    /// `Some` only when this call started the daemon.
    pub process: Option<OllamaProcess>,
}

/// Checks if Ollama is running, starts/installs it when `auto_start`, and
/// pulls the model on-demand when `auto_pull`.
pub async fn ensure_ollama(
    api_url: &str,
    model: &str,
    auto_start: bool,
    auto_pull: bool,
) -> Result<EnsureOllamaOutcome, AiError> {
    let api_url = if api_url.is_empty() {
        DEFAULT_API_URL
    } else {
        api_url
    };
    let http = http_client(HEALTH_TIMEOUT);

    tracing::info!("Checking if Ollama is running at {api_url}...");
    let mut tags_body: Option<Vec<u8>> = None;
    let mut outcome = EnsureOllamaOutcome { process: None };

    match check_tags(&http, api_url).await {
        Ok(body) => {
            tracing::info!("Ollama is already running and responsive.");
            tags_body = Some(body);
        }
        Err(err) => {
            if !auto_start {
                return Err(AiError::other(format!(
                    "ollama is not running and auto-start is disabled: {err}"
                )));
            }
            tracing::warn!("Ollama is not running. Attempting to start local daemon...");
            let process = start_ollama_daemon()
                .await
                .map_err(|e| AiError::other(format!("failed to auto-start Ollama: {e}")))?;

            let mut ready = false;
            let deadline = tokio::time::Instant::now() + READY_WINDOW;
            while tokio::time::Instant::now() < deadline {
                match check_tags(&http, api_url).await {
                    Ok(body) => {
                        tags_body = Some(body);
                        ready = true;
                        break;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
            if !ready {
                let _ = process.stop().await;
                return Err(AiError::other(format!(
                    "ollama daemon started but failed to respond on {api_url} within 30s"
                )));
            }
            tracing::info!("Ollama daemon started successfully and is responsive.");
            outcome.process = Some(process);
        }
    }

    if auto_pull && !model.is_empty() {
        let installed = tags_body
            .as_ref()
            .map(|body| is_model_installed(body, model))
            .unwrap_or(false);
        if !installed {
            tracing::info!("Model {model} is not installed. Pulling model on-demand...");
            pull_model(api_url, model)
                .await
                .map_err(|e| AiError::other(format!("failed to pull model {model}: {e}")))?;
            tracing::info!("Model {model} successfully pulled.");
        } else {
            tracing::info!("Model {model} is already installed.");
        }
    }

    Ok(outcome)
}

async fn check_tags(http: &reqwest::Client, api_url: &str) -> Result<Vec<u8>, AiError> {
    let resp = http
        .get(format!("{api_url}/api/tags"))
        .send()
        .await
        .map_err(|e| AiError::other(e.to_string()))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(AiError::other(format!("status {status}")));
    }
    let bytes = resp.bytes().await?;
    Ok(bytes.to_vec())
}

#[derive(Debug, Deserialize, Default)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Debug, Deserialize, Default)]
struct TagModel {
    #[serde(default)]
    name: String,
}

/// `name == model` / `model:latest` / any `model:<tag>`.
pub fn is_model_installed(tags_response: &[u8], model: &str) -> bool {
    let Ok(tags) = serde_json::from_slice::<TagsResponse>(tags_response) else {
        return false;
    };
    let latest = format!("{model}:latest");
    let prefix = format!("{model}:");
    tags.models
        .iter()
        .any(|m| m.name == model || m.name == latest || m.name.starts_with(&prefix))
}

#[derive(Debug, Deserialize, Default)]
struct PullProgress {
    #[serde(default)]
    status: String,
    #[serde(default)]
    total: i64,
    #[serde(default)]
    completed: i64,
}

/// `POST {url}/api/pull` consuming the streaming NDJSON progress
///
pub async fn pull_model(api_url: &str, model: &str) -> Result<(), AiError> {
    let url = format!("{api_url}/api/pull");
    let body = serde_json::json!({ "name": model }).to_string();
    let http = reqwest::Client::new();
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| AiError::other(e.to_string()))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let raw = resp.bytes().await?;
        return Err(AiError::other(format!(
            "failed to pull model, status {status}: {}",
            String::from_utf8_lossy(&raw)
        )));
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(e) => return Err(AiError::other(e.to_string())),
        }
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            handle_progress_line(&line[..line.len() - 1], model)?;
        }
    }
    if !buf.iter().all(|&b| b.is_ascii_whitespace()) {
        handle_progress_line(&buf, model)?;
    }
    Ok(())
}

fn handle_progress_line(line: &[u8], model: &str) -> Result<(), AiError> {
    if line.iter().all(|&b| b.is_ascii_whitespace()) {
        return Ok(());
    }
    let progress: PullProgress =
        serde_json::from_slice(line).map_err(|e| AiError::other(e.to_string()))?;
    if progress.total > 0 {
        let pct = progress.completed as f64 / progress.total as f64 * 100.0;
        tracing::info!("Ollama pull progress ({model}): {pct:.2}% completed");
    } else {
        tracing::info!("Ollama pull: {}", progress.status);
    }
    Ok(())
}

/// Streams a download to `dest` (`downloadFile — the file is created
/// before the request, so a failed GET leaves an empty file).
async fn download_file(url: &str, dest: &Path) -> Result<(), AiError> {
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| AiError::other(e.to_string()))?;
    let resp = reqwest::get(url)
        .await
        .map_err(|e| AiError::other(e.to_string()))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(AiError::other(format!("download failed: {status}")));
    }
    let mut resp = resp;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                file.write_all(&chunk)
                    .await
                    .map_err(|e| AiError::other(e.to_string()))?;
            }
            Ok(None) => break,
            Err(e) => return Err(AiError::other(e.to_string())),
        }
    }
    file.flush()
        .await
        .map_err(|e| AiError::other(e.to_string()))?;
    Ok(())
}

/// Checks presence of the Ollama binary; otherwise downloads and installs it
/// on-demand.
pub async fn ensure_ollama_binary() -> Result<PathBuf, AiError> {
    if let Ok(path) = find_ollama_exec() {
        return Ok(path);
    }
    tracing::info!("Ollama binary not found on standard paths. Starting download on-demand...");

    #[cfg(target_os = "windows")]
    {
        install_ollama_windows().await
    }
    #[cfg(target_os = "linux")]
    {
        download_unix_ollama(OLLAMA_LINUX_URL).await
    }
    #[cfg(target_os = "macos")]
    {
        download_unix_ollama(OLLAMA_DARWIN_URL).await
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        Err(AiError::other("automatic download not supported on OS"))
    }
}

#[cfg(target_os = "windows")]
async fn install_ollama_windows() -> Result<PathBuf, AiError> {
    let setup_path = std::env::temp_dir().join("OllamaSetup.exe");
    tracing::info!(
        "Downloading Ollama installer from {OLLAMA_INSTALLER_URL} to {}...",
        setup_path.display()
    );
    download_file(OLLAMA_INSTALLER_URL, &setup_path)
        .await
        .map_err(|e| AiError::other(format!("failed to download Ollama setup: {e}")))?;

    let result = match run_silent_install(&setup_path).await {
        Ok(()) => Ok(()),
        Err((code, msg)) => {
            if is_elevation_required(code, &msg) {
                tracing::info!(
                    "Ollama installer requires admin privileges. Requesting UAC elevation via PowerShell..."
                );
                run_elevated_install(&setup_path).await
            } else {
                Err(AiError::other(format!(
                    "failed to execute Ollama installation: {msg}"
                )))
            }
        }
    };

    // Wait a brief moment for the installer to finish writing files, then
    // clean up and search again (sleep 3s, defer os.Remove, re-search).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = tokio::fs::remove_file(&setup_path).await;
    result?;

    find_ollama_exec().map_err(|e| {
        AiError::other(format!(
            "ollama installation completed but binary still not found: {e}"
        ))
    })
}

/// Runs the installer; returns the exit code and a -style error string so
/// the elevation heuristic can classify it.
#[cfg(target_os = "windows")]
async fn run_silent_install(setup_path: &Path) -> Result<(), (Option<i32>, String)> {
    match tokio::process::Command::new(setup_path)
        .arg("/VERYSILENT")
        .arg("/NORESTART")
        .status()
        .await
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            let code = status.code();
            Err((
                code,
                format!(
                    "exit status {}",
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string())
                ),
            ))
        }
        Err(e) => Err((None, e.to_string())),
    }
}

/// Windows `ERROR_ELEVATION_REQUIRED = 740` retry via
/// `powershell Start-Process -Verb RunAs` — a single UAC pass
///
#[cfg(target_os = "windows")]
async fn run_elevated_install(setup_path: &Path) -> Result<(), AiError> {
    let ps_cmd = format!(
        "Start-Process -FilePath '{}' -ArgumentList '/VERYSILENT','/NORESTART' -Verb RunAs -Wait",
        setup_path.display()
    );
    match tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .status()
        .await
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(AiError::other(format!(
            "Ollama installation requires administrator privileges. Please run the installer manually from: {}\n(PowerShell error: exit status {})",
            setup_path.display(),
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string())
        ))),
        Err(e) => Err(AiError::other(format!(
            "Ollama installation requires administrator privileges. Please run the installer manually from: {}\n(PowerShell error: {e})",
            setup_path.display()
        ))),
    }
}

/// `isElevationRequired: exit code 740 or the error-text heuristics
/// (including the Italian locale variant).
pub fn is_elevation_required(code: Option<i32>, msg: &str) -> bool {
    if code == Some(740) {
        return true;
    }
    let msg = msg.to_lowercase();
    msg.contains("740")
        || msg.contains("elevation required")
        || msg.contains("requires elevation")
        || msg.contains("access is denied")
        || msg.contains("accesso negato")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn download_unix_ollama(url: &str) -> Result<PathBuf, AiError> {
    let bin_dir = current_exe_dir().join("bin");
    let _ = tokio::fs::create_dir_all(&bin_dir).await;
    let dest = bin_dir.join("ollama");
    tracing::info!(
        "Downloading Ollama binary from {url} to {}...",
        dest.display()
    );
    download_file(url, &dest)
        .await
        .map_err(|e| AiError::other(format!("failed to download Ollama binary: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(|e| {
                AiError::other(format!(
                    "failed to set executable permission on Ollama binary: {e}"
                ))
            })?;
    }
    tracing::info!("Ollama successfully downloaded and marked executable.");
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::Router;

    fn ok(body: &str) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body.to_string())
            .unwrap()
            .into_response()
    }

    #[test]
    fn model_installed_matches_reference_rules() {
        let tags = br#"{"models":[{"name":"qwen2.5-coder:7b"},{"name":"granite4.1:3b"}]}"#;
        assert!(is_model_installed(tags, "qwen2.5-coder:7b"));
        assert!(!is_model_installed(tags, "nomic-embed-text"));
        assert!(!is_model_installed(b"garbage", "x"));

        let tags = br#"{"models":[{"name":"granite4.1:latest"}]}"#;
        assert!(is_model_installed(tags, "granite4.1"));
        assert!(is_model_installed(tags, "granite4.1:latest"));

        let tags = br#"{"models":[{"name":"granite4.1:7b-instruct"}]}"#;
        assert!(is_model_installed(tags, "granite4.1"));

        let tags = br#"{"models":[]}"#;
        assert!(!is_model_installed(tags, "granite4.1"));
    }

    #[test]
    fn elevation_required_heuristics() {
        assert!(is_elevation_required(Some(740), "exit status 1"));
        assert!(is_elevation_required(None, "exit status 740"));
        assert!(is_elevation_required(None, "Access is denied."));
        assert!(is_elevation_required(None, "Accesso negato."));
        assert!(is_elevation_required(None, "operation requires elevation"));
        assert!(!is_elevation_required(None, "exit status 1"));
        assert!(!is_elevation_required(Some(2), "file not found"));
    }

    #[test]
    fn candidate_paths_follow_reference_order() {
        let paths =
            candidate_ollama_paths(Path::new("/exe"), Some("C:\\LAD"), Some("C:\\PF"), true);
        assert_eq!(
            paths,
            vec![
                Path::new("/exe").join("ollama.exe"),
                Path::new("/exe").join("bin").join("ollama.exe"),
                Path::new("C:\\LAD")
                    .join("Programs")
                    .join("Ollama")
                    .join("ollama.exe"),
                Path::new("C:\\PF").join("Ollama").join("ollama.exe"),
            ]
        );
        let paths = candidate_ollama_paths(Path::new("/exe"), Some("x"), Some("y"), false);
        assert_eq!(
            paths,
            vec![
                Path::new("/exe").join("ollama"),
                Path::new("/exe").join("bin").join("ollama"),
            ]
        );
        let paths = candidate_ollama_paths(Path::new("/exe"), None, None, true);
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn ensure_ollama_noop_when_already_running() {
        let app = Router::new().route("/api/tags", get(|| async { ok(r#"{"models":[]}"#) }));
        let base_url = serve(app).await;
        let outcome = ensure_ollama(&base_url, "", false, false).await.unwrap();
        assert!(outcome.process.is_none());
    }

    #[tokio::test]
    async fn ensure_ollama_errors_without_auto_start() {
        // Port 1: nothing listens.
        let err = ensure_ollama("http://127.0.0.1:1", "m", false, false)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("ollama is not running and auto-start is disabled"));
    }

    #[tokio::test]
    async fn ensure_ollama_pulls_missing_model() {
        let pulled = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_state = pulled.clone();
        let app = Router::new()
            .route(
                "/api/tags",
                get(|| async { ok(r#"{"models":[{"name":"other:latest"}]}"#) }),
            )
            .route(
                "/api/pull",
                post(move |req: Request| {
                    let pulled = handler_state.clone();
                    async move {
                        let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                            .await
                            .unwrap()
                            .to_vec();
                        pulled.lock().unwrap().push(
                            String::from_utf8_lossy(&body).into_owned(),
                        );
                        ok(
                            "{\"status\":\"pulling manifest\"}\n{\"status\":\"downloading\",\"digest\":\"sha256:abc\",\"total\":100,\"completed\":50}\n{\"status\":\"success\"}\n",
                        )
                    }
                }),
            );
        let base_url = serve(app).await;
        let outcome = ensure_ollama(&base_url, "granite4.1:3b", false, true)
            .await
            .unwrap();
        assert!(outcome.process.is_none());
        assert_eq!(
            pulled.lock().unwrap().as_slice(),
            [r#"{"name":"granite4.1:3b"}"#]
        );
    }

    #[tokio::test]
    async fn ensure_ollama_skips_pull_when_installed() {
        let pulled = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_state = pulled.clone();
        let app = Router::new()
            .route(
                "/api/tags",
                get(|| async { ok(r#"{"models":[{"name":"granite4.1:latest"}]}"#) }),
            )
            .route(
                "/api/pull",
                post(move || {
                    let pulled = handler_state.clone();
                    async move {
                        pulled.lock().unwrap().push("called".to_string());
                        ok("{}")
                    }
                }),
            );
        let base_url = serve(app).await;
        ensure_ollama(&base_url, "granite4.1", false, true)
            .await
            .unwrap();
        assert!(pulled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pull_failure_propagates_wrapped() {
        let app = Router::new()
            .route("/api/tags", get(|| async { ok(r#"{"models":[]}"#) }))
            .route(
                "/api/pull",
                post(|| async {
                    Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body("no such model".to_string())
                        .unwrap()
                        .into_response()
                }),
            );
        let base_url = serve(app).await;
        let err = ensure_ollama(&base_url, "m", false, true)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "failed to pull model m: failed to pull model, status 404: no such model"
        );
    }

    #[tokio::test]
    async fn download_file_streams_to_disk() {
        let app = Router::new().route("/blob", get(|| async { ok_bytes(b"hello ollama payload") }));
        let base_url = serve(app).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("downloaded.bin");
        download_file(&format!("{base_url}/blob"), &dest)
            .await
            .unwrap();
        let content = std::fs::read(&dest).unwrap();
        assert_eq!(content, b"hello ollama payload");
    }

    #[tokio::test]
    async fn download_failure_non_200() {
        let app = Router::new().route(
            "/blob",
            get(|| async {
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body("no".to_string())
                    .unwrap()
                    .into_response()
            }),
        );
        let base_url = serve(app).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("downloaded.bin");
        let err = download_file(&format!("{base_url}/blob"), &dest)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "download failed: 403");
    }

    fn ok_bytes(body: &[u8]) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/octet-stream")
            .body(String::from_utf8_lossy(body).into_owned())
            .unwrap()
            .into_response()
    }

    #[tokio::test]
    async fn default_api_url_substituted() {
        // Empty api_url falls back to the loopback default; nothing answers on
        // the default port in tests, so auto_start=false produces the
        // disabled error.
        let err = ensure_ollama("", "m", false, false).await.unwrap_err();
        assert!(err.to_string().contains("auto-start is disabled"));
    }
}
