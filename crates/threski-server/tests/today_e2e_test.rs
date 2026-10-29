//! E2E launch gate binary
//! scenarios plus `06` DoD #4: build the real binary, launch
//! `run --mode stdio --runtime-mode plugin`, complete the MCP stdio
//! handshake over NDJSON, bind the workspace through the plugin ListRoots
//! hook, run one memory tool, terminate cleanly.
//!
//! mirrored behaviors:
//! - `TestTodayE2EPluginBinaryNoWorkspace`: plugin without workspace must
//!   complete MCP `initialize`, not hang;
//! - `TestTodayE2EPluginBinary` (decoy): plugin launched with a workspace
//!   must not write into the CWD `db` (plugin isolation remap);
//! - #4 (no counterpart — bound roots only implicitly):
//!   handshake-only mode serves `tools/list` (56 tools) while workspace/DB
//!   tool calls degrade exactly like the nil `dbClient`
//!   (`get_working_state failed: database not connected`,
//!   ), then bind via roots and serve a real memory read.
//!
//! Build seam: ran inside the test; the
//! Rust equivalent is cargo's `CARGO_BIN_EXE_threski` contract (cargo
//! builds the bin before running this integration test), with a
//! `target/debug` fallback — same property (the gate drives the freshly
//! built real binary), no nested-cargo deadlock. Sidecar binaries are
//! seeded beside the exe (PATH / spike cache, else the same GitHub install
//! path the product uses at startup) with `DB_AUTO_UPDATE=false` so the
//! child takes the local-binary fast path after the seed.

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Locates the assistant executable for the e2e run.
fn assistant_exe() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_threski") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    // Fallback for direct test-binary runs: the workspace target dir.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .ancestors()
        .nth(2)
        .expect("manifest sits at <root>/crates/threski-server")
        .to_path_buf();
    let bin_name = if cfg!(windows) {
        "threski.exe"
    } else {
        "threski"
    };
    let candidate = workspace_root.join("target").join("debug").join(bin_name);
    assert!(
        candidate.exists(),
        "{bin_name} not built — run `cargo build -p threski-server` (or use cargo test)"
    );
    candidate
}

fn sidecar_bin_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// First PATH hit for a bare binary name.
fn locate_on_path(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let out = Command::new("where.exe").arg(name).output().ok()?;
    #[cfg(not(windows))]
    let out = Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .next()
}

/// The legacy spike sidecar binary; used only as a fallback candidate for seeding.
fn spike_surreal() -> Option<PathBuf> {
    let temp = std::env::var_os("TEMP").or_else(|| std::env::var_os("TMPDIR"))?;
    let path = PathBuf::from(temp)
        .join("opencode")
        .join("spike")
        .join(sidecar_bin_name("surreal"));
    path.is_file().then_some(path)
}

fn block_on_download<F, T, E>(label: &str, fut: F) -> T
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for sidecar download");
    match rt.block_on(fut) {
        Ok(v) => v,
        Err(err) => panic!("failed to download {label} for e2e seed: {err}"),
    }
}

static SEED_LOCK: Mutex<()> = Mutex::new(());

/// Seeds `surreal` / `sg` beside the threski executable so
/// `ensure_surreal_db(false)` / `ensure_ast_grep(false)` take the
/// "local binary found" fast path. Prefers PATH (or the spike cache), then
/// downloads the same GitHub release assets the product installs at startup —
/// CI runners have neither binary on PATH.
fn seed_sidecar_binaries(exe: &Path) {
    let _guard = SEED_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = exe.parent().expect("exe has a parent").to_path_buf();

    let surreal = dir.join(sidecar_bin_name("surreal"));
    if !surreal.exists() {
        if let Some(src) = locate_on_path("surreal").or_else(spike_surreal) {
            std::fs::copy(&src, &surreal).unwrap_or_else(|e| {
                panic!(
                    "seed {} from {}: {e}",
                    surreal.display(),
                    src.display()
                )
            });
        } else {
            block_on_download("surreal", async {
                let release = threski_db::process::install::latest_release().await?;
                threski_db::process::install::install_surreal(&release, &surreal).await
            });
        }
        assert!(
            surreal.is_file(),
            "surreal sidecar missing after seed: {}",
            surreal.display()
        );
    }

    let sg = dir.join(sidecar_bin_name("sg"));
    if !sg.exists() {
        if let Some(src) = locate_on_path("sg") {
            std::fs::copy(&src, &sg).unwrap_or_else(|e| {
                panic!("seed {} from {}: {e}", sg.display(), src.display())
            });
        } else {
            block_on_download("ast-grep (sg)", async {
                let release = threski_ingest_code::install::latest_release().await?;
                threski_ingest_code::install::install_ast_grep(&release, &sg).await
            });
        }
        assert!(
            sg.is_file(),
            "sg sidecar missing after seed: {}",
            sg.display()
        );
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("free port probe")
        .local_addr()
        .unwrap()
        .port()
}

/// Child env: inherit minus the workspace-selection variables (
/// `envWithout(os.Environ, "THRESKI_WORKSPACE")`, extended to every
/// workspace candidate key), plus the test overrides.
fn child_env(overrides: &[(&str, String)]) -> Vec<(String, String)> {
    let skip = [
        "THRESKI_WORKSPACE",
        "CURSOR_WORKSPACE_FOLDER",
        "CURSOR_WORKSPACE",
        "VSCODE_WORKSPACE_FOLDER",
    ];
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .collect();
    for (k, v) in overrides {
        env.retain(|(name, _)| name != k);
        env.push(((*k).to_string(), v.clone()));
    }
    env
}

struct E2eChild {
    proc: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    pending_server_requests: Vec<(Value, String)>,
}

impl E2eChild {
    fn spawn(exe: &Path, args: &[&str], cwd: &Path, env: &[(String, String)]) -> E2eChild {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut proc = cmd.spawn().expect("spawn threski");
        let stdin = proc.stdin.take().expect("stdin piped");
        let stdout = proc.stdout.take().expect("stdout piped");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        E2eChild {
            proc,
            stdin: Some(stdin),
            rx,
            pending_server_requests: Vec::new(),
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).expect("write stdin");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush stdin");
    }

    fn close_stdin(&mut self) {
        self.stdin.take(); // EOF, like the fixed-string stdin reader
    }

    /// Next JSON message before `deadline`; server requests (method+id) are
    /// stashed for `take_server_request`, bare notifications are skipped.
    fn next_message(&mut self, deadline: Instant) -> Option<Value> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(_) => continue, // non-JSON stdout noise is skipped
                    };
                    if value.get("method").is_some() {
                        if let Some(id) = value.get("id") {
                            self.pending_server_requests
                                .push((id.clone(), value["method"].as_str().unwrap_or("").into()));
                        }
                        continue;
                    }
                    return Some(value);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn take_server_request(&mut self, method: &str) -> Option<Value> {
        let idx = self
            .pending_server_requests
            .iter()
            .position(|(_, m)| m == method)?;
        Some(self.pending_server_requests.remove(idx).0)
    }

    /// Waits until the server sends a `method` request (or `timeout` elapses).
    /// Runs its own recv loop because [`Self::next_message`] stashes server
    /// requests internally and only surfaces responses.
    fn wait_for_server_request(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(id) = self.take_server_request(method) {
                return Some(id);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                        continue;
                    };
                    if value.get("method").is_some() {
                        if let Some(id) = value.get("id") {
                            self.pending_server_requests
                                .push((id.clone(), value["method"].as_str().unwrap_or("").into()));
                        }
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Sends a request and waits for the response with the matching id.
    fn request(&mut self, id: u64, method: &str, params: Value, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        self.send(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        );
        loop {
            let msg = self
                .next_message(deadline)
                .unwrap_or_else(|| panic!("no response to {method} (id {id})"));
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    fn respond(&mut self, id: &Value, result: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.proc.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// the defensive process-tree kill (taskkill /T /F), best-effort.
    fn kill_tree(&mut self) {
        let pid = self.proc.id();
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
        let _ = self.proc.wait();
    }

    /// Tail of the plugin log file for failure diagnostics
    /// (plugin isolation remaps the log into THRESKI_DATA_DIR).
    fn plugin_log_tail(data_dir: &Path) -> String {
        let log = data_dir.join("plugin.log");
        std::fs::read_to_string(&log)
            .map(|s| {
                let lines: Vec<&str> = s.lines().collect();
                let start = lines.len().saturating_sub(40);
                lines[start..].join("\n")
            })
            .unwrap_or_else(|e| format!("<no plugin.log: {e}>"))
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "e2e", "version": "0"}
    })
}

/// `TestTodayE2EPluginBinaryNoWorkspace: `run --mode stdio
/// --runtime-mode plugin` with NO workspace must complete the MCP
/// `initialize` handshake (handshake-only mode) and exit when stdin closes —
/// never hang.
#[test]
fn plugin_without_workspace_completes_initialize() {
    let exe = assistant_exe();
    let cwd = tempfile::tempdir().expect("cwd tempdir");
    let data = tempfile::tempdir().expect("data tempdir");
    let env = child_env(&[
        ("RUNTIME_MODE", "plugin".into()),
        ("THRESKI_DATA_DIR", data.path().display().to_string()),
        ("EMBEDDING_PROVIDER", "none".into()),
        ("DB_AUTO_UPDATE", "false".into()),
        ("LOG_LEVEL", "ERROR".into()),
    ]);
    let mut child = E2eChild::spawn(
        &exe,
        &[
            "run",
            "--mode",
            "stdio",
            "--runtime-mode",
            "plugin",
            "--scan",
            "false",
        ],
        cwd.path(),
        &env,
    );

    child.send(
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": initialize_params()
        })
        .to_string(),
    );
    child.close_stdin();

    let deadline = Instant::now() + Duration::from_secs(30);
    let resp = child
        .next_message(deadline)
        .expect("plugin without workspace must complete MCP initialize, not hang");
    assert_eq!(resp["id"], json!(1), "initialize response: {resp}");
    assert!(
        resp.get("result").is_some() && resp.get("error").is_none(),
        "initialize must succeed, got {resp}"
    );
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "Threski Studio",
        "MCP initialize on stdout: {resp}"
    );

    // stdin is closed: the process must terminate cleanly ( waited 15s
    // then failed with "must complete MCP initialize, not hang").
    let status = child
        .wait_for_exit(Duration::from_secs(30))
        .expect("process must exit when stdin closes");
    assert!(
        status.success(),
        "clean exit expected after stdin close, got {status:?}"
    );
    child.kill_tree();
}

/// `TestTodayE2EPluginBinary (decoy): the plugin launched with a
/// workspace must not write into the CWD `db` — plugin isolation remaps
/// data paths into `THRESKI_DATA_DIR`.
#[test]
fn plugin_with_workspace_does_not_write_into_cwd_db() {
    let exe = assistant_exe();
    seed_sidecar_binaries(&exe);
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();

    let cwd = tempfile::tempdir().expect("cwd tempdir");
    let decoy = cwd.path().join("db");
    std::fs::create_dir_all(&decoy).expect("decoy db dir");
    std::fs::write(decoy.join("PERSONAL_MARKER"), b"personal").expect("decoy marker");

    let data = tempfile::tempdir().expect("data tempdir");
    let db_port = free_port();
    let env = child_env(&[
        ("RUNTIME_MODE", "plugin".into()),
        ("THRESKI_DATA_DIR", data.path().display().to_string()),
        ("THRESKI_WORKSPACE", workspace_root.display().to_string()),
        ("EMBEDDING_PROVIDER", "none".into()),
        ("DB_AUTO_UPDATE", "false".into()),
        ("LOG_LEVEL", "ERROR".into()),
        ("SURREAL_URL", format!("ws://127.0.0.1:{db_port}/rpc")),
    ]);
    let mut child = E2eChild::spawn(
        &exe,
        &[
            "run",
            "--mode",
            "stdio",
            "--runtime-mode",
            "plugin",
            "--scan",
            "false",
            "--workspace",
            &workspace_root.display().to_string(),
        ],
        cwd.path(),
        &env,
    );

    // empty stdin (immediate EOF); the process runs, then exits.
    child.close_stdin();

    // Startup binds the workspace and boots the embedded DB (seeded local
    // surreal); the graceful shutdown then stops it. Wait for a natural
    // exit — killed at its 20s context and discarded the exit status
    // (`out, _ := cmd.CombinedOutput`); a no-initialize EOF surfaces as
    // rmcp's `connection closed: initialize request` error, so only the
    // exit-within-deadline property is pinned here (parity).
    let exited = child.wait_for_exit(Duration::from_secs(150)).is_some();
    child.kill_tree();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        exited,
        "plugin process did not exit; plugin.log tail:\n{}",
        E2eChild::plugin_log_tail(data.path())
    );

    // `if !fileExists(filepath.Join(cwd, "db", "PERSONAL_MARKER"))` and
    // `len(entries) != 1` — the decoy must be untouched.
    let entries: Vec<String> = std::fs::read_dir(&decoy)
        .expect("decoy dir readable")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["PERSONAL_MARKER".to_string()],
        "plugin wrote into cwd db: {entries:?}"
    );
    let marker = std::fs::read_to_string(decoy.join("PERSONAL_MARKER")).expect("marker intact");
    assert_eq!(marker, "personal");
}

/// `06` DoD #4 — the full plugin session: handshake-only mode serves
/// `tools/list` (56 tools) while workspace/DB tool calls degrade likewise's
/// nil `dbClient`; the plugin ListRoots hook binds the workspace; the same
/// memory tool then serves a real read; stdin close terminates cleanly.
#[test]
fn plugin_session_binds_roots_and_serves_memory_tool() {
    let exe = assistant_exe();
    seed_sidecar_binaries(&exe);

    // EMPTY workspace: a fresh temp dir, deliberately not a git repo — the
    // plugin binds it through the roots flow, not at startup.
    let cwd = tempfile::tempdir().expect("cwd tempdir");
    let data = tempfile::tempdir().expect("data tempdir");
    let db_port = free_port();
    let env = child_env(&[
        ("RUNTIME_MODE", "plugin".into()),
        ("THRESKI_DATA_DIR", data.path().display().to_string()),
        ("EMBEDDING_PROVIDER", "none".into()),
        ("DB_AUTO_UPDATE", "false".into()),
        ("LOG_LEVEL", "INFO".into()),
        ("SURREAL_URL", format!("ws://127.0.0.1:{db_port}/rpc")),
    ]);
    let mut child = E2eChild::spawn(
        &exe,
        &[
            "run",
            "--mode",
            "stdio",
            "--runtime-mode",
            "plugin",
            "--scan",
            "false",
        ],
        cwd.path(),
        &env,
    );

    // --- MCP handshake over NDJSON ---
    let init = child.request(
        1,
        "initialize",
        initialize_params(),
        Duration::from_secs(30),
    );
    assert!(
        init.get("result").is_some() && init.get("error").is_none(),
        "initialize must succeed, got {init}"
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"], "Threski Studio",
        "{init}"
    );
    child.send(
        &json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string(),
    );

    // The plugin binding fires on `initialized`: the server asks us for the
    // workspace roots (3s timeout). Hold the answer until the pre-bind
    // assertions ran — they must fit inside the ListRoots window, like a
    // real Cursor session where the client answers immediately.
    let id = child
        .wait_for_server_request("roots/list", Duration::from_secs(10))
        .expect("roots/list request arrives after initialized");
    // --- handshake-only mode (before the workspace binds) ---
    let tools = child.request(2, "tools/list", json!({}), Duration::from_secs(10));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(
        names.len(),
        56,
        "handshake-only mode lists the 56-tool registry"
    );

    // Tool calls that need the workspace/DB must degrade exactly as
    // the do : a tool RESULT carrying the
    // `database not connected` error, never a protocol error.
    let degraded = child.request(
        3,
        "tools/call",
        json!({
            "name": "get_working_state",
            "arguments": {"project_name": "e2e-probe", "branch": "main"}
        }),
        Duration::from_secs(10),
    );
    assert!(
        degraded.get("result").is_some() && degraded.get("error").is_none(),
        "degraded read must be a tool result, not a protocol error: {degraded}"
    );
    assert_eq!(degraded["result"]["isError"], json!(true), "{degraded}");
    let text = degraded["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("get_working_state failed: database not connected"),
        "expected the nil-dbClient degrade text, got {text:?}"
    );

    // --- bind: answer roots with the opened folder ---
    let path = cwd.path().display().to_string().replace('\\', "/");
    child.respond(
        &id,
        json!({ "roots": [ { "uri": format!("file:///{path}"), "name": "e2e" } ] }),
    );

    // --- one memory tool after the bind: the degrade disappears once the
    // --- embedded DB the binding started is reachable.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut bound_call: Option<Value> = None;
    let mut call_id = 4;
    while Instant::now() < deadline {
        let resp = child.request(
            call_id,
            "tools/call",
            json!({
                "name": "get_working_state",
                "arguments": {"project_name": "e2e-probe", "branch": "main"}
            }),
            Duration::from_secs(15),
        );
        call_id += 1;
        assert!(
            resp.get("result").is_some() && resp.get("error").is_none(),
            "tools/call must return a result, not a protocol error: {resp}"
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if text.contains("database not connected") {
            // The binding's embedded-DB bootstrap is still running.
            std::thread::sleep(Duration::from_millis(700));
            continue;
        }
        let value: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("post-bind memory read must be JSON, got {text:?}: {e}"));
        assert!(
            value["found"].is_boolean(),
            "get_working_state JSON shape: {value}"
        );
        assert_eq!(
            value["found"],
            json!(false),
            "fresh isolated DB: no working state yet: {value}"
        );
        bound_call = Some(resp);
        break;
    }
    assert!(
        bound_call.is_some(),
        "the plugin binding never connected the embedded DB; plugin.log tail:\n{}",
        E2eChild::plugin_log_tail(data.path())
    );

    // --- clean termination on stdin close ( killed the process tree;
    // --- the Rust gate asserts the graceful exit instead).
    child.close_stdin();
    let status = child
        .wait_for_exit(Duration::from_secs(60))
        .unwrap_or_else(|| {
            child.kill_tree();
            panic!(
                "process must exit when stdin closes; plugin.log tail:\n{}",
                E2eChild::plugin_log_tail(data.path())
            )
        });
    assert!(
        status.success(),
        "clean exit expected, got {status:?}; plugin.log tail:\n{}",
        E2eChild::plugin_log_tail(data.path())
    );
    child.kill_tree();
}
