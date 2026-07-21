//! Enable or disable the built-in **Administrator** account (RID 500) on a
//! chosen Windows installation.
//!
//! A recovery tool for cloned or restored drives: when a copied Windows won't
//! let you in (broken Hello, forgotten password, disabled profile), turning on
//! its built-in Administrator gives a way back in. The built-in account is
//! disabled by default and normally has a blank password, so enabling it on a
//! default install is a passwordless admin login.
//!
//! Two backends, picked by whether the target is the running system:
//!
//! * **Live** (the install this PC booted from): driven through PowerShell's
//!   `*-LocalUser` cmdlets, keyed by the account SID so it survives a rename.
//!   Enabling also blanks the password (best-effort — a password policy can
//!   refuse it, which is reported without failing the enable).
//! * **Offline** (an attached, non-running install): the account can't be
//!   reached through any live API, so its SAM registry hive is loaded and the
//!   `ACB_DISABLED` bit in the RID-500 user's `F` value is flipped directly —
//!   the same mechanism `chntpw` uses. The password is NOT touched offline
//!   (safely clearing the stored hash is unreliable on modern Windows); the
//!   built-in account is normally blank anyway.
//!
//! Enabling by setting a specific state (not blind-toggling) is deliberate: the
//! caller decided "enable" or "disable" from the state it showed the user, so
//! the operation is idempotent and can't invert a stale reading.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::ptr;

use phoenix_core::disk::DiskInfo;
use phoenix_core::error::{PhoenixError, Result};
use tracing::{info, warn};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, LUID,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegLoadKeyW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, RegUnLoadKeyW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_BINARY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::bootrepair::{system_disk_index, WindowsInstall};

/// Don't flash a console window for the PowerShell helper.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `ACB_DISABLED` — the account-disabled bit in the SAM user's `F` value flags.
const ACB_DISABLED: u16 = 0x0001;
/// Byte offset of the 2-byte little-endian account-control flags within `F`.
/// Stable across NT versions and the offset `chntpw` and friends use.
const F_ACB_OFFSET: usize = 0x38;
/// Temporary mount name for the offline SAM hive under `HKLM`.
const SAM_MOUNT: &str = "PHNX_SAM_ADMIN";
/// Registry path (below the mount) to the built-in Administrator (RID 500 =
/// 0x1F4). The key name is the RID in uppercase hex, zero-padded to 8 digits,
/// regardless of any account rename.
const RID500_SUBKEY: &str = "PHNX_SAM_ADMIN\\SAM\\Domains\\Account\\Users\\000001F4";

/// The built-in Administrator's current state on one installation.
#[derive(Debug, Clone)]
pub struct AdminState {
    pub enabled: bool,
    /// Display name (may be renamed from "Administrator"); best-effort — the
    /// offline path reports the generic "Administrator".
    pub account_name: String,
    /// True when this is the installation this PC is running from.
    pub is_live: bool,
    /// The account SID, resolved on the live path (needed to drive the
    /// `*-LocalUser` cmdlets). `None` offline.
    pub sid: Option<String>,
}

/// What [`execute_set`] did.
#[derive(Debug, Clone)]
pub struct ToggleReport {
    pub actions: Vec<String>,
    pub enabled_now: bool,
}

/// The SAM hive path for an install's volume, when the volume has a drive
/// letter. Offline installs without a letter can't be reached.
fn sam_path(install: &WindowsInstall) -> Option<PathBuf> {
    let letter = install.drive_letter?;
    Some(PathBuf::from(format!(
        r"{letter}:\Windows\System32\config\SAM"
    )))
}

/// Read the built-in Administrator's current state on `install`. Cheap enough
/// to run when the picker's selection changes (one PowerShell call live, one
/// hive load/unload offline).
pub fn query_state(disks: &[DiskInfo], install: &WindowsInstall) -> Result<AdminState> {
    if system_disk_index(disks) == Some(install.disk_index) {
        query_state_live()
    } else {
        query_state_offline(install)
    }
}

/// Set the built-in Administrator on `install` to `enable`. Idempotent: setting
/// an already-matching state is a no-op success. On the live path, enabling
/// also blanks the password (best-effort). Offline, the password is untouched.
pub fn execute_set(
    disks: &[DiskInfo],
    install: &WindowsInstall,
    enable: bool,
) -> Result<ToggleReport> {
    if system_disk_index(disks) == Some(install.disk_index) {
        execute_set_live(enable)
    } else {
        execute_set_offline(install, enable)
    }
}

// ---- live (running system) -------------------------------------------------

/// `powershell -Command <script>`, returning trimmed stdout on success.
fn run_ps(script: &str) -> Result<String> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| PhoenixError::Other(format!("failed to launch PowerShell: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(PhoenixError::Other(format!(
            "PowerShell failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn query_state_live() -> Result<AdminState> {
    // RID 500 is the built-in Administrator; match on the SID suffix so a rename
    // doesn't hide it. Emit SID, enabled-as-0/1 (locale-proof), and name.
    let script = "$u = Get-LocalUser | Where-Object { $_.SID.Value -like '*-500' } | \
                  Select-Object -First 1; \
                  if ($u) { \"$($u.SID.Value)`t$([int][bool]$u.Enabled)`t$($u.Name)\" } \
                  else { 'MISSING' }";
    let out = run_ps(script)?;
    if out == "MISSING" || out.is_empty() {
        return Err(PhoenixError::Other(
            "no built-in Administrator account found on the running system".into(),
        ));
    }
    let mut parts = out.splitn(3, '\t');
    let sid = parts.next().unwrap_or_default().to_string();
    let enabled = parts.next().map(|s| s.trim() == "1").unwrap_or(false);
    let account_name = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Administrator".to_string());
    Ok(AdminState {
        enabled,
        account_name,
        is_live: true,
        sid: Some(sid),
    })
}

fn execute_set_live(enable: bool) -> Result<ToggleReport> {
    let state = query_state_live()?;
    let sid = state
        .sid
        .clone()
        .ok_or_else(|| PhoenixError::Other("could not resolve the Administrator SID".into()))?;
    let name = &state.account_name;

    if !enable {
        run_ps(&format!(
            "$ErrorActionPreference='Stop'; Disable-LocalUser -SID '{sid}'; 'OK'"
        ))?;
        info!(target: "phoenix_restore::winadmin", live = true, "disabled built-in Administrator");
        return Ok(ToggleReport {
            actions: vec![format!("Disabled the built-in Administrator ({name})")],
            enabled_now: false,
        });
    }

    // Enable first so a later password-policy refusal can't undo it.
    run_ps(&format!(
        "$ErrorActionPreference='Stop'; Enable-LocalUser -SID '{sid}'; 'OK'"
    ))?;
    let mut actions = vec![format!("Enabled the built-in Administrator ({name})")];
    // Best-effort blank password. A complexity policy can refuse an empty one;
    // that leaves the account enabled with its existing password, which we
    // report rather than treat as a failure of the whole operation.
    let blank = run_ps(&format!(
        "$ErrorActionPreference='Stop'; $s = New-Object System.Security.SecureString; \
         Set-LocalUser -SID '{sid}' -Password $s; 'OK'"
    ));
    match blank {
        Ok(_) => actions.push("Cleared its password (now blank)".into()),
        Err(e) => {
            warn!(target: "phoenix_restore::winadmin", "blank password refused: {e}");
            actions.push(format!(
                "Could not blank the password ({e}) — the existing password is unchanged"
            ));
        }
    }
    info!(target: "phoenix_restore::winadmin", live = true, "enabled built-in Administrator");
    Ok(ToggleReport {
        actions,
        enabled_now: true,
    })
}

// ---- offline (attached, non-running) --------------------------------------

fn query_state_offline(install: &WindowsInstall) -> Result<AdminState> {
    let sam = sam_path(install).ok_or_else(|| {
        PhoenixError::Other(
            "the selected Windows volume has no drive letter — assign one (Disk Management) so \
             its registry can be reached, then try again"
                .into(),
        )
    })?;
    if !sam.exists() {
        return Err(PhoenixError::Other(format!(
            "no SAM registry hive at {} — the volume may not be a Windows system partition",
            sam.display()
        )));
    }
    let enabled = with_admin_user_key(&sam, KEY_READ, |hkey| {
        let f = query_binary_value(hkey, "F")?;
        Ok(!acb_disabled(&f)?)
    })?;
    Ok(AdminState {
        enabled,
        account_name: "Administrator".to_string(),
        is_live: false,
        sid: None,
    })
}

fn execute_set_offline(install: &WindowsInstall, enable: bool) -> Result<ToggleReport> {
    let sam = sam_path(install).ok_or_else(|| {
        PhoenixError::Other("the selected Windows volume has no drive letter".into())
    })?;
    with_admin_user_key(&sam, KEY_READ | KEY_SET_VALUE, |hkey| {
        let mut f = query_binary_value(hkey, "F")?;
        acb_check_len(&f)?;
        let flags = u16::from_le_bytes([f[F_ACB_OFFSET], f[F_ACB_OFFSET + 1]]);
        // Idempotent: only touch the hive when the bit actually changes.
        if let Some(new_flags) = acb_apply(flags, enable) {
            let b = new_flags.to_le_bytes();
            f[F_ACB_OFFSET] = b[0];
            f[F_ACB_OFFSET + 1] = b[1];
            set_binary_value(hkey, "F", &f)?;
        }
        Ok(())
    })?;
    info!(target: "phoenix_restore::winadmin", live = false, enable, "set built-in Administrator state (offline)");
    let mut actions = vec![format!(
        "{} the built-in Administrator (Disk {}, Partition {})",
        if enable { "Enabled" } else { "Disabled" },
        install.disk_index,
        install.partition_index
    )];
    if enable {
        actions.push(
            "Kept its existing password (the built-in Administrator is normally blank)".into(),
        );
    }
    Ok(ToggleReport {
        actions,
        enabled_now: enable,
    })
}

/// Error if an `F` value is too short to hold the account-control flags, rather
/// than reading an out-of-range byte.
fn acb_check_len(f: &[u8]) -> Result<()> {
    if f.len() < F_ACB_OFFSET + 2 {
        return Err(PhoenixError::Other(format!(
            "SAM account record is too short ({} bytes) to read its state safely",
            f.len()
        )));
    }
    Ok(())
}

/// Read the 2-byte account-control flags from an `F` value and report whether
/// `ACB_DISABLED` is set.
fn acb_disabled(f: &[u8]) -> Result<bool> {
    acb_check_len(f)?;
    let flags = u16::from_le_bytes([f[F_ACB_OFFSET], f[F_ACB_OFFSET + 1]]);
    Ok(flags & ACB_DISABLED != 0)
}

/// New account-control flags to reach target state `enable`, or `None` when the
/// account is already in that state (no write needed). Only the `ACB_DISABLED`
/// bit is ever touched; all other flags are preserved.
fn acb_apply(flags: u16, enable: bool) -> Option<u16> {
    let disabled = flags & ACB_DISABLED != 0;
    // A change is needed exactly when the current disabled-state equals the
    // desired enabled-state.
    if disabled != enable {
        return None;
    }
    Some(if enable {
        flags & !ACB_DISABLED
    } else {
        flags | ACB_DISABLED
    })
}

/// Load the offline SAM hive, run `f` against the RID-500 user key, then always
/// unload the hive (even if `f` fails). Requires `SeBackupPrivilege` /
/// `SeRestorePrivilege`, which are enabled here (the process is elevated and
/// holds them; they're just not on in the token by default).
fn with_admin_user_key<T>(
    sam: &std::path::Path,
    access: u32,
    f: impl FnOnce(HKEY) -> Result<T>,
) -> Result<T> {
    enable_privilege("SeBackupPrivilege")?;
    enable_privilege("SeRestorePrivilege")?;

    let wmount = wide(SAM_MOUNT);
    let wfile = wide(&sam.to_string_lossy());
    // Best-effort unload of a hive left mounted by an earlier interrupted run,
    // so a stale mount doesn't turn every subsequent attempt into ERROR_BUSY.
    unsafe { RegUnLoadKeyW(HKEY_LOCAL_MACHINE, wmount.as_ptr()) };

    let rc = unsafe { RegLoadKeyW(HKEY_LOCAL_MACHINE, wmount.as_ptr(), wfile.as_ptr()) };
    if rc != ERROR_SUCCESS {
        return Err(PhoenixError::Other(format!(
            "could not load the offline SAM hive (Win32 error {rc}) — is the volume in use?"
        )));
    }

    let result = (|| {
        let wsub = wide(RID500_SUBKEY);
        let mut hkey: HKEY = ptr::null_mut();
        let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wsub.as_ptr(), 0, access, &mut hkey) };
        if rc != ERROR_SUCCESS {
            return Err(PhoenixError::Other(format!(
                "could not open the built-in Administrator account in the SAM (Win32 error {rc})"
            )));
        }
        let r = f(hkey);
        unsafe { RegCloseKey(hkey) };
        r
    })();

    let rc = unsafe { RegUnLoadKeyW(HKEY_LOCAL_MACHINE, wmount.as_ptr()) };
    if rc != ERROR_SUCCESS {
        warn!(target: "phoenix_restore::winadmin", "RegUnLoadKey failed (Win32 error {rc}); hive may remain mounted at HKLM\\{SAM_MOUNT}");
    }
    result
}

/// Read a REG_BINARY value into a `Vec<u8>` (two-call size-then-data).
fn query_binary_value(hkey: HKEY, name: &str) -> Result<Vec<u8>> {
    let wname = wide(name);
    let mut ty: u32 = 0;
    let mut len: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            wname.as_ptr(),
            ptr::null(),
            &mut ty,
            ptr::null_mut(),
            &mut len,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(PhoenixError::Other(format!(
            "could not read the '{name}' value (Win32 error {rc})"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            wname.as_ptr(),
            ptr::null(),
            &mut ty,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(PhoenixError::Other(format!(
            "could not read the '{name}' value (Win32 error {rc})"
        )));
    }
    buf.truncate(len as usize);
    Ok(buf)
}

/// Write a REG_BINARY value.
fn set_binary_value(hkey: HKEY, name: &str, data: &[u8]) -> Result<()> {
    let wname = wide(name);
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            wname.as_ptr(),
            0,
            REG_BINARY,
            data.as_ptr(),
            data.len() as u32,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(PhoenixError::Other(format!(
            "could not write the '{name}' value (Win32 error {rc})"
        )));
    }
    Ok(())
}

/// Enable one named privilege in the current process token. A no-op if already
/// enabled; errors if the token doesn't hold the privilege at all.
fn enable_privilege(name: &str) -> Result<()> {
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) == 0 {
            return Err(PhoenixError::Other(format!(
                "OpenProcessToken failed (Win32 error {})",
                GetLastError()
            )));
        }
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        let wname = wide(name);
        let ok = LookupPrivilegeValueW(ptr::null(), wname.as_ptr(), &mut luid);
        if ok == 0 {
            let e = GetLastError();
            CloseHandle(token);
            return Err(PhoenixError::Other(format!(
                "LookupPrivilegeValue({name}) failed (Win32 error {e})"
            )));
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let ok = AdjustTokenPrivileges(token, 0, &tp, 0, ptr::null_mut(), ptr::null_mut());
        let err = GetLastError();
        CloseHandle(token);
        if ok == 0 {
            return Err(PhoenixError::Other(format!(
                "AdjustTokenPrivileges({name}) failed (Win32 error {err})"
            )));
        }
        // AdjustTokenPrivileges "succeeds" even when it couldn't assign the
        // privilege — that surfaces as ERROR_NOT_ALL_ASSIGNED on GetLastError.
        if err == ERROR_NOT_ALL_ASSIGNED {
            return Err(PhoenixError::Other(format!(
                "this process does not hold {name} — run as administrator"
            )));
        }
    }
    Ok(())
}

/// UTF-16, null-terminated, for the wide Win32 APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acb_bit_reads_disabled_and_enabled() {
        let mut f = vec![0u8; 0x50];
        // disabled bit set
        f[F_ACB_OFFSET] = 0x11; // 0x0011 has ACB_DISABLED (bit 0) set
        assert!(acb_disabled(&f).unwrap());
        // clear it
        f[F_ACB_OFFSET] = 0x10; // 0x0010, bit 0 clear
        assert!(!acb_disabled(&f).unwrap());
    }

    #[test]
    fn short_f_record_is_rejected_not_guessed() {
        let f = vec![0u8; 0x10];
        assert!(acb_disabled(&f).is_err());
    }

    #[test]
    fn acb_apply_writes_only_when_state_must_change() {
        // ACB_DISABLED set (0x0011 = NORMAL_ACCOUNT | DISABLED), other bits kept.
        let disabled = 0x0011u16;
        // enabled (0x0010): DISABLED clear, other bits kept.
        let enabled = 0x0010u16;

        // Enable a disabled account → clears the bit, preserves 0x0010.
        assert_eq!(acb_apply(disabled, true), Some(0x0010));
        // Disable an enabled account → sets the bit, preserves 0x0010.
        assert_eq!(acb_apply(enabled, false), Some(0x0011));
        // Already in the target state → no write.
        assert_eq!(acb_apply(enabled, true), None);
        assert_eq!(acb_apply(disabled, false), None);
    }
}
