//! ast-grep (`sg`) binary installer and rule/asset sync
//!
//!
//! Behavior contract:
//! - binary next to the ibis executable; version probe `sg --version`
//!   (second field); upgrade check against the `ast-grep/ast-grep` GitHub
//!   releases API (30s API timeout, 5-minute download timeout, degrade to the
//!   local binary on any failure).
//! - asset naming: windows→`pc-windows-msvc`, linux→`unknown-linux-gnu`,
//!   darwin→`apple-darwin`; arch tokens are the Rust-native `x86_64` /
//!   `aarch64` ( mapped GOARCH `amd64`→`x86_64` — the inverse direction of
//!   lane B's LB-05, same class of fix).
//! - archive member `sg`/`sg.exe`/`ast-grep`/`ast-grep.exe`; chmod 0755 on
//!   Unix.
//! - [`ensure_ast_grep_rules`] syncs `sgconfig.yml` + `rules/` into the exe
//!   dir and the sg binary dir so chunking works when the process cwd is a
//!   harness directory.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::ast_chunker::find_sg_config_dir;
use crate::error::CodeIngestError;

const RELEASES_API: &str = "https://api.github.com/repos/ast-grep/ast-grep/releases/latest";
const API_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Represents a release on GitHub.
#[derive(Debug, Deserialize)]
pub struct GitHubRelease {
    #[serde(rename = "tag_name")]
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

/// Represents a file attached to a release.
#[derive(Debug, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub browser_download_url: String,
}

/// Path where the ast-grep binary is managed: beside the ibis executable,
/// falling back to a bare name resolved via the OS PATH.
pub fn ast_grep_bin_path() -> PathBuf {
    let bin_name = if cfg!(windows) { "sg.exe" } else { "sg" };
    match std::env::current_exe() {
        Ok(exe) => exe.parent().unwrap_or(Path::new(".")).join(bin_name),
        Err(_) => PathBuf::from(bin_name),
    }
}

/// Parses `sg --version` output (format `ast-grep 0.51.0` → `0.51.0`)
/// (`getLocalVersion field extraction).
pub fn parse_version_output(out: &str) -> Result<String, CodeIngestError> {
    let parts: Vec<&str> = out.split_whitespace().collect();
    if parts.len() >= 2 {
        return Ok(parts[1].to_string());
    }
    Err(CodeIngestError::other(format!(
        "could not parse version output: {out}"
    )))
}

/// Runs `<bin> --version` and extracts the version.
/// 30s probe timeout (the original had none — a hung binary would hang ingest forever;
/// mirrors lane B's surreal version probe).
pub async fn local_version(bin: &Path) -> Result<String, CodeIngestError> {
    // Bare names (no parent dir) go through the OS PATH lookup; real paths
    // must exist (F-A08).
    let is_bare = bin.parent().is_none_or(|p| p.as_os_str().is_empty());
    if !is_bare && !bin.is_file() {
        return Err(CodeIngestError::other(format!(
            "{} not found",
            bin.display()
        )));
    }
    let output = tokio::time::timeout(
        VERSION_PROBE_TIMEOUT,
        tokio::process::Command::new(bin).arg("--version").output(),
    )
    .await
    .map_err(|_| CodeIngestError::other("version probe timed out"))?
    .map_err(CodeIngestError::Io)?;
    if !output.status.success() {
        return Err(CodeIngestError::other(format!(
            "sg --version exited with {}",
            output.status
        )));
    }
    parse_version_output(&String::from_utf8_lossy(&output.stdout))
}

/// Fetches the latest ast-grep release from the GitHub API.
pub async fn latest_release() -> Result<GitHubRelease, CodeIngestError> {
    let client = reqwest::Client::builder().timeout(API_TIMEOUT).build()?;
    let mut request = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "ibis-assistant");
    if let Some(token) = github_api_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(CodeIngestError::other(format!(
            "github api returned status: {}",
            response.status().as_u16()
        )));
    }
    Ok(response.json::<GitHubRelease>().await?)
}

fn github_api_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(key) {
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// Maps the Rust OS token to the ast-grep asset naming convention
/// (`installAstGrep osMap).
pub fn asset_os_token() -> &'static str {
    if cfg!(windows) {
        "pc-windows-msvc"
    } else if cfg!(target_os = "linux") {
        "unknown-linux-gnu"
    } else if cfg!(target_os = "macos") {
        "apple-darwin"
    } else {
        "unknown"
    }
}

/// Rust arch tokens already match the ast-grep asset tokens
/// (`x86_64`/`aarch64`; mapped its GOARCH tokens to these, LB-05 inverse).
pub fn asset_arch_token() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

/// Selects the release asset matching OS/arch with a `.zip`/`.tar.gz`
/// suffix (`installAstGrep matching loop). Pure function, unit-tested.
pub fn match_asset<'a>(
    release: &'a GitHubRelease,
    os: &str,
    arch: &str,
) -> Option<&'a GitHubAsset> {
    for asset in &release.assets {
        let name = asset.name.to_lowercase();
        if name.contains(arch)
            && name.contains(os)
            && (name.ends_with(".zip") || name.ends_with(".tar.gz"))
        {
            return Some(asset);
        }
    }
    None
}

/// Checks that the ast-grep binary is present and up to date, downloading the
/// latest GitHub release when needed.
///
/// Any network/install failure degrades to the local binary when one exists.
/// Always best-effort-syncs the rule assets on the way out.
pub async fn ensure_ast_grep(auto_update: bool) -> Result<PathBuf, CodeIngestError> {
    let bin_path = ast_grep_bin_path();
    tracing::info!(
        "Checking ast-grep (sg) installation ({})...",
        bin_path.display()
    );

    let local = match local_version(&bin_path).await {
        Ok(v) => {
            tracing::info!("Found local ast-grep version: {v}");
            Some(v)
        }
        Err(err) => {
            tracing::info!("ast-grep not found or version check failed: {err}");
            None
        }
    };

    if !auto_update && local.is_some() {
        tracing::info!(
            "ast-grep auto-update is disabled, using local version: {}",
            local.unwrap_or_default()
        );
        let _ = ensure_ast_grep_rules();
        return Ok(bin_path);
    }
    if !auto_update {
        tracing::warn!("ast-grep auto-update is disabled but no local binary found. Force checking for download...");
    }

    let release = match latest_release().await {
        Ok(r) => r,
        Err(err) => {
            return match local {
                Some(_) => {
                    tracing::warn!("Failed to check for updates (using local version): {err}");
                    let _ = ensure_ast_grep_rules();
                    Ok(bin_path)
                }
                None => Err(CodeIngestError::other(format!(
                    "failed to get release info and no local binary found: {err}"
                ))),
            };
        }
    };

    let remote = release.tag_name.trim_start_matches('v').to_string();
    if local.as_deref() == Some(remote.as_str()) {
        tracing::info!("ast-grep is up to date ({}).", local.unwrap_or_default());
        let _ = ensure_ast_grep_rules();
        return Ok(bin_path);
    }

    tracing::info!(
        "Newer ast-grep version available: {remote} (Local: {}). Downloading...",
        local.as_deref().unwrap_or("<none>")
    );

    if let Err(err) = install_ast_grep(&release, &bin_path).await {
        return match local {
            Some(_) => {
                tracing::warn!("Failed to update ast-grep (using local version): {err}");
                let _ = ensure_ast_grep_rules();
                Ok(bin_path)
            }
            None => Err(CodeIngestError::other(format!(
                "failed to install ast-grep: {err}"
            ))),
        };
    }

    tracing::info!("ast-grep updated successfully to {remote}.");
    let _ = ensure_ast_grep_rules();
    Ok(bin_path)
}

/// Downloads and installs the matching release asset.
///
/// Downloads into a **unique temp dir** with cleanup on success/failure
/// (F-A06: wrote a fixed-name `download_<asset>` into the process CWD,
/// the race/debris bug lane B fixed for the surreal installer, LB-04).
pub async fn install_ast_grep(release: &GitHubRelease, dest: &Path) -> Result<(), CodeIngestError> {
    let os = asset_os_token();
    let arch = asset_arch_token();
    let asset = match_asset(release, os, arch).ok_or_else(|| {
        CodeIngestError::other(format!(
            "no compatible asset found for {arch}/{os} in release {}",
            release.tag_name
        ))
    })?;

    tracing::info!("Downloading {}...", asset.name);

    let tmp_dir = tempfile::Builder::new()
        .prefix("ibis-sg-download-")
        .tempdir()?;
    let tmp_file = tmp_dir.path().join(&asset.name);
    download_file(&asset.browser_download_url, &tmp_file).await?;

    let name = asset.name.to_lowercase();
    let outcome = if name.ends_with(".zip") {
        extract_zip(&tmp_file, dest)
    } else if name.ends_with(".tar.gz") {
        extract_tar_gz(&tmp_file, dest)
    } else {
        Err(CodeIngestError::other(format!(
            "unsupported asset type: {}",
            asset.name
        )))
    };
    let _ = std::fs::remove_file(&tmp_file); // best-effort cleanup
    outcome?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = dest;
    Ok(())
}

async fn download_file(url: &str, dest: &Path) -> Result<(), CodeIngestError> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut out = std::fs::File::create(dest)?;
    // Stream to disk to avoid buffering large archives in memory.
    while let Some(chunk) = response.chunk().await? {
        out.write_all(&chunk)?;
    }
    out.flush()?;
    Ok(())
}

/// Extracts the sg binary from a `.tar.gz` archive.
pub fn extract_tar_gz(archive: &Path, target: &Path) -> Result<(), CodeIngestError> {
    let file = std::fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name == "sg" || name == "sg.exe" || name == "ast-grep" || name == "ast-grep.exe" {
            let mut out = std::fs::File::create(target)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    Err(CodeIngestError::other("binary 'sg' not found in archive"))
}

/// Extracts the sg binary from a `.zip` archive.
pub fn extract_zip(archive: &Path, target: &Path) -> Result<(), CodeIngestError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| CodeIngestError::other(e.to_string()))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| CodeIngestError::other(e.to_string()))?;
        let name = entry
            .enclosed_name()
            .and_then(|n| n.file_name().map(|f| f.to_string_lossy().to_string()))
            .unwrap_or_default();
        if name == "sg" || name == "sg.exe" || name == "ast-grep" || name == "ast-grep.exe" {
            let mut out = std::fs::File::create(target)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    Err(CodeIngestError::other(
        "binary 'sg' not found in zip archive",
    ))
}

/// Copies `sgconfig.yml` + `rules/` next to the sg binary (and executable
/// dir) so chunking finds LogAlign extractors regardless of the process cwd
///
///
/// The source root is the discovered sgconfig root; when none is found, cwd
/// parents are searched for a `rules/go-logs.yml` marker. Errors during sync
/// are warned and swallowed (best-effort likewise); the call itself never
/// fails in practice.
pub fn ensure_ast_grep_rules() -> Result<(), CodeIngestError> {
    let mut src_root = find_sg_config_dir();
    if src_root.is_none() {
        // Last resort: look beside the running binary's parent (repo root)
        // via cwd parents for the rules marker.
        if let Ok(cwd) = std::env::current_dir() {
            let mut dir = cwd;
            for _ in 0..8 {
                if dir.join("rules").join("go-logs.yml").is_file() {
                    src_root = Some(dir.clone());
                    break;
                }
                match dir.parent() {
                    Some(parent) if parent != dir => dir = parent.to_path_buf(),
                    _ => break,
                }
            }
        }
    }
    let Some(src_root) = src_root else {
        tracing::debug!("EnsureAstGrepRules: no rules source found");
        return Ok(());
    };

    let mut targets: BTreeSet<PathBuf> = BTreeSet::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            targets.insert(dir.to_path_buf());
        }
    }
    let bin = ast_grep_bin_path();
    if let Some(dir) = bin.parent() {
        targets.insert(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
    }
    for dest in targets {
        if dest.as_os_str().is_empty() || dest == src_root {
            continue;
        }
        match sync_sg_assets(&src_root, &dest) {
            Ok(()) => tracing::info!(
                "EnsureAstGrepRules: synced rules/sgconfig → {}",
                dest.display()
            ),
            Err(err) => tracing::warn!(
                "EnsureAstGrepRules: sync to {} failed: {err}",
                dest.display()
            ),
        }
    }
    Ok(())
}

/// Syncs `sgconfig.yml` + `rules/` from `src_root` into `dest_dir`
///: copy the config, replace the rules directory.
pub fn sync_sg_assets(src_root: &Path, dest_dir: &Path) -> Result<(), CodeIngestError> {
    copy_file(
        &src_root.join("sgconfig.yml"),
        &dest_dir.join("sgconfig.yml"),
    )?;
    let src_rules = src_root.join("rules");
    let dst_rules = dest_dir.join("rules");
    match std::fs::remove_dir_all(&dst_rules) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    copy_dir(&src_rules, &dst_rules)
}

fn copy_file(src: &Path, dst: &Path) -> Result<(), CodeIngestError> {
    let data = std::fs::read(src)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dst, data)?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), CodeIngestError> {
    for entry in walkdir::WalkDir::new(src).sort_by_file_name() {
        let entry = entry.map_err(|e| CodeIngestError::Walk(e.to_string()))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| CodeIngestError::other(e.to_string()))?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            copy_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_output() {
        assert_eq!(parse_version_output("ast-grep 0.31.0").unwrap(), "0.31.0");
        assert_eq!(
            parse_version_output("ast-grep 0.51.0 (abc123)\n").unwrap(),
            "0.51.0"
        );
        assert!(parse_version_output("").is_err());
        assert!(parse_version_output("ast-grep").is_err());
    }

    #[test]
    fn matches_release_assets() {
        let release = GitHubRelease {
            tag_name: "v0.51.0".to_string(),
            assets: vec![
                GitHubAsset {
                    name: "app-aarch64-apple-darwin.tar.gz".to_string(),
                    browser_download_url: "https://example.com/darwin.tgz".to_string(),
                },
                GitHubAsset {
                    name: "app-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                    browser_download_url: "https://example.com/linux.tgz".to_string(),
                },
                GitHubAsset {
                    name: "app-x86_64-pc-windows-msvc.zip".to_string(),
                    browser_download_url: "https://example.com/win.zip".to_string(),
                },
                GitHubAsset {
                    name: "app-x86_64-pc-windows-msvc.zip.sha256".to_string(),
                    browser_download_url: "https://example.com/win.sha".to_string(),
                },
            ],
        };
        let win = match_asset(&release, "pc-windows-msvc", "x86_64").expect("windows asset");
        assert_eq!(win.name, "app-x86_64-pc-windows-msvc.zip");
        let linux = match_asset(&release, "unknown-linux-gnu", "x86_64").expect("linux asset");
        assert_eq!(linux.name, "app-x86_64-unknown-linux-gnu.tar.gz");
        let darwin = match_asset(&release, "apple-darwin", "aarch64").expect("darwin asset");
        assert_eq!(darwin.name, "app-aarch64-apple-darwin.tar.gz");
        // Checksum files are not archives → skipped.
        assert!(match_asset(&release, "unknown-linux-gnu", "riscv64").is_none());
    }

    #[test]
    fn extracts_zip_and_tar_gz_members() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("sg.zip");
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("inner/sg.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"fake sg binary").unwrap();
        zip.finish().unwrap();

        let target = tmp.path().join("out").join("sg.exe");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        extract_zip(&archive, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"fake sg binary");

        // tar.gz
        let tgz = tmp.path().join("sg.tar.gz");
        let file = std::fs::File::create(&tgz).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let member = tmp.path().join("member-sg");
        std::fs::write(&member, b"fake tar sg").unwrap();
        tar.append_path_with_name(&member, "nested/ast-grep")
            .unwrap();
        let enc = tar.into_inner().unwrap();
        enc.finish().unwrap();

        let target2 = tmp.path().join("out2").join("sg");
        std::fs::create_dir_all(target2.parent().unwrap()).unwrap();
        extract_tar_gz(&tgz, &target2).unwrap();
        assert_eq!(std::fs::read(&target2).unwrap(), b"fake tar sg");

        // Missing member errors.
        let empty_zip = tmp.path().join("empty.zip");
        let file = std::fs::File::create(&empty_zip).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("other.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"no binary").unwrap();
        zip.finish().unwrap();
        assert!(extract_zip(&empty_zip, &target).is_err());
    }

    #[test]
    fn sync_sg_assets_copies_config_and_replaces_rules() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("sgconfig.yml"), "ruleDirs:\n  - rules\n").unwrap();
        std::fs::create_dir_all(src.path().join("rules")).unwrap();
        std::fs::write(
            src.path().join("rules").join("go-chunk.yml"),
            "id: go-chunk\n",
        )
        .unwrap();
        // Stale destination rules are replaced wholesale.
        let dst_rules = dest.path().join("rules");
        std::fs::create_dir_all(&dst_rules).unwrap();
        std::fs::write(dst_rules.join("stale.yml"), "id: stale\n").unwrap();

        sync_sg_assets(src.path(), dest.path()).unwrap();
        let cfg = std::fs::read_to_string(dest.path().join("sgconfig.yml")).unwrap();
        assert!(cfg.contains("ruleDirs"));
        assert!(dst_rules.join("go-chunk.yml").is_file());
        assert!(!dst_rules.join("stale.yml").exists());
    }
}
