//! xtask — build/dist/test/lint driver replacing the `makehelper.
//!
//! Commands mirror the Makefile targets (D9): `build`, `build-mcp`, `dist`,
//! `clean`, `test`, `lint`. Run via `cargo xtask <command>` (see root
//! `[workspace.metadata.xtask]`-style aliases in `.cargo/config.toml`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

fn main() {
    if let Err(err) = run(std::env::args().nth(1)) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run(cmd: Option<String>) -> Result<()> {
    let Some(cmd) = cmd else {
        bail!("usage: cargo xtask <build|build-mcp|dist|clean|test|lint>");
    };
    let workspace_root = workspace_root()?;
    match cmd.as_str() {
        "build" => cargo(
            &workspace_root,
            &["build", "--release", "-p", "ibis-server"],
        ),
        "build-mcp" => cargo(&workspace_root, &["build", "--release", "-p", "mcp-bridge"]),
        "dist" => dist(&workspace_root),
        "clean" => clean(&workspace_root),
        "test" => cargo(&workspace_root, &["test", "--workspace"]),
        "lint" => cargo(
            &workspace_root,
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        other => bail!("unknown command: {other}"),
    }
}

fn workspace_root() -> Result<PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("CARGO_MANIFEST_DIR not set and no cwd fallback"))
        .or_else(|_| std::env::current_dir().context("current dir"))?;
    for dir in manifest.ancestors() {
        if dir.join("Cargo.toml").is_file() && dir.join("crates").is_dir() {
            return Ok(dir.to_path_buf());
        }
    }
    bail!("workspace root not found (no Cargo.toml with crates/ above the manifest)");
}

fn cargo(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .status()
        .context("spawning cargo")?;
    if !status.success() {
        bail!("cargo {args:?} failed with status {status}");
    }
    Ok(())
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn target_profile_exe(root: &Path, crate_bin: &str) -> PathBuf {
    root.join("target")
        .join("release")
        .join(exe_name(crate_bin))
}

fn dist(root: &Path) -> Result<()> {
    cargo(
        root,
        &[
            "build",
            "--release",
            "-p",
            "ibis-server",
            "-p",
            "mcp-bridge",
        ],
    )?;

    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir).context("creating dist/")?;

    fs::copy(
        target_profile_exe(root, "ibis"),
        dist_dir.join(exe_name("ibis")),
    )
    .context("copying ibis")?;
    fs::copy(
        target_profile_exe(root, "mcp-bridge"),
        dist_dir.join(exe_name("mcp-bridge")),
    )
    .context("copying mcp-bridge")?;

    if dist_dir.join("rules").exists() {
        fs::remove_dir_all(dist_dir.join("rules")).context("removing dist/rules")?;
    }
    copy_dir(&root.join("rules"), &dist_dir.join("rules")).context("copying rules/")?;
    fs::copy(root.join("sgconfig.yml"), dist_dir.join("sgconfig.yml"))
        .context("copying sgconfig.yml")?;
    fs::copy(
        root.join("config_master.json"),
        dist_dir.join("config.json"),
    )
    .context("copying config_master.json -> dist/config.json")?;

    Ok(())
}

fn clean(root: &Path) -> Result<()> {
    let dist_dir = root.join("dist");
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir).context("removing dist/")?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
