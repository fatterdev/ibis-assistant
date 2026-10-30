//! Workspace scenarios that mutate process-global state (env vars, cwd),
//! run sequentially inside one test function.

use std::path::Path;

use ibis_core::config::{Config, ProjectConfig};
use ibis_core::workspace::{
    candidate_paths, ensure_opened_workspace, file_uri_to_path, owned_clone_path,
    plugin_has_bound_workspace, resolve, workspace_start, WorkspaceError,
};

fn clear_workspace_env() {
    for v in [
        "IBIS_WORKSPACE",
        "CURSOR_WORKSPACE_FOLDER",
        "CURSOR_WORKSPACE",
        "VSCODE_WORKSPACE_FOLDER",
    ] {
        std::env::remove_var(v);
    }
}

fn init_git_dir(dir: &Path) -> String {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    dir.display().to_string()
}

#[test]
fn workspace_scenarios() {
    // ── TestResolvePersonalWorkingRepoPath ───────────────────────────────
    clear_workspace_env();
    let tmp = tempfile::tempdir().unwrap();
    let dir = init_git_dir(&tmp.path().join("repoA"));
    let cfg = Config {
        runtime_mode: "personal".into(),
        discovery_root: tempfile::tempdir().unwrap().path().display().to_string(),
        projects: vec![ProjectConfig {
            name: "ibis".into(),
            working_repo_path: dir.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let r = resolve(&cfg, "ibis").unwrap();
    assert!(!r.owned);
    assert_eq!(r.path, dir);

    // ── TestResolvePersonalGitReposBasename ──────────────────────────────
    let cfg = Config {
        runtime_mode: "personal".into(),
        git_repos: vec![dir.clone()],
        ..Default::default()
    };
    let base = Path::new(&dir)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let r = resolve(&cfg, &base).unwrap();
    assert!(!r.owned);
    assert_eq!(r.path, dir);

    // ── TestResolvePersonalMissingPathError ──────────────────────────────
    let cfg = Config {
        runtime_mode: "personal".into(),
        projects: vec![ProjectConfig {
            name: "gone".into(),
            working_repo_path: tempfile::tempdir()
                .unwrap()
                .path()
                .join("does-not-exist")
                .display()
                .to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(matches!(
        resolve(&cfg, "gone"),
        Err(WorkspaceError::WorkingTreeMissing { .. })
    ));

    // ── TestResolvePersonalNoMatchError ──────────────────────────────────
    clear_workspace_env();
    let empty = tempfile::tempdir().unwrap();
    let _guard = chdir(empty.path());
    let cfg = Config {
        runtime_mode: "personal".into(),
        ..Default::default()
    };
    assert!(matches!(
        resolve(&cfg, "unknown-project"),
        Err(WorkspaceError::NoLiveWorkspace(_))
    ));
    drop(_guard);

    // ── TestResolvePluginWalksWorkspace ──────────────────────────────────
    clear_workspace_env();
    let root_tmp = tempfile::tempdir().unwrap();
    let root = init_git_dir(root_tmp.path());
    let nested = Path::new(&root).join("internal").join("pkg");
    std::fs::create_dir_all(&nested).unwrap();
    let cfg = Config {
        runtime_mode: "plugin".into(),
        workspace: nested.display().to_string(),
        ..Default::default()
    };
    let r = resolve(&cfg, "whatever").unwrap();
    assert!(!r.owned);
    assert_eq!(r.path, root);

    // ── TestCandidatePathsIncludesResolved ───────────────────────────────
    clear_workspace_env();
    let dir_tmp = tempfile::tempdir().unwrap();
    let dir = init_git_dir(&dir_tmp.path().join("repoC"));
    let disc_tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        runtime_mode: "personal".into(),
        discovery_root: disc_tmp.path().display().to_string(),
        projects: vec![ProjectConfig {
            name: "demo".into(),
            working_repo_path: dir.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let cands = candidate_paths(Some(&cfg), "demo");
    assert!(
        !cands.is_empty() && cands[0] == dir,
        "candidates={cands:?} want live path first"
    );

    // ── TestEnsureOpenedWorkspacePluginReplaces ──────────────────────────
    clear_workspace_env();
    let root_tmp = tempfile::tempdir().unwrap();
    let root = init_git_dir(root_tmp.path());
    let mut cfg = Config {
        runtime_mode: "plugin".into(),
        workspace: root.clone(),
        projects: vec![ProjectConfig {
            name: "other".into(),
            working_repo_path: "C:\\old".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    ensure_opened_workspace(&mut cfg).unwrap();
    assert_eq!(cfg.projects.len(), 1);
    assert_eq!(cfg.projects[0].working_repo_path, root);
    assert_eq!(
        cfg.projects[0].name,
        Path::new(&root).file_name().unwrap().to_str().unwrap()
    );

    // ── TestEnsureOpenedWorkspacePluginSkipsNonGitCwd ────────────────────
    clear_workspace_env();
    let empty = tempfile::tempdir().unwrap();
    let _guard = chdir(empty.path());
    let mut cfg = Config {
        runtime_mode: "plugin".into(),
        projects: vec![ProjectConfig {
            name: "stale".into(),
            working_repo_path: "C:\\old".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    ensure_opened_workspace(&mut cfg).unwrap();
    assert!(
        cfg.projects.is_empty(),
        "plugin must not bind a non-git cwd as a project: {:?}",
        cfg.projects
    );
    assert!(!plugin_has_bound_workspace(&cfg));
    drop(_guard);

    // ── TestWorkspaceStartIgnoresUnexpandedFolder ────────────────────────
    clear_workspace_env();
    std::env::set_var("IBIS_WORKSPACE", "${workspaceFolder}");
    std::env::remove_var("CURSOR_WORKSPACE_FOLDER");
    std::env::remove_var("CURSOR_WORKSPACE");
    std::env::remove_var("VSCODE_WORKSPACE_FOLDER");
    let cfg = Config {
        runtime_mode: "plugin".into(),
        ..Default::default()
    };
    assert_eq!(workspace_start(&cfg), None);
    std::env::remove_var("IBIS_WORKSPACE");

    // ── TestWorkspaceStartUsesCursorFolderEnv ────────────────────────────
    std::env::remove_var("IBIS_WORKSPACE");
    std::env::set_var("CURSOR_WORKSPACE_FOLDER", "C:\\work\\ibis");
    let cfg = Config {
        runtime_mode: "plugin".into(),
        ..Default::default()
    };
    assert_eq!(
        workspace_start(&cfg).as_deref(),
        Some("C:\\work\\ibis")
    );
    std::env::remove_var("CURSOR_WORKSPACE_FOLDER");

    // ── FileURIToPath (windows-specific expectations) ────────────────────
    if cfg!(windows) {
        assert_eq!(
            file_uri_to_path("file:///C:/work/ibis"),
            "C:\\work\\ibis"
        );
        assert_eq!(
            file_uri_to_path("c:\\work\\ibis").to_lowercase(),
            "c:\\work\\ibis"
        );
    } else {
        assert_eq!(
            file_uri_to_path("file:///tmp/ibis"),
            "/tmp/ibis"
        );
    }

    // ── OwnedClonePath default root ──────────────────────────────────────
    assert_eq!(
        owned_clone_path(Some(&Config::default()), "demo"),
        Path::new(".")
            .join("dynamic")
            .join("demo")
            .display()
            .to_string()
    );

    clear_workspace_env();
}

struct CwdGuard(Option<std::path::PathBuf>);
fn chdir(path: &Path) -> CwdGuard {
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(path).unwrap();
    CwdGuard(Some(original))
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            std::env::set_current_dir(p).unwrap();
        }
    }
}
