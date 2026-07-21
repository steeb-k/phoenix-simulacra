//! Downloading the bundled QEMU into the app's private `qemu\` folder.
//!
//! The installer normally fetches this, but it is skippable — an offline
//! install, a firewalled machine, or simply unticking the task — so the
//! Virtualize page offers the same download. Same payload, same URL, same
//! pinned hash.
//!
//! **Pin-driven, not latest-driven.** This does not ask upstream what the
//! newest QEMU is. It knows the one version this app was validated against and
//! fetches exactly that. QEMU version changes alter the boot recipe — the
//! clipboard support that only exists in 11.1+, the WHPX behaviour, the device
//! set — so moving to a new build is an app-release decision made after
//! testing, not something to drift into in the background. Bumping the pin
//! here (and in `installer/simulacra.iss`) is how a new QEMU ships.
//!
//! Payloads live in the separate `phoenix-simulacra-deps` repository, not the
//! binaries repo: release assets there would sit alongside app releases and
//! read as one, and the in-app updater reads that repo's "latest release".
//!
//! Keep [`PAYLOAD_SHA256`] in step with `scripts/build-qemu-payload.ps1`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use sha2::{Digest, Sha256};

/// The QEMU build this app is validated against. Reported by `--version`, and
/// deliberately NOT a tag: Windows builds are branch snapshots, and this one
/// (11.0.50) is the 11.1 development tree. Clipboard needs 11.1+.
pub const PAYLOAD_VERSION: &str = "11.0.50";
const PAYLOAD_URL: &str = "https://github.com/steeb-k/phoenix-simulacra-deps/releases/download/qemu-payload-11.0.50/qemu-x86_64-11.0.50-20260501-win64.zip";
const PAYLOAD_SHA256: &str = "c86f19d18e0b479922ea01f2a6eb91952de5ccc543960d318f4ddadf13590c8c";
/// Roughly, for the UI to set expectations before the first progress event.
pub const PAYLOAD_MIB: u64 = 84;

/// Events posted by the download worker.
pub enum DlEvent {
    Progress {
        downloaded: u64,
        total: u64,
    },
    /// Unpacking — no byte counter, but it takes long enough to say so.
    Extracting,
    Done,
    Failed(String),
}

/// Where the bundled QEMU lives: a private folder beside the application, the
/// same place the installer puts it and the first place discovery looks.
pub fn install_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("qemu"))
}

/// True when a bundled QEMU is already unpacked here.
pub fn installed() -> bool {
    install_dir().is_some_and(|d| d.join("qemu-system-x86_64.exe").is_file())
}

/// Fetch and unpack the payload in the background.
pub fn spawn_download(tx: Sender<DlEvent>) {
    std::thread::spawn(move || {
        let _ = tx.send(match run(&tx) {
            Ok(()) => DlEvent::Done,
            Err(m) => DlEvent::Failed(m),
        });
    });
}

fn run(tx: &Sender<DlEvent>) -> Result<(), String> {
    let dir = install_dir().ok_or("cannot locate the application folder")?;
    let zip = dir.with_extension("zip.part");
    if let Some(parent) = zip.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot write next to the app: {e}"))?;
    }

    let digest = download(tx, &zip)?;
    // Verified before unpacking, not after: an unpacked bad payload is a
    // scattered mess to clean up, and this is an executable we are about to
    // hand a disk image to.
    if !digest.eq_ignore_ascii_case(PAYLOAD_SHA256) {
        let _ = std::fs::remove_file(&zip);
        return Err(format!(
            "downloaded QEMU failed its checksum (expected {PAYLOAD_SHA256}, got {digest}) — discarded"
        ));
    }

    let _ = tx.send(DlEvent::Extracting);
    // Replace wholesale: a half-populated folder from an interrupted run must
    // not be mistaken for a working install.
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("cannot replace {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    extract(&zip, &dir)?;
    let _ = std::fs::remove_file(&zip);

    if !installed() {
        return Err("unpacked QEMU is missing qemu-system-x86_64.exe".into());
    }
    tracing::info!(
        dir = %dir.display(),
        version = PAYLOAD_VERSION,
        "bundled QEMU installed"
    );
    Ok(())
}

/// Stream to `dest`, reporting progress, returning the payload's SHA-256.
fn download(tx: &Sender<DlEvent>, dest: &std::path::Path) -> Result<String, String> {
    let agent = crate::updater::agent();
    let resp = agent
        .get(PAYLOAD_URL)
        .set("User-Agent", crate::updater::USER_AGENT)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut file =
        std::fs::File::create(dest).map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_report: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("cannot write QEMU download: {e}"))?;
        downloaded += n as u64;
        // At most ~one event per MB, so the channel stays quiet.
        if downloaded - last_report >= 1_000_000 {
            last_report = downloaded;
            let _ = tx.send(DlEvent::Progress { downloaded, total });
        }
    }
    drop(file);

    if total > 0 && downloaded != total {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "download truncated ({downloaded} of {total} bytes) — discarded"
        ));
    }
    Ok(phoenix_core::hash::hex_encode(&hasher.finalize()))
}

/// Unpack with the inbox `tar.exe` (bsdtar, present since Windows 10 1803),
/// which reads zip archives and is far faster than PowerShell's
/// `Expand-Archive` on a tree this size. Avoids taking a zip dependency for
/// one call.
fn extract(zip: &std::path::Path, dir: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    /// tar is a console app; never flash a console from the windowed GUI.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let tar = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32")
        .join("tar.exe");
    let out = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tar.display()))?;
    if !out.status.success() {
        return Err(format!(
            "unpacking QEMU failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}
