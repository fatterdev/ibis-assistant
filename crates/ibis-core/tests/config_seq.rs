//! Config load scenarios that mutate process-global state (env vars, cwd).
//! Rust tests run in parallel within a process, so these run sequentially in
//! one test function, mirroring the original per-test isolation.

use std::path::Path;

use ibis_core::config::{Config, DEFAULT_PLUGIN_DB_URL};

const ENV_VARS: &[&str] = &[
    "PORT",
    "KNOWLEDGE_MODE",
    "KNOWLEDGE_RPM",
    "MODE",
    "GEMINI_API_KEY",
    "SURREAL_URL",
    "SURREAL_NS",
    "SURREAL_DB",
    "SURREAL_USER",
    "SURREAL_PASS",
    "REDMINE_URL",
    "REDMINE_API_KEY",
    "DISCOVERY_ROOT",
    "LOGS_ROOT",
    "AUTO_SCAN",
    "DB_AUTO_UPDATE",
    "RUNTIME_MODE",
    "BIND_ADDRESS",
    "IBIS_DATA_DIR",
    "IBIS_WORKSPACE",
    "GIT_TOKEN",
    "LOG_FILE",
    "LOG_LEVEL",
    "EMBEDDING_PROVIDER",
    "EMBEDDING_MODEL",
    "EMBEDDING_URL",
    "REASONING_PROVIDER",
    "REASONING_MODEL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "REDMINE_CONCURRENCY",
    "CODE_CONCURRENCY",
    "DB_TIMEOUT",
];

fn clean_env() {
    for v in ENV_VARS {
        std::env::remove_var(v);
    }
}

type CwdGuard = PathBufRestore;
fn chdir(path: &Path) -> CwdGuard {
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(path).unwrap();
    PathBufRestore(Some(original))
}
struct PathBufRestore(Option<std::path::PathBuf>);
impl Drop for PathBufRestore {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            std::env::set_current_dir(p).unwrap();
        }
    }
}

#[test]
fn config_load_scenarios() {
    // NOTE: this must stay a single #[test] — env vars and cwd are process-global.
    // ── TestLoadDefaults ─────────────────────────────────────────────────
    clean_env();
    let cfg = Config::load();
    assert_eq!(cfg.port, 3030);
    assert_eq!(cfg.mode, "sse");
    assert_eq!(cfg.listen_addr(), ":3030");
    assert_eq!(cfg.gemini_default_rpm, 100);
    assert!(!cfg.memory.projection_enabled);
    assert_eq!(cfg.memory.git_store.personal_claims, "");

    // ── TestLoadEnvOverrides ─────────────────────────────────────────────
    clean_env();
    std::env::set_var("PORT", "4040");
    std::env::set_var("KNOWLEDGE_MODE", "stdio");
    std::env::set_var("KNOWLEDGE_RPM", "120");
    std::env::set_var("GEMINI_API_KEY", "key1,   key2");
    std::env::set_var("DB_AUTO_UPDATE", "false");
    let cfg = Config::load();
    assert_eq!(cfg.port, 4040);
    assert_eq!(cfg.mode, "stdio");
    assert_eq!(cfg.gemini_default_rpm, 120);
    assert!(!cfg.db_auto_update);
    assert_eq!(cfg.gemini_keys.len(), 2);
    assert_eq!(cfg.gemini_keys[0].key, "key1");
    assert_eq!(cfg.gemini_keys[1].key, "key2");
    assert_eq!(cfg.gemini_keys[0].rpm, 120);
    assert_eq!(cfg.gemini_keys[0].owner, "");
    assert_eq!(cfg.ai.reasoning.clouds.gemini.keys.len(), 2);
    clean_env();

    // ── TestLoadFileOverrides ────────────────────────────────────────────
    clean_env();
    let tmp = tempfile::tempdir().unwrap();
    let _guard = chdir(tmp.path());
    std::fs::write(
        tmp.path().join("config.json"),
        r#"{"port": 5050, "mode": "http"}"#,
    )
    .unwrap();
    let cfg = Config::load();
    assert_eq!(cfg.port, 5050);
    assert_eq!(
        cfg.gemini_default_rpm, 100,
        "defaults persist if not in file"
    );
    assert!(cfg.config_loaded);
    drop(_guard);
    clean_env();

    // ── TestEnvOverridesFile ─────────────────────────────────────────────
    clean_env();
    let tmp = tempfile::tempdir().unwrap();
    let _guard = chdir(tmp.path());
    std::fs::write(tmp.path().join("config.json"), r#"{"port": 5050}"#).unwrap();
    std::env::set_var("PORT", "6060");
    let cfg = Config::load();
    assert_eq!(cfg.port, 6060);
    drop(_guard);
    clean_env();

    // ── TestConfigLoadResolution ─────────────────────────────────────────
    clean_env();
    let cfg = Config::load();
    assert!(
        Path::new(&cfg.db_data_path).is_absolute(),
        "DBDataPath={}",
        cfg.db_data_path
    );
    assert!(Path::new(&cfg.discovery_root).is_absolute());
    assert!(Path::new(&cfg.logs_root).is_absolute());

    // ── TestPluginIsolationUsesDataDir ───────────────────────────────────
    clean_env();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("RUNTIME_MODE", "plugin");
    std::env::set_var("IBIS_DATA_DIR", dir.path().display().to_string());
    let cfg = Config::load();
    assert!(cfg.is_plugin());
    assert!(
        cfg.db_data_path.starts_with(dir.path().to_str().unwrap()),
        "DBDataPath={}",
        cfg.db_data_path
    );
    assert!(cfg.logs_root.starts_with(dir.path().to_str().unwrap()));
    assert!(cfg.log_file.starts_with(dir.path().to_str().unwrap()));
    assert_eq!(cfg.db_url, DEFAULT_PLUGIN_DB_URL);
    clean_env();

    // ── TestPluginLoadIgnoresCWDConfig ───────────────────────────────────
    clean_env();
    let cwd = tempfile::tempdir().unwrap();
    let personal = cwd.path().join("personal-db");
    let payload = format!(
        r#"{{"db_data_path": "{}", "runtime_mode": "personal"}}"#,
        personal.display().to_string().replace('\\', "\\\\")
    );
    std::fs::write(cwd.path().join("config.json"), payload).unwrap();
    let data = tempfile::tempdir().unwrap();
    let _guard = chdir(cwd.path());
    std::env::set_var("RUNTIME_MODE", "plugin");
    std::env::set_var("IBIS_DATA_DIR", data.path().display().to_string());
    let cfg = Config::load();
    assert!(
        !cfg.db_data_path.contains("personal-db"),
        "plugin loaded personal config: {}",
        cfg.db_data_path
    );
    assert!(cfg.db_data_path.starts_with(data.path().to_str().unwrap()));
    drop(_guard);
    clean_env();

    // ── env precedence details (merged: same process-global state) ──
    {
        clean_env();
        std::env::set_var("KNOWLEDGE_MODE", "http");
        std::env::set_var("MODE", "stdio");
        let cfg = Config::load();
        assert_eq!(
            cfg.mode, "stdio",
            "MODE wins over KNOWLEDGE_MODE when both set"
        );
        clean_env();

        std::env::set_var("AUTO_SCAN", "true");
        let cfg = Config::load();
        assert!(cfg.auto_scan, "AUTO_SCAN=true forces scan on");
        std::env::set_var("AUTO_SCAN", "false");
        let cfg = Config::load();
        assert!(cfg.auto_scan, "AUTO_SCAN=false never unsets (parity)");
        clean_env();

        std::env::set_var("DB_AUTO_UPDATE", "false");
        let cfg = Config::load();
        assert!(!cfg.db_auto_update, "DB_AUTO_UPDATE=false unsets");
        clean_env();

        std::env::set_var("GEMINI_API_KEY", "a, b");
        let cfg = Config::load();
        assert_eq!(
            cfg.gemini_keys.len(),
            2,
            "empty parts dropped, whitespace trimmed"
        );
        clean_env();

        // GEMINI_API_KEY must not override existing clouds credentials.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = chdir(tmp.path());
        std::env::set_var("GEMINI_API_KEY", "envkey");
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"ai": {"reasoning": {"clouds": {"gemini": {"keys": [{"key": "filekey"}]}}}}}"#,
        )
        .unwrap();
        let cfg = Config::load();
        assert_eq!(
            cfg.ai.reasoning.clouds.gemini.keys[0].key, "filekey",
            "file keys win over env"
        );
        drop(_guard);
        clean_env();
    }
}
