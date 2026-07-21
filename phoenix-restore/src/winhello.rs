//! Reset Windows Hello: clear a Windows installation's NGC credential store so
//! its PIN and biometric sign-in are wiped and can be enrolled again.
//!
//! Cloning or restoring a system disk changes the machine identity Windows
//! binds Hello credentials to (the TPM/DPAPI keys that seal them), so the PIN
//! and biometrics on the copy stop working — and often can't even be removed
//! through Settings. Deleting the NGC folder forces Windows to treat Hello as
//! never-configured on the next sign-in, at which point it can be set up fresh.
//!
//! Mirrors the community Reset-NGC procedure (take ownership + grant
//! Administrators + delete of
//! `…\ServiceProfiles\LocalService\AppData\Local\Microsoft\Ngc`), but targets a
//! *chosen* installation — usually an OFFLINE clone on an attached disk —
//! rather than only the running system. The same [`WindowsInstall`] the Boot
//! Repair page enumerates identifies the target. When the target IS the live
//! system the Windows Biometric Service is stopped first and restarted after,
//! and a reboot is advised before re-enrolling.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use phoenix_core::disk::DiskInfo;
use phoenix_core::error::{PhoenixError, Result};
use tracing::{info, warn};

use crate::bootrepair::{system_disk_index, WindowsInstall};

/// Don't flash a console window for the `takeown`/`icacls`/`net` helpers.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The NGC store path under a Windows installation's volume, when that volume
/// carries a drive letter. An offline install without a letter can't be reached
/// by path — the caller surfaces that as "assign a drive letter first" rather
/// than guessing a volume-GUID path the shell tools can't all take.
pub fn ngc_dir(install: &WindowsInstall) -> Option<PathBuf> {
    let letter = install.drive_letter?;
    Some(PathBuf::from(format!(
        r"{letter}:\Windows\ServiceProfiles\LocalService\AppData\Local\Microsoft\Ngc"
    )))
}

/// A fully resolved reset: which install, the NGC path, and whether the target
/// is the live system (which additionally cycles the biometric service and
/// wants a reboot). Producing the plan is read-only; nothing is touched until
/// [`execute_reset`].
#[derive(Debug, Clone)]
pub struct ResetPlan {
    pub install: WindowsInstall,
    pub ngc_path: PathBuf,
    /// True when the target is the disk this machine is currently running from.
    pub is_live_system: bool,
    /// Human-readable action list for the confirmation UI.
    pub actions: Vec<String>,
}

/// Resolve a reset for `install`. Fails only when the volume can't be reached
/// by path (no drive letter); an already-clean install is *not* an error here
/// (the folder is re-checked at execution time, where "already reset" is the
/// friendlier message).
pub fn plan_reset(disks: &[DiskInfo], install: &WindowsInstall) -> Result<ResetPlan> {
    let ngc_path = ngc_dir(install).ok_or_else(|| {
        PhoenixError::Other(
            "the selected Windows volume has no drive letter — assign one (Disk Management) so \
             its files can be reached, then try again"
                .into(),
        )
    })?;
    let is_live_system = system_disk_index(disks) == Some(install.disk_index);

    let mut actions = Vec::new();
    if is_live_system {
        actions.push("Stop the Windows Biometric Service (WbioSrvc)".to_string());
    }
    actions.push(format!("Take ownership of {}", ngc_path.display()));
    actions.push("Grant Administrators full control of it".to_string());
    actions.push("Delete the NGC credential store — every PIN and biometric enrolment".to_string());
    if is_live_system {
        actions.push("Restart the Windows Biometric Service".to_string());
        actions.push("Reboot recommended before setting Windows Hello up again".to_string());
    }
    Ok(ResetPlan {
        install: install.clone(),
        ngc_path,
        is_live_system,
        actions,
    })
}

/// What [`execute_reset`] actually did, for the results screen.
#[derive(Debug, Clone)]
pub struct ResetReport {
    pub actions: Vec<String>,
}

/// Carry out the reset. Takes ownership + grants Administrators (so the
/// restrictive NGC ACLs can't block removal), deletes the tree, and — only for
/// the live system — cycles the biometric service around the delete.
///
/// The ownership/permission steps are best-effort (their failures are logged,
/// not fatal): the authoritative check is whether the folder is gone at the
/// end. A folder that was never there is reported as "already reset" rather
/// than silently succeeding on nothing.
pub fn execute_reset(plan: &ResetPlan) -> Result<ResetReport> {
    let ngc = &plan.ngc_path;
    if !ngc.exists() {
        return Err(PhoenixError::Other(format!(
            "no NGC folder at {} — Windows Hello is already cleared on this installation",
            ngc.display()
        )));
    }

    let mut done = Vec::new();
    if plan.is_live_system {
        run_net("stop");
        done.push("Stopped the Windows Biometric Service".to_string());
    }

    take_ownership(ngc);
    grant_admins(ngc);
    remove_tree(ngc);

    if ngc.exists() {
        // Restart the service we stopped before bailing, so a failed reset
        // doesn't leave the live system with Hello disabled.
        if plan.is_live_system {
            run_net("start");
        }
        return Err(PhoenixError::Other(format!(
            "the NGC folder at {} could not be fully removed — a file may still be locked; \
             reboot and try again",
            ngc.display()
        )));
    }
    done.push("Deleted the NGC credential store".to_string());

    if plan.is_live_system {
        run_net("start");
        done.push("Restarted the Windows Biometric Service".to_string());
        done.push("Reboot before setting up Windows Hello again".to_string());
    }

    info!(target: "phoenix_restore::winhello", path = %ngc.display(), live = plan.is_live_system, "NGC reset completed");
    Ok(ResetReport { actions: done })
}

/// `net <verb> WbioSrvc`. Best-effort: the biometric service is often already
/// stopped (offline install, or the user never used a fingerprint reader), and
/// a stop/start failure shouldn't sink the whole reset.
fn run_net(verb: &str) {
    match Command::new("net")
        .args([verb, "WbioSrvc"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if !o.status.success() => {
            warn!(target: "phoenix_restore::winhello", verb, stderr = %String::from_utf8_lossy(&o.stderr), "net WbioSrvc non-zero");
        }
        Err(e) => warn!(target: "phoenix_restore::winhello", verb, "net WbioSrvc failed to launch: {e}"),
        _ => {}
    }
}

/// `takeown /f <ngc> /r /d y` — recursive, defaulting the per-item prompt to
/// yes. Best-effort (logged, not fatal): removal is the real gate.
fn take_ownership(path: &Path) {
    let out = Command::new("takeown")
        .arg("/f")
        .arg(path)
        .args(["/r", "/d", "y"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    if let Ok(o) = &out {
        if !o.status.success() {
            warn!(target: "phoenix_restore::winhello", "takeown reported failures: {}", String::from_utf8_lossy(&o.stderr));
        }
    } else if let Err(e) = out {
        warn!(target: "phoenix_restore::winhello", "takeown failed to launch: {e}");
    }
}

/// `icacls <ngc> /grant *S-1-5-32-544:(F) /t` — grant the Administrators group
/// full control recursively. The well-known SID is used instead of the name
/// "administrators" so the call works on non-English Windows too.
fn grant_admins(path: &Path) {
    let out = Command::new("icacls")
        .arg(path)
        .args(["/grant", "*S-1-5-32-544:(F)", "/t"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    if let Ok(o) = &out {
        if !o.status.success() {
            warn!(target: "phoenix_restore::winhello", "icacls reported failures: {}", String::from_utf8_lossy(&o.stderr));
        }
    } else if let Err(e) = out {
        warn!(target: "phoenix_restore::winhello", "icacls failed to launch: {e}");
    }
}

/// Delete the NGC tree. Tries the in-process removal first (fast, and enough
/// once Administrators has full control), falling back to `rd /s /q` — the
/// exact command the reference script uses — if the std call trips over a
/// stubborn ACL. Neither is treated as fatal here; the caller re-checks
/// existence and reports the real outcome.
fn remove_tree(path: &Path) {
    if let Err(e) = std::fs::remove_dir_all(path) {
        warn!(target: "phoenix_restore::winhello", "std remove failed ({e}); falling back to rd /s /q");
        let _ = Command::new("cmd")
            .args(["/c", "rd", "/s", "/q"])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(letter: Option<char>, disk: u32) -> WindowsInstall {
        WindowsInstall {
            disk_index: disk,
            partition_index: 2,
            drive_letter: letter,
            volume_label: Some("OS".into()),
            volume_path: r"\\.\E:".into(),
            version: Some("Windows 11 (build 26100)".into()),
        }
    }

    #[test]
    fn ngc_dir_uses_the_volume_letter() {
        let p = ngc_dir(&install(Some('E'), 3)).expect("lettered volume yields a path");
        assert_eq!(
            p,
            PathBuf::from(
                r"E:\Windows\ServiceProfiles\LocalService\AppData\Local\Microsoft\Ngc"
            )
        );
    }

    #[test]
    fn unlettered_volume_has_no_path() {
        assert!(ngc_dir(&install(None, 3)).is_none());
    }

    #[test]
    fn plan_refuses_a_letterless_volume() {
        // No disks needed: the drive-letter check fails before the
        // live-system lookup runs.
        let err = plan_reset(&[], &install(None, 3)).unwrap_err();
        assert!(matches!(err, PhoenixError::Other(_)));
    }
}
