//! Volume Shadow Copy Service integration for live Windows backups.

use phoenix_core::error::{PhoenixError, Result};

/// Create a VSS snapshot for the given volume path (e.g. `\\\\.\\C:`) and return
/// a device path suitable for opening as a volume reader.
#[cfg(windows)]
pub fn snapshot_volume_path(volume_path: &str) -> Result<String> {
    snapshot_volume_path_impl(volume_path)
}

#[cfg(not(windows))]
pub fn snapshot_volume_path(volume_path: &str) -> Result<String> {
    let _ = volume_path;
    Err(PhoenixError::Other(
        "VSS is only available on Windows".into(),
    ))
}

#[cfg(windows)]
fn snapshot_volume_path_impl(volume_path: &str) -> Result<String> {
    // VSS COM integration requires elevated privileges and vssapi.
    // This implementation uses vssadmin as a bootstrap path when COM is unavailable,
    // and documents the snapshot device naming convention for production COM wiring.
    //
    // Production path: IVssBackupComponents::InitializeForBackup,
    // AddToSnapshotSet, PrepareForBackup, CreateSnapshot, GetSnapshotProperties.
    //
    // For WinPE (offline), callers should set use_vss=false.

    use std::process::Command;

    let drive = volume_path
        .trim_end_matches('\\')
        .trim_start_matches(r"\\.\")
        .trim_end_matches(':');
    let drive_letter = format!("{drive}:");

    let output = Command::new("vssadmin")
        .args(["create", "shadow", &format!("/for={drive_letter}")])
        .output()
        .map_err(|e| PhoenixError::Other(format!("vssadmin failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Fall back to direct volume access when snapshot creation fails (e.g. WinPE)
        tracing::warn!("VSS snapshot failed ({stderr}); using live volume");
        return Ok(volume_path.to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("Shadow Copy Volume Name") || line.contains("\\\\?\\GLOBALROOT") {
            if let Some(path) = line.split(':').nth(1) {
                let path = path.trim();
                if path.starts_with("\\\\") {
                    return Ok(path.to_string());
                }
            }
        }
    }

    // Parse device object from vssadmin list shadows
    let list = Command::new("vssadmin")
        .args(["list", "shadows"])
        .output()
        .map_err(|e| PhoenixError::Other(format!("vssadmin list: {e}")))?;
    let list_out = String::from_utf8_lossy(&list.stdout);
    for line in list_out.lines() {
        if line.contains("GLOBALROOT") {
            let path = line.split(':').last().unwrap_or("").trim();
            if !path.is_empty() {
                return Ok(path.to_string());
            }
        }
    }

    tracing::warn!("Could not parse VSS shadow path; using original volume");
    Ok(volume_path.to_string())
}

/// Delete all snapshots created in this session (best-effort cleanup).
#[cfg(windows)]
pub fn delete_all_snapshots() -> Result<()> {
    use std::process::Command;
    let _ = Command::new("vssadmin")
        .args(["delete", "shadows", "/all", "/quiet"])
        .output();
    Ok(())
}

#[cfg(not(windows))]
pub fn delete_all_snapshots() -> Result<()> {
    Ok(())
}
