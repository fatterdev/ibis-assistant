//! SurrealDB binary install/upgrade.
//!
//! Behavior contract:
//! - `surreal[.exe]` next to the ibis executable (fallback: PATH lookup).
//! - Version probe via `surreal version`; upgrade check against
//!   `https://api.github.com/repos/surrealdb/surrealdb/releases/latest`
//!   (30s API timeout, 5-minute download timeout, degrade to the local binary
//!   on failure).
//! - Asset matching: release asset name containing OS and arch with a
//!   `.tgz`/`.exe`/`.zip` suffix; chmod 0755 on Unix; on Windows the old binary
//!   is renamed to `<dest>.old` before the replacement moves in.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::ProcessError;

const RELEASES_API: &str = "https://api.github.com/repos/surrealdb/surrealdb/releases/latest";
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

/// Path of the bundled `surreal` binary: next to the ibis executable, falling
/// back to a bare name so the OS `PATH` lookup applies.
pub fn surreal_bin_path() -> PathBuf {
    let bin_name = if cfg!(windows) {
        "surreal.exe"
    } else {
        "surreal"
    };
    match std::env::current_exe() {
        Ok(exe) => exe.parent().unwrap_or(Path::new(".")).join(bin_name),
        Err(_) => PathBuf::from(bin_name),
    }
}

/// Parses `surreal version` output (e.g. `surreal 2.0.4 ...` → `2.0.4`).
pub fn parse_version_output(out: &str) -> Result<String, ProcessError> {
    let parts: Vec<&str> = out.split_whitespace().collect();
    if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("surreal") {
        return Ok(parts[1].to_string());
    }
    if let Some(first) = parts.first() {
        return Ok((*first).to_string());
    }
    Err(ProcessError::VersionOutput(out.to_string()))
}

/// Runs `surreal version` and extracts the version string.
pub async fn local_version(bin: &Path) -> Result<String, ProcessError> {
    // Bare names (no parent dir) go through the OS PATH lookup; real paths must exist.
    let is_bare = bin.parent().is_none_or(|p| p == Path::new(""));
    if !is_bare && !bin.is_file() {
        return Err(ProcessError::Other(format!("{} not found", bin.display())));
    }
    let output = tokio::time::timeout(
        VERSION_PROBE_TIMEOUT,
        tokio::process::Command::new(bin).arg("version").output(),
    )
    .await
    .map_err(|_| ProcessError::VersionOutput("version probe timed out".to_string()))?
    .map_err(ProcessError::Io)?;
    if !output.status.success() {
        return Err(ProcessError::Other(format!(
            "surreal version exited with {}",
            output.status
        )));
    }
    parse_version_output(&String::from_utf8_lossy(&output.stdout))
}

/// Fetches the latest SurrealDB release from the GitHub API.
pub async fn latest_release() -> Result<GitHubRelease, ProcessError> {
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
        return Err(ProcessError::Other(format!(
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

/// Selects the release asset matching this OS/arch with a supported archive
/// suffix (`installSurreal matching loop). Pure function, unit-tested.
pub fn match_asset<'a>(
    release: &'a GitHubRelease,
    os: &str,
    arch: &str,
) -> Option<&'a GitHubAsset> {
    for asset in &release.assets {
        let name = asset.name.to_lowercase();
        if name.contains(os)
            && name.contains(arch)
            && (name.ends_with(".tgz") || name.ends_with(".exe") || name.ends_with(".zip"))
        {
            return Some(asset);
        }
    }
    None
}

/// Checks that the SurrealDB binary is present and up to date, downloading the
/// latest GitHub release when needed.
///
/// Returns the binary path. Any network/install failure degrades to the local
/// binary when one exists.
pub async fn ensure_surreal_db(auto_update: bool) -> Result<PathBuf, ProcessError> {
    let bin_path = surreal_bin_path();
    tracing::info!(
        "Checking SurrealDB installation ({})...",
        bin_path.display()
    );

    let local = match local_version(&bin_path).await {
        Ok(v) => {
            tracing::info!("Found local SurrealDB version: {v}");
            Some(v)
        }
        Err(err) => {
            tracing::info!("SurrealDB not found or version check failed: {err}");
            None
        }
    };

    if !auto_update && local.is_some() {
        tracing::info!("SurrealDB auto-update is disabled, using local version");
        return Ok(bin_path);
    }

    let release = match latest_release().await {
        Ok(r) => r,
        Err(err) => {
            return match local {
                Some(_) => {
                    tracing::warn!("Failed to check for updates (using local version): {err}");
                    Ok(bin_path)
                }
                None => Err(ProcessError::Other(format!(
                    "failed to get release info and no local binary found: {err}"
                ))),
            };
        }
    };

    // Clean tag name (remove 'v' prefix)
    let remote = release.tag_name.trim_start_matches('v').to_string();
    if local.as_deref() == Some(remote.as_str()) {
        if let Some(version) = &local {
            tracing::info!("SurrealDB is up to date ({version}).");
        }
        return Ok(bin_path);
    }

    tracing::info!(
        "Newer SurrealDB version available: {remote} (Local: {}). Downloading...",
        local.as_deref().unwrap_or("<none>")
    );

    if let Err(err) = install_surreal(&release, &bin_path).await {
        return match local {
            Some(_) => {
                tracing::warn!("Failed to update SurrealDB (using local version): {err}");
                Ok(bin_path)
            }
            None => Err(ProcessError::Other(format!(
                "failed to install SurrealDB: {err}"
            ))),
        };
    }

    tracing::info!("SurrealDB updated successfully to {remote}.");
    Ok(bin_path)
}

/// Downloads and installs the matching release asset.
pub async fn install_surreal(release: &GitHubRelease, dest: &Path) -> Result<(), ProcessError> {
    let target_os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    // GitHub asset names carry -style GOARCH tokens (amd64/arm64).
    let target_arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        std::env::consts::ARCH
    };

    let asset = match_asset(release, target_os, target_arch).ok_or(ProcessError::NoAsset {
        os: target_os.to_string(),
        arch: target_arch.to_string(),
        tag: release.tag_name.clone(),
    })?;

    tracing::info!("Downloading {}...", asset.name);

    // Download to a unique temp file ( used a fixed name in the CWD).
    let tmp_dir = std::env::temp_dir();
    let tmp_file = tmp_dir.join(format!(
        "ibis_download_{}_{}",
        uuid::Uuid::new_v4(),
        asset.name
    ));
    let outcome = match download_file(&asset.browser_download_url, &tmp_file).await {
        Ok(()) => {
            let installed = install_asset(&tmp_file, asset, dest);
            let _ = std::fs::remove_file(&tmp_file); // best-effort cleanup
            installed
        }
        Err(err) => {
            let _ = std::fs::remove_file(&tmp_file);
            Err(err)
        }
    };
    outcome
}

fn install_asset(tmp_file: &Path, asset: &GitHubAsset, dest: &Path) -> Result<(), ProcessError> {
    let name = asset.name.to_lowercase();
    if name.ends_with(".exe") {
        // Windows: just rename/move
        replace_binary(tmp_file, dest)?;
    } else if name.ends_with(".tgz") {
        // Linux/Mac: extract
        extract_tar_gz(tmp_file, dest)?;
    } else if name.ends_with(".zip") {
        // Windows: extract zip
        extract_zip(tmp_file, dest)?;
    } else {
        return Err(ProcessError::Other(format!(
            "unsupported asset type: {}",
            asset.name
        )));
    }
    // Ensure executable permissions (Unix)
    #[cfg(unix)]
    set_executable(dest)?;
    Ok(())
}

async fn download_file(url: &str, dest: &Path) -> Result<(), ProcessError> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut out = std::fs::File::create(dest)?;
    // Stream to disk to avoid buffering large archives in memory.
    while let Some(chunk) = response.chunk().await? {
        std::io::copy(&mut &chunk[..], &mut out)?;
    }
    out.flush()?;
    Ok(())
}

/// Replaces an existing binary, renaming the old one to `<dest>.old` first on
/// Windows (where a running binary cannot be overwritten).
fn replace_binary(src: &Path, dest: &Path) -> Result<(), ProcessError> {
    if dest.exists() {
        let backup = dest_with_suffix(dest, ".old");
        let _ = std::fs::remove_file(&backup); // ignore error
        std::fs::rename(dest, &backup).map_err(|e| ProcessError::ReplaceBinary(e.to_string()))?;
    }
    std::fs::rename(src, dest)
        .map_err(|e| ProcessError::Other(format!("failed to move new binary into place: {e}")))
}

fn dest_with_suffix(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    dest.with_file_name(name)
}

/// Extracts the `surreal` binary from a `.tgz` archive.
fn extract_tar_gz(archive: &Path, target: &Path) -> Result<(), ProcessError> {
    let file = std::fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    for entry in tar
        .entries()
        .map_err(|e| ProcessError::Archive(e.to_string()))?
    {
        let entry = entry.map_err(|e| ProcessError::Archive(e.to_string()))?;
        let name = entry
            .path()
            .map_err(|e| ProcessError::Archive(e.to_string()))?
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name == "surreal" || name == "surreal.exe" {
            let mut out = std::fs::File::create(target)?;
            let mut entry = entry;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    Err(ProcessError::Archive(
        "binary 'surreal' not found in archive".to_string(),
    ))
}

/// Extracts the `surreal` binary from a `.zip` archive.
fn extract_zip(archive: &Path, target: &Path) -> Result<(), ProcessError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| ProcessError::Archive(e.to_string()))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| ProcessError::Archive(e.to_string()))?;
        let name = entry
            .enclosed_name()
            .map(|n| n.file_name().map(|f| f.to_string_lossy().to_string()))
            .unwrap_or_default()
            .unwrap_or_default();
        if name == "surreal" || name == "surreal.exe" {
            let mut out = std::fs::File::create(target)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    Err(ProcessError::Archive(
        "binary 'surreal' not found in zip archive".to_string(),
    ))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), ProcessError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| ProcessError::Other(format!("failed to set executable permissions: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_output() {
        assert_eq!(
            parse_version_output("surreal 2.0.4 linux amd64").unwrap(),
            "2.0.4"
        );
        assert_eq!(parse_version_output("Surreal 3.3.4").unwrap(), "3.3.4");
        assert_eq!(parse_version_output("3.3.4").unwrap(), "3.3.4");
        assert!(parse_version_output("").is_err());
    }

    #[test]
    fn matches_release_assets() {
        let release = GitHubRelease {
            tag_name: "v3.3.4".to_string(),
            assets: vec![
                GitHubAsset {
                    name: "surreal-v3.3.4.darwin-arm64.tgz".to_string(),
                    browser_download_url: "https://example.com/darwin.tgz".to_string(),
                },
                GitHubAsset {
                    name: "surreal-v3.3.4.linux-amd64.tgz".to_string(),
                    browser_download_url: "https://example.com/linux.tgz".to_string(),
                },
                GitHubAsset {
                    name: "surreal-v3.3.4.windows-amd64.zip".to_string(),
                    browser_download_url: "https://example.com/win.zip".to_string(),
                },
                GitHubAsset {
                    name: "surreal-v3.3.4 checksums.txt".to_string(),
                    browser_download_url: "https://example.com/sums".to_string(),
                },
            ],
        };
        let win = match_asset(&release, "windows", "amd64").expect("windows asset");
        assert_eq!(win.name, "surreal-v3.3.4.windows-amd64.zip");
        let linux = match_asset(&release, "linux", "amd64").expect("linux asset");
        assert_eq!(linux.name, "surreal-v3.3.4.linux-amd64.tgz");
        let darwin = match_asset(&release, "darwin", "arm64").expect("darwin asset");
        assert_eq!(darwin.name, "surreal-v3.3.4.darwin-arm64.tgz");
        assert!(match_asset(&release, "plan9", "amd64").is_none());
    }

    #[test]
    fn trims_v_prefix_and_keeps_destination_backup_naming() {
        assert_eq!("v3.3.4".trim_start_matches('v'), "3.3.4");
        let dest = Path::new("C:\\tools\\surreal.exe");
        assert_eq!(
            dest_with_suffix(dest, ".old"),
            Path::new("C:\\tools\\surreal.exe.old")
        );
    }
}
