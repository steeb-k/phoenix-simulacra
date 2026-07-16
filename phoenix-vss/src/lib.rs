//! Volume Shadow Copy Service integration for live Windows backups.
//!
//! Why we shell out to PowerShell instead of calling the COM API directly:
//! the IVssBackupComponents interface requires a substantial amount of
//! boilerplate (writers, components, snapshot sets, async waits) and the
//! `vssadmin create shadow /for=...` shortcut works **only on Windows
//! Server**. Every Windows 10/11 client SKU rejects it with empty stderr,
//! which is exactly what produced the silent "VSS snapshot failed (); using
//! live volume" warning that prompted this rewrite.
//!
//! The WMI class `Win32_ShadowCopy` exposes a stable `Create(Volume,
//! Context)` method that works on both client and Server SKUs as long as
//! the caller is elevated, so we drive it through PowerShell. That lets us
//! reuse the OS's snapshot machinery without dragging a COM dependency into
//! the build.

use phoenix_core::error::Result;
// `PhoenixError` is only constructed on non-Windows hosts where VSS isn't
// available; gating the import keeps the Windows build warning-free without
// introducing dead `#[allow]` attributes scattered through the file.
#[cfg(not(windows))]
use phoenix_core::error::PhoenixError;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// `CREATE_NO_WINDOW` (from `winbase.h`). The GUI is a `/SUBSYSTEM:WINDOWS`
/// process with no console of its own, so every `powershell` / `vssadmin` child
/// we spawn to drive VSS would otherwise pop a visible console window — one per
/// snapshot create and one per teardown, flashing on screen during a backup.
/// This flag runs them fully hidden.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A scope-bound set of VSS shadow copies. Snapshots created via
/// [`VssSession::snapshot_volume`] are tracked by ShadowID and torn down on
/// `Drop`, so a backup run can't leak shadow copies even if it panics or is
/// cancelled mid-flight.
///
/// The previous implementation called `vssadmin delete shadows /all /quiet`
/// at session end which would clobber unrelated snapshots — including ones
/// the user created for other purposes. Tracking IDs ensures we only ever
/// delete what we created.
pub struct VssSession {
    #[cfg(windows)]
    shadow_ids: Vec<String>,
}

impl VssSession {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            shadow_ids: Vec::new(),
        }
    }

    /// Create a VSS snapshot of `volume_path` (e.g. `\\.\C:`) and return a
    /// device path suitable for opening with `CreateFileW` (typically the
    /// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN` form).
    ///
    /// On any failure — non-elevated process, VSS service stopped, drive
    /// without shadow storage — we **don't** propagate the error. Instead
    /// we log a warning that names the actual failure mode and return the
    /// original live-volume path so backup can still proceed. Reading from a
    /// live volume is less consistent than from a shadow but is always
    /// preferable to aborting the backup outright.
    #[cfg(windows)]
    pub fn snapshot_volume(&mut self, volume_path: &str) -> Result<String> {
        if let Some((dev, id)) = create_snapshot(volume_path) {
            self.shadow_ids.push(id);
            return Ok(dev);
        }
        Ok(volume_path.to_string())
    }

    #[cfg(not(windows))]
    pub fn snapshot_volume(&mut self, volume_path: &str) -> Result<String> {
        let _ = volume_path;
        Err(PhoenixError::Other(
            "VSS is only available on Windows".into(),
        ))
    }
}

impl Default for VssSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl Drop for VssSession {
    fn drop(&mut self) {
        if self.shadow_ids.is_empty() {
            return;
        }
        for id in std::mem::take(&mut self.shadow_ids) {
            delete_snapshot_by_id(&id);
        }
    }
}

/// Backwards-compatible single-shot wrapper. Creates a snapshot but **does
/// not** track it for cleanup, so callers that don't manage their own
/// `VssSession` will leak shadow copies until the OS reclaims them. New
/// code should prefer [`VssSession`].
#[cfg(windows)]
pub fn snapshot_volume_path(volume_path: &str) -> Result<String> {
    if let Some((dev, _id)) = create_snapshot(volume_path) {
        Ok(dev)
    } else {
        Ok(volume_path.to_string())
    }
}

#[cfg(not(windows))]
pub fn snapshot_volume_path(volume_path: &str) -> Result<String> {
    let _ = volume_path;
    Err(PhoenixError::Other(
        "VSS is only available on Windows".into(),
    ))
}

/// Best-effort cleanup of every shadow copy on the system. **Destructive** —
/// deletes shadows that other tools may rely on (System Restore, prior
/// backups). Kept for compatibility but new code should rely on
/// [`VssSession`]'s scoped cleanup instead.
#[cfg(windows)]
pub fn delete_all_snapshots() -> Result<()> {
    use std::process::Command;
    let _ = Command::new("vssadmin")
        .args(["delete", "shadows", "/all", "/quiet"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    Ok(())
}

#[cfg(not(windows))]
pub fn delete_all_snapshots() -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn create_snapshot(volume_path: &str) -> Option<(String, String)> {
    use std::process::Command;

    // Normalize whatever volume form the caller has into a root path
    // `Win32_ShadowCopy.Create` accepts: `X:\` for lettered volumes, or the
    // `\\?\Volume{GUID}\` form (trailing backslash required) for un-lettered
    // ones like the EFI System / Recovery partitions. Anything else (an
    // existing GLOBALROOT shadow path, a physical-drive path) can't be
    // snapshotted; the caller falls back to reading the path as-is.
    let drive_root = if let Some(c) = extract_drive_letter(volume_path) {
        format!("{c}:\\")
    } else if volume_path.starts_with(r"\\?\Volume{") {
        let mut s = volume_path.trim_end_matches('\\').to_string();
        s.push('\\');
        s
    } else {
        tracing::warn!(
            volume = volume_path,
            "VSS: not a snapshottable volume path; reading from the path as-is"
        );
        return None;
    };

    // Single-quoted PowerShell string is literal, so the trailing backslash
    // in `C:\` doesn't need escaping. We separate the device path and ID on
    // distinct lines so parsing is unambiguous.
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n\
         try {{\n\
         \t$res = Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create \
         -Arguments @{{ Volume = '{drive_root}'; Context = 'ClientAccessible' }}\n\
         }} catch {{\n\
         \tWrite-Error $_.Exception.Message\n\
         \texit 4\n\
         }}\n\
         if ($res.ReturnValue -ne 0) {{\n\
         \tWrite-Error (\"Win32_ShadowCopy.Create returned {{0}}\" -f $res.ReturnValue)\n\
         \texit 2\n\
         }}\n\
         $shadow = Get-CimInstance -ClassName Win32_ShadowCopy \
         -Filter (\"ID='\" + $res.ShadowID + \"'\")\n\
         if (-not $shadow) {{\n\
         \tWrite-Error 'shadow created but Win32_ShadowCopy lookup failed'\n\
         \texit 3\n\
         }}\n\
         Write-Output $shadow.DeviceObject\n\
         Write-Output $res.ShadowID\n",
        drive_root = drive_root,
    );

    let output = match Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                err = %e,
                volume = drive_root.as_str(),
                "VSS: failed to launch powershell; using live volume"
            );
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        // Surface both streams; PowerShell's CIM cmdlets sometimes write
        // diagnostics to stdout (especially when the user isn't elevated).
        tracing::warn!(
            exit_code = output.status.code().unwrap_or(-1),
            volume = drive_root.as_str(),
            stderr = stderr.trim(),
            stdout = stdout.trim(),
            "VSS snapshot creation failed; using live volume \
             (the process must be elevated, the Volume Shadow Copy service \
             must be running, and the source disk needs available shadow storage)"
        );
        return None;
    }

    let mut device = None;
    let mut shadow_id = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if device.is_none()
            && (trimmed.starts_with(r"\\?\GLOBALROOT")
                || trimmed.starts_with(r"\\.\HarddiskVolumeShadowCopy"))
        {
            device = Some(trimmed.to_string());
            continue;
        }
        if shadow_id.is_none() && trimmed.starts_with('{') && trimmed.ends_with('}') {
            shadow_id = Some(trimmed.to_string());
        }
    }

    match (device, shadow_id) {
        (Some(dev), Some(id)) => {
            tracing::info!(
                volume = drive_root.as_str(),
                shadow = dev.as_str(),
                "VSS snapshot created"
            );
            Some((dev, id))
        }
        _ => {
            tracing::warn!(
                volume = drive_root.as_str(),
                stdout = stdout.trim(),
                "VSS: could not parse shadow device path or ID; using live volume"
            );
            None
        }
    }
}

#[cfg(windows)]
fn delete_snapshot_by_id(shadow_id: &str) {
    use std::process::Command;
    // `Get-CimInstance ... | Remove-CimInstance` is the supported way to
    // remove an individual shadow on both client and Server SKUs. Best
    // effort — if it fails the snapshot will just live until the OS's
    // diff-area pressure forces it out.
    let script = format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
         $s = Get-CimInstance -ClassName Win32_ShadowCopy -Filter (\"ID='{id}'\")\n\
         if ($s) {{ Remove-CimInstance -InputObject $s }}\n",
        id = shadow_id,
    );
    let result = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match result {
        Ok(o) if !o.status.success() => {
            tracing::debug!(
                shadow_id = shadow_id,
                stderr = String::from_utf8_lossy(&o.stderr).trim(),
                "VSS: failed to delete tracked shadow (best-effort)"
            );
        }
        Err(e) => tracing::debug!(shadow_id, err = %e, "VSS: powershell delete failed"),
        _ => {}
    }
}

#[cfg(windows)]
fn extract_drive_letter(volume_path: &str) -> Option<char> {
    // Accept the `\\.\X:` device-namespace form produced elsewhere in the
    // crate, the Win32 `\\?\X:\` form, and a bare `X:` path. Reject anything
    // that doesn't look like a single letter followed by a colon — physical
    // drives, GLOBALROOT shadow paths, etc.
    let stripped = volume_path
        .strip_prefix(r"\\.\")
        .or_else(|| volume_path.strip_prefix(r"\\?\"))
        .unwrap_or(volume_path);
    let mut chars = stripped.chars();
    let letter = chars.next()?;
    let colon = chars.next()?;
    if colon != ':' || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(letter.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn extract_drive_letter_handles_dos_namespace() {
        assert_eq!(extract_drive_letter(r"\\.\C:"), Some('C'));
        assert_eq!(extract_drive_letter(r"\\?\D:\"), Some('D'));
        assert_eq!(extract_drive_letter("e:"), Some('E'));
        assert_eq!(extract_drive_letter(r"\\.\PhysicalDrive0"), None);
        assert_eq!(extract_drive_letter(r"\\?\GLOBALROOT\Device\X"), None);
    }
}
