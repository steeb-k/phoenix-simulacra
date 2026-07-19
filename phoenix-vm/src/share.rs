//! Host↔guest file exchange over the HOST's native SMB server.
//!
//! No Samba, no VVFAT, no guest drivers: Windows already runs an SMB server on
//! port 445, and a slirp (user-mode NAT) guest reaches the host's loopback at
//! the gateway address `10.0.2.2`. So the whole feature is: share a folder
//! next to the image (`net share`, we're already elevated), and hand the user
//! one command to paste inside the guest:
//!
//! ```text
//! net use S: \\10.0.2.2\SimulacraShare /user:HOSTNAME\user
//! ```
//!
//! The guest prompts for the host account's password (a Microsoft-account PC
//! accepts the Microsoft-account password). Mapping can't be automatic — we
//! can't reach into the guest before it boots — which is why the GUI surfaces
//! the command with a Copy button instead.
//!
//! One share at a time (we run one VM at a time): `ensure_share` re-points the
//! fixed share name at the current image's folder, and teardown removes it.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// `net.exe` is a console app; never flash a console from the GUI.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The fixed Windows share name. One VM (and thus one share) at a time.
pub const SHARE_NAME: &str = "SimulacraShare";

/// The default shared folder: `SimulacraShare` next to the backup image.
pub fn share_dir_for_backup(backup: &Path) -> PathBuf {
    backup
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(SHARE_NAME)
}

/// Create the folder (if needed) and point the Windows share at it, granting
/// the current user full access. Needs elevation — the VM path already runs
/// elevated. An existing share of the same name is re-pointed (`net share`
/// cannot move one in place, so it is dropped and recreated).
pub fn ensure_share(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    remove_share();

    let user = std::env::var("USERNAME").context("USERNAME not set")?;
    let spec = format!("{SHARE_NAME}={}", dir.display());
    let grant = format!("/GRANT:{user},FULL");
    let out = Command::new("net")
        .args(["share", &spec, &grant])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("run net share")?;
    if !out.status.success() {
        bail!(
            "net share failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    tracing::info!("shared {} as \\\\10.0.2.2\\{SHARE_NAME}", dir.display());
    Ok(())
}

/// Drop the share (the folder and its contents stay). Best-effort — a share
/// that never existed is fine.
pub fn remove_share() {
    let _ = Command::new("net")
        .args(["share", SHARE_NAME, "/delete", "/y"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// The command the user pastes INSIDE the guest to map the share. `10.0.2.2`
/// is slirp's gateway — the host's loopback as seen from the guest.
pub fn guest_mount_command() -> String {
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "HOST".into());
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    format!(r"net use S: \\10.0.2.2\{SHARE_NAME} /user:{host}\{user}")
}
