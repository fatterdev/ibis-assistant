//! System GPU probe — Windows registry first, then `nvidia-smi`.
//!
//! The Windows registry probe reads the 64-bit
//! `HardwareInformation.qwMemorySize` value under
//! `HKLM\SYSTEM\CurrentControlSet\Enum\PCI\<device>\<instance>` — no process
//! spawn, full QWORD range — and keeps the `nvidia-smi` CLI as the fallback
//! (with a bounded wait instead of an unbounded child wait).
//!
//! Parity note: the system GPU is probed on Windows only; non-Windows
//! returns `None` here too.

use std::time::Duration;

use async_trait::async_trait;

use crate::engine::{GpuInfo, SystemGpuProbe};

/// Ollama HTTP probe timeout.
pub const OLLAMA_PROBE_HTTP_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounded wait for the `nvidia-smi` child process — a stalled driver can no
/// longer hang the probe.
pub const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// Production system-GPU probe (Windows only, parity).
pub struct SystemGpuProbeImpl;

#[async_trait]
impl SystemGpuProbe for SystemGpuProbeImpl {
    async fn probe(&self) -> Option<GpuInfo> {
        system_gpu_probe().await
    }
}

/// Registry first, then `nvidia-smi`.
pub(crate) async fn system_gpu_probe() -> Option<GpuInfo> {
    #[cfg(windows)]
    {
        if let Some(info) = probe_windows_registry().await {
            return Some(info);
        }
        probe_nvidia_smi().await
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Vendor classification from the PCI `VEN_xxxx` device-key prefix, falling
/// back to the name-substring heuristic.
#[cfg(windows)]
fn vendor_from_device_key(device_key: &str) -> &str {
    let key = device_key.to_ascii_uppercase();
    if key.starts_with("VEN_1002") {
        "amd"
    } else if key.starts_with("VEN_10DE") {
        "nvidia"
    } else if key.starts_with("VEN_8086") {
        "intel"
    } else {
        ""
    }
}

/// Name-substring vendor fallback, default "unknown".
#[cfg(windows)]
fn vendor_from_name(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("amd") || lower.contains("radeon") {
        "amd"
    } else if lower.contains("nvidia") || lower.contains("geforce") {
        "nvidia"
    } else if lower.contains("intel") {
        "intel"
    } else {
        "unknown"
    }
}

/// Enumerates `HKLM\SYSTEM\CurrentControlSet\Enum\PCI` for the adapter with
/// the largest non-zero 64-bit `HardwareInformation.qwMemorySize`.
///
/// The instance key carries `FriendlyName`; the `HardwareInformation` subkey
/// carries `qwMemorySize` (REG_QWORD, bytes). Registry reads are in-memory and
/// fast; no spawn is involved. Errors degrade to `None` with a debug log.
#[cfg(windows)]
async fn probe_windows_registry() -> Option<GpuInfo> {
    const PCI_ROOT: &str = r"SYSTEM\CurrentControlSet\Enum\PCI";
    let best = tokio::task::spawn_blocking(move || -> Option<GpuInfo> {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let pci = hklm.open_subkey(PCI_ROOT).ok()?;
        let mut best: Option<GpuInfo> = None;
        for device in pci.enum_keys().filter_map(Result::ok) {
            let vendor_key = vendor_from_device_key(&device);
            let Ok(dev_key) = pci.open_subkey(&device) else {
                continue;
            };
            for instance in dev_key.enum_keys().filter_map(Result::ok) {
                let Ok(inst) = dev_key.open_subkey(&instance) else {
                    continue;
                };
                let name: String = inst
                    .get_value("FriendlyName")
                    .unwrap_or_else(|_| device.clone());
                let vram_bytes: u64 = inst
                    .open_subkey("HardwareInformation")
                    .ok()
                    .and_then(|hw| hw.get_value("qwMemorySize").ok())
                    .unwrap_or(0);
                if vram_bytes == 0 {
                    continue;
                }
                let vram_mib = (vram_bytes / (1024 * 1024)) as i64;
                let better = match &best {
                    Some(cur) => vram_mib > cur.vram_mib,
                    None => true,
                };
                if better {
                    let vendor = if vendor_key.is_empty() {
                        vendor_from_name(&name).to_string()
                    } else {
                        vendor_key.to_string()
                    };
                    best = Some(GpuInfo {
                        vram_mib,
                        name,
                        vendor,
                    });
                }
            }
        }
        best
    })
    .await;
    match best {
        Ok(found) => found,
        Err(err) => {
            tracing::debug!("GPU registry probe failed: {err}");
            None
        }
    }
}

/// `nvidia-smi --query-gpu=memory.total,name --format=csv,noheader,nounits`,
/// bounded by [`NVIDIA_SMI_TIMEOUT`]
/// (`kill_on_drop` terminates a stalled child).
#[cfg(windows)]
async fn probe_nvidia_smi() -> Option<GpuInfo> {
    use std::process::Stdio;

    let child = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total,name",
            "--format=csv,noheader,nounits",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    // `wait_with_output` consumes the child; on timeout the dropped future
    // drops the child, and `kill_on_drop` terminates the stalled process.
    let output = match tokio::time::timeout(NVIDIA_SMI_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => out,
        _ => return None,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // Legacy behavior split on "\n" and took the first line; CRLF output left a
    // trailing "\r" on the name field — trim it.
    let line = text.lines().next().unwrap_or("").trim();
    let mut parts = line.split(',');
    let mib = parts.next()?.trim().parse::<i64>().ok()?;
    let name = parts.next().unwrap_or("").trim().to_string();
    Some(GpuInfo {
        vram_mib: mib,
        name,
        vendor: "nvidia".to_string(),
    })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// PCI VEN_ prefix → vendor (registry probe helper).
    #[test]
    fn vendor_from_device_key_prefix() {
        assert_eq!(
            vendor_from_device_key("VEN_1002&DEV_744C&SUBSYS_1210"),
            "amd"
        );
        assert_eq!(vendor_from_device_key("ven_10de&dev_2504"), "nvidia");
        assert_eq!(vendor_from_device_key("VEN_8086&DEV_A7A0"), "intel");
        assert_eq!(vendor_from_device_key("VEN_1234&DEV_5678"), "");
    }

    /// Name-substring fallback.
    #[test]
    fn vendor_from_name_matrix() {
        assert_eq!(vendor_from_name("AMD Radeon RX 7900 XT"), "amd");
        assert_eq!(vendor_from_name("NVIDIA GeForce RTX 4080"), "nvidia");
        assert_eq!(vendor_from_name("Intel(R) UHD Graphics"), "intel");
        assert_eq!(vendor_from_name("Generic Display Adapter"), "unknown");
    }
}

#[cfg(all(test, not(windows)))]
mod non_windows_tests {
    use super::*;

    /// Non-Windows systems: the probe returns `None`.
    #[tokio::test]
    async fn non_windows_probe_is_none() {
        assert!(system_gpu_probe().await.is_none());
    }
}
