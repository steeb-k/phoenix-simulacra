//! Locating an external QEMU install and its firmware.
//!
//! QEMU is not bundled yet (see `docs/VIRTUALIZATION.md` "QEMU discovery and
//! bundling" — a bundled build must be 11.1+); we detect it. The
//! runtime path always honors a user-supplied location first, so a bundled
//! copy and a user's own install can coexist.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// A resolved QEMU install: the two binaries we drive plus the firmware dir.
#[derive(Debug, Clone)]
pub struct Qemu {
    pub system: PathBuf,
    pub img: PathBuf,
    /// `share/` — holds the edk2/OVMF firmware blobs.
    pub share: PathBuf,
    pub version: String,
    /// True if `-display gtk` accepts `clipboard=on`. QEMU 11.1 turned the
    /// old `--enable-gtk-clipboard` build flag into a runtime option and
    /// fixed the hang that kept it off by default; before that, no Windows
    /// build shipped `ui/gtk-clipboard.c` at all. Probed, not inferred from
    /// the version string, since Windows builds are branch snapshots whose
    /// numbering doesn't track upstream tags.
    pub gtk_clipboard: bool,
}

impl Qemu {
    /// Discover QEMU: an explicit directory first, then `PATH`, then the stock
    /// installer locations. `explicit_dir` comes from the GUI setting or a
    /// `--qemu-dir` flag.
    pub fn discover(explicit_dir: Option<&Path>) -> Result<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(d) = explicit_dir {
            candidates.push(d.to_path_buf());
        }
        if let Some(d) = which_dir("qemu-system-x86_64") {
            candidates.push(d);
        }
        for p in [
            r"C:\Program Files\qemu",
            r"C:\Program Files (x86)\qemu",
            r"C:\msys64\mingw64\bin",
        ] {
            candidates.push(PathBuf::from(p));
        }

        for dir in candidates {
            if let Some(q) = Self::try_dir(&dir) {
                return Ok(q);
            }
        }
        bail!(
            "QEMU not found. Install it (https://www.qemu.org/download/#windows) \
             or point the app at an existing install."
        )
    }

    fn try_dir(dir: &Path) -> Option<Self> {
        let system = dir.join("qemu-system-x86_64.exe");
        let img = dir.join("qemu-img.exe");
        if !system.exists() || !img.exists() {
            return None;
        }
        let version = query_version(&system).ok()?;
        // `share` sits next to the binaries in the stock installer; for a
        // PATH/msys layout it may be one level up under share/qemu.
        let share = [dir.join("share"), dir.join("..").join("share").join("qemu")]
            .into_iter()
            .find(|p| p.join("edk2-x86_64-code.fd").exists())
            .unwrap_or_else(|| dir.join("share"));
        let gtk_clipboard = probe_gtk_clipboard(&system);
        Some(Qemu {
            system,
            img,
            share,
            version,
            gtk_clipboard,
        })
    }

    /// The x86_64 OVMF code flash (read-only master; copy it per session).
    pub fn ovmf_code(&self) -> PathBuf {
        self.share.join("edk2-x86_64-code.fd")
    }

    /// The OVMF NVRAM varstore template (copy it per session; boot order and
    /// UEFI settings persist into the copy).
    pub fn ovmf_vars_template(&self) -> PathBuf {
        self.share.join("edk2-i386-vars.fd")
    }

    /// True if both firmware blobs a UEFI guest needs are present.
    pub fn has_uefi_firmware(&self) -> bool {
        self.ovmf_code().exists() && self.ovmf_vars_template().exists()
    }
}

/// `CREATE_NO_WINDOW` — qemu-system is a console app, so spawning it from
/// the windowed GUI flashes a console unless we suppress it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A `Command` for QEMU that never flashes a console window.
fn quiet(system: &Path) -> Command {
    let mut cmd = Command::new(system);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn query_version(system: &Path) -> Result<String> {
    let out = quiet(system)
        .arg("--version")
        .output()
        .with_context(|| format!("run {} --version", system.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().next().unwrap_or("").trim().to_string())
}

/// Does this build's GTK display accept `clipboard=on`?
///
/// `-machine help` exits during option parsing, so this is a cheap parse
/// check that never starts a machine or opens a window: a build without the
/// option rejects it with "Parameter 'clipboard' is unexpected" and a
/// non-zero status, exactly as it would any unknown key.
fn probe_gtk_clipboard(system: &Path) -> bool {
    quiet(system)
        .args(["-display", "gtk,clipboard=on", "-machine", "help"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Directory containing `name(.exe)` on `PATH`, if any. A tiny `which` so we
/// don't take a dependency for one lookup.
fn which_dir(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.join(format!("{name}.exe")).exists() || dir.join(name).exists() {
            return Some(dir);
        }
    }
    None
}
