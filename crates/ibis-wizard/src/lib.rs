//! ibis-wizard — the configuration wizard.
//!
//! Library crate: the binary dispatch lives in wave-4 `ibis-server`
//! (`ibis config [fast|full|show|--recommend|ai]`,
//! ); F-011 (service elevation) is ibis-server
//! scope, not this crate's.
//!
//! The fast flow prompts runtime mode/folder, port, AI mode, cloud keys,
//! local model, CPU-only cloud routing and the personal-claims policy, then
//! probes the hardware ([`ibis_settings::probe_blocking`]), prints the probe
//! summary and applies the choices through [`ibis_settings::apply`]. The full
//! flow adds database, embeddings, cloud-advanced, SMTP and ticketing
//! sections.
//!
//! Deliberate deltas (D15, rows K-11..K-13):
//! - `run*` returns an explicit [`Outcome`] instead of the `os.Exit(0)`
//!   library side effect;
//! - the ticketing-config write propagates errors instead of swallowing them;
//! - `prompt_bool` drops the Italian `s`/`si` aliases (English-only repo).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use clap::Subcommand;
use ibis_core::config::{default_config, Config, ProjectConfig};
use ibis_settings as settings;

/// Wizard mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Full,
}

/// What the binary should do after the wizard ( relied on `os.Exit(0)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// User answered yes to "Start Ibis Assistant now?" — the caller runs the
    /// server ( rewrote argv to `run`).
    StartServer,
    /// Wizard finished without starting ( printed the run hint + exited 0).
    Done,
}

/// Wizard failures.
#[derive(Debug, thiserror::Error)]
pub enum WizardError {
    #[error("wizard interrupted: {0}")]
    Interrupted(#[source] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl WizardError {
    fn io(err: std::io::Error) -> Self {
        WizardError::Interrupted(err)
    }
}

/// The `ibis config …` subcommands ( dispatch).
#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
    /// Interactive fast configuration wizard (default).
    #[command(name = "fast")]
    Fast,
    /// Interactive full configuration wizard.
    #[command(name = "full")]
    Full,
    /// Print a masked non-interactive AI configuration summary.
    #[command(name = "show", alias = "--show")]
    Show,
    /// Print hardware probe + recommendations as JSON.
    #[command(name = "recommend", alias = "--recommend")]
    Recommend,
    /// Alias of the fast wizard.
    #[command(name = "ai", alias = "--ai")]
    Ai,
}

/// Interactive input source. Injected so the
/// prompt flows are unit-testable.
pub trait PromptSource {
    /// Next line or `None` on EOF.
    fn read_line(&mut self) -> Option<String>;
}

/// Real stdin prompt source (production).
pub struct StdinPrompt;

impl PromptSource for StdinPrompt {
    fn read_line(&mut self) -> Option<String> {
        let mut buf = String::new();
        match std::io::stdin().read_line(&mut buf) {
            Ok(0) => None,
            Ok(_) => Some(buf),
            Err(_) => None,
        }
    }
}

/// Scripted prompt source for tests.
pub struct CursorPrompt {
    lines: VecDeque<String>,
}

impl CursorPrompt {
    pub fn new(lines: &[&str]) -> Self {
        CursorPrompt {
            lines: lines.iter().map(|l| l.to_string()).collect(),
        }
    }
}

impl PromptSource for CursorPrompt {
    fn read_line(&mut self) -> Option<String> {
        self.lines.pop_front()
    }
}

/// Runs the fast wizard.
pub fn run() -> Result<Outcome, WizardError> {
    run_mode(Mode::Fast)
}

/// Runs fast or full configuration wizard.
pub fn run_mode(mode: Mode) -> Result<Outcome, WizardError> {
    let cfg = try_load_existing().unwrap_or_else(default_config);
    let mut prompt = StdinPrompt;
    run_mode_with_config(mode, cfg, &mut prompt)
}

/// [`run_mode`] with an injected prompt source (test seam).
pub fn run_mode_with(mode: Mode, prompt: &mut dyn PromptSource) -> Result<Outcome, WizardError> {
    let cfg = try_load_existing().unwrap_or_else(default_config);
    run_mode_with_config(mode, cfg, prompt)
}

/// The full wizard flow over a supplied starting config ( body of
/// `RunMode` after `tryLoadExisting`).
pub fn run_mode_with_config(
    mode: Mode,
    mut cfg: Config,
    prompt: &mut dyn PromptSource,
) -> Result<Outcome, WizardError> {
    println!("\n=== Ibis Assistant — Configuration Wizard ===");
    println!("Leave a field empty to keep the recommended or current value.");

    let result = if mode == Mode::Full {
        run_full(&mut cfg, prompt)
    } else {
        run_fast(&mut cfg, prompt)
    };
    result.map_err(WizardError::Other)?;

    let path = if cfg.config_path.is_empty() {
        "config.json".to_string()
    } else {
        cfg.config_path.clone()
    };
    let data = serde_json::to_string_pretty(&cfg).map_err(|e| WizardError::Other(e.to_string()))?;
    std::fs::write(&path, data).map_err(WizardError::io)?;
    println!("\n[OK] Saved {path}");

    print!("\nStart Ibis Assistant now? (Y/N) [Y]: ");
    let _ = std::io::stdout().flush();
    let start = prompt
        .read_line()
        .unwrap_or_default()
        .to_lowercase()
        .trim()
        .to_string();
    if start.is_empty() || start == "y" || start == "yes" {
        println!("Starting server...");
        return Ok(Outcome::StartServer);
    }
    println!("Done. Run: ibis run");
    Ok(Outcome::Done)
}

/// Loads config.json from the CWD when present (`tryLoadExisting,
/// ). Invalid JSON is ignored (defaults win, parity).
fn try_load_existing() -> Option<Config> {
    let contents = std::fs::read_to_string("config.json").ok()?;
    let mut cfg: Config = match serde_json::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(_) => return None,
    };
    cfg.config_loaded = true;
    cfg.config_path = std::fs::canonicalize("config.json")
        .map(|c| c.display().to_string())
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|d| d.join("config.json").display().to_string())
                .unwrap_or_else(|_| "config.json".to_string())
        });
    ibis_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, cfg.gemini_default_rpm);
    Some(cfg)
}

/// `filepath.Base — unlike `settings::base_name`, trailing
/// separators are dropped.
fn filepath_base(p: &str) -> String {
    let norm = p.replace('\\', "/");
    let trimmed = norm.trim_end_matches('/');
    if trimmed.is_empty() {
        if norm.is_empty() {
            return ".".to_string();
        }
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// The fast setup flow (`runFast). Mutates `cfg` in
/// place; error strings mirror the `fmt.Errorf` returns.
fn run_fast(cfg: &mut Config, prompt: &mut dyn PromptSource) -> Result<(), String> {
    println!("\n--- Fast setup ---");
    prompt_runtime_and_folder(cfg, prompt);
    cfg.port = prompt_int("Server port", cfg.port, prompt);

    let ollama_url = if cfg.ai.embedding.url.trim().is_empty() {
        "http://127.0.0.1:11434".to_string()
    } else {
        cfg.ai.embedding.url.clone()
    };
    let probe = settings::probe_blocking(&ollama_url);
    let rec = settings::recommend(cfg, &probe);
    println!(
        "\nHardware probe:\n  GPU: {} ({}) VRAM≈{} MiB\n  Ollama reachable: {} library={}\n  Tier: {} → {}\n  {}\n",
        probe.gpu_name,
        probe.gpu_vendor,
        probe.vram_mib,
        probe.ollama_reachable,
        probe.ollama_library,
        probe.tier,
        settings::resolve_local_model(&cfg.ai.reasoning, &probe),
        rec.summary
    );
    if !probe.diagnostic.is_empty() {
        println!(
            "  Note: {} (install/fix ROCm if you expect AMD GPU acceleration)",
            probe.diagnostic
        );
    }

    print!("\nAI mode:\n  1) Hybrid (recommended)\n  2) Local only (Ollama)\n  3) Cloud only (pick keys next)\n  4) Context only (retrieve; IDE agent answers)\nChoice (1-4) [1]: ");
    let _ = std::io::stdout().flush();
    let ai_choice = prompt.read_line().unwrap_or_default().trim().to_string();
    let ai_choice = if ai_choice.is_empty() {
        "1".to_string()
    } else {
        ai_choice
    };
    let mut choices = settings::UserChoices::default();
    match ai_choice.as_str() {
        "2" => choices.ai_mode = "ollama".to_string(),
        "3" => choices.ai_mode = "gemini".to_string(),
        "4" => choices.ai_mode = "none".to_string(),
        _ => choices.ai_mode = "hybrid".to_string(),
    }

    if choices.ai_mode != "none" && choices.ai_mode != "ollama" {
        println!("\nCloud API keys (paste into config — skip any you do not use):");
        choices.gemini_key = prompt_string("Gemini API key", "", prompt);
        choices.openai_key = prompt_string("OpenAI-compatible API key", "", prompt);
        if !choices.openai_key.is_empty() {
            choices.openai_base_url = prompt_string(
                "OpenAI-compatible base URL",
                "https://api.openai.com/v1",
                prompt,
            );
        }
        choices.claude_key = prompt_string("Claude (Anthropic) API key", "", prompt);
    }

    if choices.ai_mode == "gemini" {
        choices.ai_mode = if !choices.gemini_key.is_empty() {
            "gemini".to_string()
        } else if !choices.openai_key.is_empty() {
            "openai_compat".to_string()
        } else if !choices.claude_key.is_empty() {
            "claude".to_string()
        } else {
            "hybrid".to_string()
        };
    }

    print!("\nLocal model:\n  1) Automatic for this PC (recommended)\n  2) Always smallest (granite4.1:3b)\n  3) Pick tag\nChoice (1-3) [1]: ");
    let _ = std::io::stdout().flush();
    let lm = prompt.read_line().unwrap_or_default().trim().to_string();
    let lm = if lm.is_empty() { "1".to_string() } else { lm };
    match lm.as_str() {
        "2" => choices.always_smallest_local = true,
        "3" => {
            choices.local_model = prompt_string("Ollama model tag", "", prompt);
        }
        _ => choices.local_model = "auto".to_string(),
    }

    let use_cloud = prompt_bool(
        "On CPU-only PCs, use the cloud for hard questions",
        true,
        prompt,
    );
    choices.use_cloud_when_no_gpu = Some(use_cloud);

    print!("\nPersonal claims in the git claim store (.ibis/claims/):\n  1) Auto (recommended: db_only on server, stub in personal mode)\n  2) db_only — personal claims never touch the repo\n  3) stub — commit title/kind only, body stays in the DB\n  4) in_repo — full personal claims in the repo (repo access becomes the visibility boundary; leak risk with remotes)\nChoice (1-4) [1]: ");
    let _ = std::io::stdout().flush();
    match prompt.read_line().unwrap_or_default().trim() {
        "2" => choices.memory_git_store_personal_claims = "db_only".to_string(),
        "3" => choices.memory_git_store_personal_claims = "stub".to_string(),
        "4" => choices.memory_git_store_personal_claims = "in_repo".to_string(),
        _ => {}
    }

    cfg.discovery_root = prompt_string("Git discovery root", &cfg.discovery_root, prompt);
    cfg.logs_root = prompt_string("Logs root", &cfg.logs_root, prompt);
    choices.discovery_root = cfg.discovery_root.clone();
    choices.logs_root = cfg.logs_root.clone();
    choices.port = cfg.port;
    choices.runtime_mode = cfg.runtime_mode.clone();
    if let Some(project) = cfg.projects.first() {
        choices.working_folder = project.working_repo_path.clone();
    }

    let (out, _) = settings::apply(cfg, &choices);
    *cfg = out;
    let _ = std::fs::create_dir_all(&cfg.discovery_root);
    let _ = std::fs::create_dir_all(&cfg.logs_root);
    Ok(())
}

/// The full setup additions (`runFull).
fn run_full(cfg: &mut Config, prompt: &mut dyn PromptSource) -> Result<(), String> {
    run_fast(cfg, prompt)?;
    println!("\n--- Full setup (additional) ---");
    println!("[Database]");
    print!("  1) Embedded SurrealDB (recommended)\n  2) Remote SurrealDB\nChoice (1/2) [1]: ");
    let _ = std::io::stdout().flush();
    let dbc = prompt.read_line().unwrap_or_default().trim().to_string();
    if dbc == "2" {
        cfg.db_url = prompt_string("DB URL", &cfg.db_url, prompt);
        cfg.db_namespace = prompt_string("Namespace", &cfg.db_namespace, prompt);
        cfg.db_database = prompt_string("Database", &cfg.db_database, prompt);
        cfg.db_user = prompt_string("User", &cfg.db_user, prompt);
        cfg.db_password = prompt_string("Password", &cfg.db_password, prompt);
    } else {
        cfg.db_data_path = prompt_string("DB data path", &cfg.db_data_path, prompt);
        cfg.db_auto_update = prompt_bool("Auto-update SurrealDB", cfg.db_auto_update, prompt);
    }

    println!("\n[Embeddings]");
    cfg.ai.embedding.provider = prompt_string(
        "Embedding provider (ollama/gemini)",
        &cfg.ai.embedding.provider,
        prompt,
    );
    if cfg.ai.embedding.provider == "ollama" {
        cfg.ai.embedding.url = prompt_string("Ollama URL", &cfg.ai.embedding.url, prompt);
        cfg.ai.embedding.model = prompt_string("Embedding model", &cfg.ai.embedding.model, prompt);
        cfg.ai.embedding.auto_start =
            prompt_bool("Auto-start Ollama", cfg.ai.embedding.auto_start, prompt);
        cfg.ai.embedding.auto_update =
            prompt_bool("Auto-pull models", cfg.ai.embedding.auto_update, prompt);
    }

    println!("\n[Cloud advanced]");
    if !cfg.ai.reasoning.clouds.openai_compat.api_key.is_empty()
        || prompt_bool(
            "Configure OpenAI-compatible model/URL further",
            false,
            prompt,
        )
    {
        cfg.ai.reasoning.clouds.openai_compat.base_url = prompt_string(
            "OpenAI-compat base URL",
            &cfg.ai.reasoning.clouds.openai_compat.base_url,
            prompt,
        );
        cfg.ai.reasoning.clouds.openai_compat.model = prompt_string(
            "OpenAI-compat model",
            &cfg.ai.reasoning.clouds.openai_compat.model,
            prompt,
        );
    }
    if !cfg.ai.reasoning.clouds.claude.api_key.is_empty() {
        cfg.ai.reasoning.clouds.claude.model = prompt_string(
            "Claude model",
            &cfg.ai.reasoning.clouds.claude.model,
            prompt,
        );
    }
    cfg.ai.reasoning.routing.local_timeout_ms = prompt_int(
        "Local LLM timeout (ms)",
        cfg.ai.reasoning.routing.local_timeout_ms,
        prompt,
    );

    println!("\n[SMTP]");
    cfg.smtp.enabled = prompt_bool("Enable SMTP alerts", cfg.smtp.enabled, prompt);
    if cfg.smtp.enabled {
        cfg.smtp.host = prompt_string("SMTP host", &cfg.smtp.host, prompt);
        cfg.smtp.port = prompt_int("SMTP port", cfg.smtp.port, prompt);
        cfg.smtp.user = prompt_string("SMTP user", &cfg.smtp.user, prompt);
        cfg.smtp.password = prompt_string("SMTP password", &cfg.smtp.password, prompt);
        cfg.smtp.from = prompt_string("From", &cfg.smtp.from, prompt);
        cfg.smtp.to = prompt_string("To", &cfg.smtp.to, prompt);
    }

    println!("\n[Ticketing]");
    if prompt_bool("Configure Redmine", false, prompt) {
        let redmine_url = prompt_string("Redmine URL", "http://redmine.local", prompt);
        let redmine_key = prompt_string("Redmine API key", "", prompt);
        cfg.redmine_url = redmine_url.clone();
        cfg.redmine_key = redmine_key.clone();
        // Same shape the original wizard wrote ; the porting
        // pack carries ticketing_config.example.json with this layout.
        let ticketing_config = serde_json::json!({
            "default_provider": "redmine",
            "project_provider_map": { "*": "redmine" },
            "providers": {
                "redmine": { "base_url": redmine_url, "api_key": redmine_key }
            },
            "pr": { "default_target_branch": "master" }
        });
        std::fs::create_dir_all("config").map_err(|e| e.to_string())?;
        let data = serde_json::to_string_pretty(&ticketing_config).map_err(|e| e.to_string())?;
        std::fs::write(Path::new("config").join("ticketing_config.json"), data)
            .map_err(|e| e.to_string())?;
    }

    ibis_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, cfg.gemini_default_rpm);
    Ok(())
}

/// Prints a masked non-interactive AI config summary (`ShowAISummary,
/// ).
pub fn show_ai_summary(cfg: &Config) {
    print!("{}", ai_summary_string(cfg));
}

/// [`show_ai_summary`] as a string (test seam; the original function only printed).
pub fn ai_summary_string(cfg: &Config) -> String {
    let mut out = String::new();
    let mut cfg = cfg.clone();
    ibis_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, cfg.gemini_default_rpm);
    let probe = settings::probe_blocking(&cfg.ai.embedding.url);
    let resolved = settings::resolve_local_model(&cfg.ai.reasoning, &probe);
    let _ = writeln!(out, "runtime_mode={} port={}", cfg.runtime_mode, cfg.port);
    let _ = writeln!(
        out,
        "reasoning.provider={} model={} resolved_local={}",
        cfg.ai.reasoning.provider, cfg.ai.reasoning.model, resolved
    );
    let _ = writeln!(
        out,
        "probe tier={} vram_mib={} cpu_only={}",
        probe.tier, probe.vram_mib, probe.cpu_only
    );
    let _ = writeln!(
        out,
        "cloud_primary={} use_cloud_when_no_gpu={}",
        cfg.ai.reasoning.resolve_cloud_primary(),
        cfg.ai.reasoning.routing.use_cloud_when_no_gpu
    );
    let _ = writeln!(
        out,
        "gemini_keys={} openai={} claude={}",
        cfg.ai.reasoning.gemini_keys_effective().len(),
        settings::mask_secret(&cfg.ai.reasoning.clouds.openai_compat.api_key),
        settings::mask_secret(&cfg.ai.reasoning.clouds.claude.api_key)
    );
    if cfg.port > 0 {
        let _ = writeln!(out, "settings_ui={}/settings", cfg.public_base_url());
    }
    out
}

/// Prints the recommend output as JSON to stdout (
/// `PrintRecommendationsJSON`).
pub fn print_recommendations_json(cfg: &Config) -> Result<(), WizardError> {
    let cfg = cfg.clone();
    let probe = settings::probe_blocking(&cfg.ai.embedding.url);
    let rec = settings::recommend(&cfg, &probe);
    let data = serde_json::to_string_pretty(&rec).map_err(|e| WizardError::Other(e.to_string()))?;
    println!("{data}");
    Ok(())
}

/// Dispatches the `config` subcommand ( ; unknown
/// subcommands fell through to fast — handled by the wave-4 binary).
pub fn run_command(command: ConfigCommand) -> Result<Outcome, WizardError> {
    match command {
        ConfigCommand::Full => run_mode(Mode::Full),
        ConfigCommand::Show => {
            show_ai_summary(&ibis_core::config::new_default_config());
            Ok(Outcome::Done)
        }
        ConfigCommand::Recommend => {
            print_recommendations_json(&ibis_core::config::new_default_config())?;
            Ok(Outcome::Done)
        }
        ConfigCommand::Fast | ConfigCommand::Ai => run_mode(Mode::Fast),
    }
}

/// Runtime mode + working folder prompts (`promptRuntimeAndFolder,
/// ).
fn prompt_runtime_and_folder(cfg: &mut Config, prompt: &mut dyn PromptSource) {
    let mode = if cfg.runtime_mode.is_empty() {
        "personal".to_string()
    } else {
        cfg.runtime_mode.clone()
    };
    cfg.runtime_mode = prompt_string("Runtime mode (personal/server/plugin)", &mode, prompt);
    if !ibis_core::config::is_live_runtime(&cfg.runtime_mode) {
        return;
    }
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let folder = prompt_string("Local working folder", &cwd, prompt);
    if folder.trim().is_empty() {
        return;
    }
    cfg.projects = vec![ProjectConfig {
        name: filepath_base(&folder),
        working_repo_path: folder,
        ..Default::default()
    }];
}

/// `promptString
fn prompt_string(prompt: &str, default_value: &str, source: &mut dyn PromptSource) -> String {
    print!("{prompt} [{default_value}]: ");
    let _ = std::io::stdout().flush();
    let val = source.read_line().unwrap_or_default().trim().to_string();
    if val.is_empty() {
        return default_value.to_string();
    }
    val
}

/// `promptInt
fn prompt_int(prompt: &str, default_value: i64, source: &mut dyn PromptSource) -> i64 {
    print!("{prompt} [{default_value}]: ");
    let _ = std::io::stdout().flush();
    let val = source.read_line().unwrap_or_default().trim().to_string();
    if val.is_empty() {
        return default_value;
    }
    match val.parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
            println!("Invalid number, using default {default_value}");
            default_value
        }
    }
}

/// `promptBool minus the Italian `s`/`si` aliases
/// (K-12); any non-affirmative answer is false.
fn prompt_bool(prompt: &str, default_value: bool, source: &mut dyn PromptSource) -> bool {
    let def_str = if default_value { "Y" } else { "N" };
    print!("{prompt} (Y/N) [{def_str}]: ");
    let _ = std::io::stdout().flush();
    let val = source
        .read_line()
        .unwrap_or_default()
        .to_lowercase()
        .trim()
        .to_string();
    if val.is_empty() {
        return default_value;
    }
    val == "y" || val == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wizard flows create CWD-relative artifacts (`logs/` mkdir, the
    /// ticketing config write) — every flow test runs inside a temp CWD,
    /// serialized by this lock (lane-A convention for env/cwd globals).
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CwdGuard {
        saved: std::path::PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CwdGuard {
        fn new(dir: &Path) -> Self {
            let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            CwdGuard { saved, _lock: lock }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.saved);
        }
    }

    fn base_cfg(path: &str) -> Config {
        let mut cfg = default_config();
        cfg.config_path = path.to_string();
        cfg
    }

    /// The fast flow: ollama local-only, smallest model, no cloud on CPU,
    /// stub personal claims — then declines to start the server.
    #[test]
    fn fast_flow_applies_choices() {
        let dir = tempfile::TempDir::new().unwrap();
        let _cwd = CwdGuard::new(dir.path());
        let path = dir.path().join("config.json").display().to_string();
        let cfg = base_cfg(&path);
        let mut prompt = CursorPrompt::new(&[
            "personal", // runtime mode
            "",         // working folder (keep cwd)
            "",         // port (default)
            "2",        // AI mode: local only
            "2",        // local model: always smallest
            "n",        // use cloud when no GPU
            "3",        // personal claims: stub
            "",         // discovery root (default)
            "",         // logs root (default)
            "n",        // start now?
        ]);
        let outcome = run_mode_with_config(Mode::Fast, cfg, &mut prompt).unwrap();
        assert_eq!(outcome, Outcome::Done);

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["ai"]["reasoning"]["provider"], "ollama");
        assert_eq!(parsed["ai"]["reasoning"]["always_smallest_local"], true);
        assert_eq!(
            parsed["ai"]["reasoning"]["routing"]["use_cloud_when_no_gpu"],
            false
        );
        assert_eq!(parsed["memory"]["git_store"]["personal_claims"], "stub");
        assert_eq!(parsed["port"], 3030);
        assert!(parsed["ai"]["reasoning"]["context_only_fallback"]
            .as_bool()
            .unwrap());
    }

    /// Cloud-only with a Gemini key resolves through the fallback chain; the
    /// Claude-only variant maps to claude
    #[test]
    fn fast_flow_cloud_key_fallbacks() {
        let dir = tempfile::TempDir::new().unwrap();
        let _cwd = CwdGuard::new(dir.path());
        let path = dir.path().join("config.json").display().to_string();
        let cfg = base_cfg(&path);
        let mut prompt = CursorPrompt::new(&[
            "server",         // runtime mode (not live → no folder prompt)
            "",               // port
            "3",              // AI mode: cloud
            "",               // gemini key (skip)
            "",               // openai key (skip)
            "claude-key-xyz", // claude key
            "1",              // local model: auto
            "y",              // use cloud
            "1",              // claims: auto (unset)
            "",               // discovery
            "",               // logs
            "n",              // start now?
        ]);
        let outcome = run_mode_with_config(Mode::Fast, cfg, &mut prompt).unwrap();
        assert_eq!(outcome, Outcome::Done);
        let parsed: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.ai.reasoning.provider, "claude");
        assert_eq!(parsed.ai.reasoning.clouds.claude.api_key, "claude-key-xyz");
    }

    /// A Gemini key wins the cloud fallback chain
    #[test]
    fn fast_flow_gemini_key_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        let _cwd = CwdGuard::new(dir.path());
        let path = dir.path().join("config.json").display().to_string();
        let cfg = base_cfg(&path);
        let mut prompt = CursorPrompt::new(&[
            "plugin",  // live runtime → folder prompt follows
            "",        // working folder (keep cwd)
            "",        // port
            "3",       // cloud
            "gem-key", // gemini key
            "oa-key",  // openai key
            "",        // openai base URL
            "",        // claude key
            "1",       // local model auto
            "y",       // use cloud
            "1",       // claims
            "",        // discovery
            "",        // logs
            "y",       // start now
        ]);
        let outcome = run_mode_with_config(Mode::Fast, cfg, &mut prompt).unwrap();
        assert_eq!(outcome, Outcome::StartServer);
        let parsed: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.ai.reasoning.provider, "gemini");
        assert!(parsed.ai.reasoning.cloud_has_credentials("gemini"));
    }

    /// Working folder prompt seeds projects ( promptRuntimeAndFolder).
    #[test]
    fn fast_flow_working_folder_sets_project() {
        let dir = tempfile::TempDir::new().unwrap();
        let _cwd = CwdGuard::new(dir.path());
        let path = dir.path().join("config.json").display().to_string();
        let cfg = base_cfg(&path);
        let mut prompt = CursorPrompt::new(&[
            "personal",         // runtime
            "C:\\work\\myrepo", // working folder
            "",                 // port
            "2",                // local only
            "1",                // local model auto
            "y",                // use cloud
            "1",                // claims
            "",                 // discovery
            "",                 // logs
            "n",
        ]);
        run_mode_with_config(Mode::Fast, cfg, &mut prompt).unwrap();
        let parsed: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.projects.len(), 1);
        assert_eq!(parsed.projects[0].name, "myrepo");
        assert_eq!(parsed.projects[0].working_repo_path, "C:\\work\\myrepo");
    }

    /// The full flow writes the ticketing config and keeps DB/embedding/
    /// SMTP answers ( runFull).
    #[test]
    fn full_flow_database_smtp_ticketing() {
        let dir = tempfile::TempDir::new().unwrap();
        // The ticketing section writes into CWD/config — the guard points
        // the whole flow at the temp dir.
        let _cwd = CwdGuard::new(dir.path());
        let path = dir.path().join("config.json").display().to_string();
        let cfg = base_cfg(&path);
        let mut prompt = CursorPrompt::new(&[
            // fast section
            "personal",
            "",
            "",
            "2",
            "1",
            "y",
            "1",
            "",
            "",
            // full section
            "1",               // embedded DB
            "",                // DB data path
            "n",               // auto-update
            "ollama",          // embedding provider
            "",                // ollama URL
            "",                // embedding model
            "n",               // auto-start
            "n",               // auto-pull
            "n",               // configure openai further
            "60000",           // local timeout
            "n",               // SMTP
            "y",               // configure redmine
            "http://rm.local", // redmine URL
            "rm-key",          // redmine key
            "n",               // start now
        ]);
        let outcome = run_mode_with_config(Mode::Full, cfg, &mut prompt).unwrap();
        assert_eq!(outcome, Outcome::Done);

        let written =
            std::fs::read_to_string(dir.path().join("config").join("ticketing_config.json"))
                .unwrap();
        let ticketing: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(ticketing["default_provider"], "redmine");
        assert_eq!(ticketing["project_provider_map"]["*"], "redmine");
        assert_eq!(
            ticketing["providers"]["redmine"]["base_url"],
            "http://rm.local"
        );
        assert_eq!(ticketing["providers"]["redmine"]["api_key"], "rm-key");
        assert_eq!(ticketing["pr"]["default_target_branch"], "master");

        let parsed: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.ai.reasoning.routing.local_timeout_ms, 60000);
        assert!(!parsed.db_auto_update);
        assert!(!parsed.ai.embedding.auto_start);
        assert!(!parsed.smtp.enabled);
    }

    /// Invalid numbers fall back to the default ( promptInt).
    #[test]
    fn prompt_int_invalid_uses_default() {
        let mut prompt = CursorPrompt::new(&["abc", "", "42"]);
        assert_eq!(prompt_int("Port", 3030, &mut prompt), 3030);
        assert_eq!(prompt_int("Port", 3030, &mut prompt), 3030);
        assert_eq!(prompt_int("Port", 3030, &mut prompt), 42);
    }

    /// promptBool: y/yes true, anything else false, empty → default
    /// (`s`/`si` Italian aliases dropped — K-12).
    #[test]
    fn prompt_bool_matrix() {
        let mut prompt = CursorPrompt::new(&["y", "yes", "n", "maybe", "", "s"]);
        assert!(prompt_bool("q", false, &mut prompt));
        assert!(prompt_bool("q", false, &mut prompt));
        assert!(!prompt_bool("q", true, &mut prompt));
        assert!(!prompt_bool("q", true, &mut prompt));
        assert!(prompt_bool("q", true, &mut prompt), "empty keeps default");
        assert!(!prompt_bool("q", true, &mut prompt), "s is not yes anymore");
    }

    /// promptString returns the default on empty input.
    #[test]
    fn prompt_string_default() {
        let mut prompt = CursorPrompt::new(&["", "typed"]);
        assert_eq!(prompt_string("P", "def", &mut prompt), "def");
        assert_eq!(prompt_string("P", "def", &mut prompt), "typed");
    }

    /// The AI summary reports provider, resolved local model, probe tier and
    /// masked keys ( ShowAISummary).
    #[test]
    fn ai_summary_contains_masked_fields() {
        let mut cfg = default_config();
        cfg.runtime_mode = "personal".into();
        cfg.port = 3030;
        cfg.ai.reasoning.clouds.gemini.keys = vec![ibis_core::config::GeminiKeyConfig {
            key: "gemini-secret-key".into(),
            ..Default::default()
        }];
        cfg.ai.reasoning.clouds.claude.api_key = "sk-ant-987654".into();
        let summary = ai_summary_string(&cfg);
        assert!(
            summary.contains("runtime_mode=personal port=3030"),
            "{summary}"
        );
        assert!(
            summary.contains("reasoning.provider=hybrid model=auto"),
            "{summary}"
        );
        assert!(summary.contains("probe tier="), "{summary}");
        assert!(summary.contains("gemini_keys=1 openai="), "{summary}");
        assert!(summary.contains("claude=…7654"), "{summary}");
        assert!(!summary.contains("gemini-secret-key"), "key must be masked");
        assert!(
            summary.contains("settings_ui=http://127.0.0.1:3030/settings"),
            "{summary}"
        );
    }

    /// Recommendations JSON round-trips into the settings type.
    #[test]
    fn recommendations_json_parses() {
        let mut cfg = default_config();
        cfg.ai.reasoning.clouds.gemini.keys = vec![ibis_core::config::GeminiKeyConfig {
            key: "abc".into(),
            ..Default::default()
        }];
        ibis_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, 100);
        let probe = settings::HardwareReport {
            tier: settings::Tier::M,
            vram_mib: 8 * 1024,
            ..Default::default()
        };
        let rec = settings::recommend(&cfg, &probe);
        let json = serde_json::to_string_pretty(&rec).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["probe"]["tier"], "M");
        assert!(parsed["fields"].as_array().unwrap().len() >= 3);
        assert!(parsed["summary"].as_str().unwrap().contains("Tier M"));
    }

    /// filepath.Base semantics vs settings::base_name.
    #[test]
    fn filepath_base_drops_trailing_separators() {
        assert_eq!(filepath_base("C:\\work\\repo\\"), "repo");
        assert_eq!(filepath_base("/home/user/repo"), "repo");
        assert_eq!(filepath_base(""), ".");
        assert_eq!(filepath_base("/"), "/");
    }

    /// Empty script (EOF everywhere) keeps the defaults and still writes the
    /// config ( scanner at EOF yields empty strings).
    #[test]
    fn eof_prompt_keeps_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let _cwd = CwdGuard::new(dir.path());
        let path = dir.path().join("config.json").display().to_string();
        let cfg = base_cfg(&path);
        let mut prompt = CursorPrompt::new(&[]);
        let outcome = run_mode_with_config(Mode::Fast, cfg, &mut prompt).unwrap();
        assert_eq!(
            outcome,
            Outcome::StartServer,
            "EOF start-answer behaves as default Y"
        );
        let parsed: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.ai.reasoning.provider, "hybrid");
    }
}
