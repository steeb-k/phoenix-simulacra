//! Self-update: check the public binaries repo for a newer signed installer,
//! download and verify it in the background, and hand it to the on-close hook
//! to run silently.
//!
//! The feed is the GitHub Releases API of `steeb-k/phoenix-simulacra-binaries`
//! (the code repo is private; only the built installers are published there).
//! A release ships `Simulacra-Setup-<ver>.exe` and a matching `.sha256`. Before
//! anything is staged we require BOTH: the SHA-256 must match the published
//! value AND the installer's Authenticode signature must verify (`WinVerifyTrust`).
//! A file that fails either check is deleted and never run.
//!
//! Threading mirrors [`crate::job`]: one worker thread runs the whole
//! check -> download -> verify chain and posts [`UpdateEvent`]s down an
//! `mpsc` channel that the egui `update()` loop polls once per frame, so the
//! UI thread never blocks. The compiled-in GUI version
//! ([`crate::version::BUILD_INFO`]) is the comparison baseline — independent of
//! the CLI's `--version`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// The public binaries repo's "latest release" endpoint. The owner/repo are
/// load-bearing; keep them exact.
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/steeb-k/phoenix-simulacra-binaries/releases/latest";

/// GitHub rejects API requests without a `User-Agent`. Identify ourselves with
/// the running version so the request log is legible.
pub(crate) const USER_AGENT: &str = concat!("PhoenixSimulacra/", env!("CARGO_PKG_VERSION"));

/// How the check was started, so the UI can decide how loud to be about the
/// outcome (auto checks stay silent on failure; manual ones report it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckMode {
    /// User clicked "Check for updates" — surface every outcome.
    Manual,
    /// Throttled startup check — stay silent unless an update is actually staged.
    SilentAuto,
}

/// The file the portable ZIP ships beside the exes to say what it is. The ZIP
/// and the installer carry the *same* binaries, so this is the only thing that
/// can tell them apart at runtime.
const PORTABLE_MARKER: &str = "portable.marker";

/// Is this the portable bundle rather than an installed copy?
///
/// A portable build must never update itself: its installer would install a
/// *second*, installed copy into Program Files and leave the extracted folder
/// the user is running from untouched — and the environment it exists for
/// (Windows PE, off a USB stick) is the worst place to spend a download and an
/// install on that. So the check never runs on its own here, nothing is ever
/// staged, and nothing is ever applied on close; the About page's manual check
/// still reports what's out there ([`Depth::Report`]).
pub fn is_portable() -> bool {
    std::env::current_exe()
        .map(|exe| exe.with_file_name(PORTABLE_MARKER).is_file())
        .unwrap_or(false)
}

/// How far a check goes once it finds a newer release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Download, verify, and stage the installer to apply on close.
    Stage,
    /// Say a new version exists and stop — download nothing. Portable builds
    /// only ([`is_portable`]).
    Report,
}

/// A verified installer waiting to be applied on exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    pub version: String,
    pub installer: PathBuf,
}

/// Progress and terminal outcomes streamed from the worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// Bytes downloaded so far (`total` is 0 when the server sends no length).
    Progress { downloaded: u64, total: u64 },
    /// Already on the latest version — nothing to do.
    UpToDate,
    /// A newer release exists and was deliberately not downloaded — the
    /// terminal event of a [`Depth::Report`] check.
    Available { version: String },
    /// A newer installer was downloaded and fully verified.
    Ready(StagedUpdate),
    /// Couldn't reach the update server (offline / DNS / timeout). Silent for
    /// auto checks; a gentle message for manual ones.
    NoNetwork,
    /// Any other failure (bad HTTP status, checksum/signature mismatch, …).
    Failed(String),
}

impl UpdateEvent {
    fn is_terminal(&self) -> bool {
        !matches!(self, UpdateEvent::Progress { .. })
    }
}

/// One in-flight check. Poll it once per frame (like [`crate::job::BackgroundJob`]).
pub struct UpdateCheck {
    mode: CheckMode,
    rx: mpsc::Receiver<UpdateEvent>,
    done: bool,
}

impl UpdateCheck {
    /// Spawn the worker for a fresh check against `local_version` (the running
    /// GUI's `CARGO_PKG_VERSION`), going as far as `depth`.
    pub fn spawn(mode: CheckMode, local_version: String, depth: Depth) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = run_check(&local_version, depth, &tx);
            // The terminal event is always sent last; progress events (if any)
            // were streamed by `run_check` as the download ran.
            let _ = tx.send(outcome);
        });
        Self {
            mode,
            rx,
            done: false,
        }
    }

    pub fn mode(&self) -> CheckMode {
        self.mode
    }

    /// Drain everything the worker has produced since the last call and return
    /// the newest event seen (progress or terminal). Terminal events latch
    /// `done`, after which this returns `None`. Safe to call every frame.
    pub fn poll(&mut self) -> Option<UpdateEvent> {
        if self.done {
            return None;
        }
        let mut latest = None;
        loop {
            match self.rx.try_recv() {
                Ok(ev) => {
                    if ev.is_terminal() {
                        self.done = true;
                        latest = Some(ev);
                        break;
                    }
                    latest = Some(ev);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker dropped its sender without a terminal event — treat
                    // as a failure so the check can't hang "in progress" forever.
                    self.done = true;
                    latest = Some(UpdateEvent::Failed(
                        "update check ended unexpectedly".to_string(),
                    ));
                    break;
                }
            }
        }
        latest
    }
}

// --- Version comparison -------------------------------------------------------

/// Parse a `vX.Y.Z` (or `X.Y.Z`) tag into a comparable tuple, ignoring any
/// `-pre`/`+build` suffix. `None` on anything unparseable.
fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    // Allow a two-part "X.Y" tag; treat the missing patch as 0.
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Is `remote` strictly newer than `local`? False (fail-safe: never "update")
/// if either side won't parse.
fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_ver(remote), parse_ver(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

// --- GitHub release model -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Reasons the initial release fetch can fail, split so an auto check can stay
/// silent when the machine is merely offline.
enum FetchErr {
    NoNetwork,
    Other(String),
}

fn classify(e: ureq::Error) -> FetchErr {
    match e {
        // 403 unauthenticated is almost always the 60-req/hr rate limit.
        ureq::Error::Status(403, _) => {
            FetchErr::Other("update service is busy (rate limited); try again later".to_string())
        }
        ureq::Error::Status(code, _) => {
            FetchErr::Other(format!("update server returned HTTP {code}"))
        }
        // Transport errors are DNS/connect/timeout — treat as "no network".
        ureq::Error::Transport(_) => FetchErr::NoNetwork,
    }
}

pub(crate) fn agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30));
    // Use the OS-native TLS stack (SChannel on Windows) so we trust the machine
    // root store and pull in no C-compiled crypto. If the connector can't be
    // built for some reason, fall through with no TLS — HTTPS then simply fails
    // and the check reports NoNetwork rather than panicking.
    if let Ok(connector) = native_tls::TlsConnector::new() {
        builder = builder.tls_connector(std::sync::Arc::new(connector));
    }
    builder.build()
}

// --- The worker chain ---------------------------------------------------------

fn run_check(local_version: &str, depth: Depth, tx: &mpsc::Sender<UpdateEvent>) -> UpdateEvent {
    let agent = agent();

    let release = match fetch_latest(&agent) {
        Ok(r) => r,
        Err(FetchErr::NoNetwork) => return UpdateEvent::NoNetwork,
        Err(FetchErr::Other(m)) => return UpdateEvent::Failed(m),
    };

    if !is_newer(&release.tag_name, local_version) {
        info!(target: "phoenix_gui::updater", latest = %release.tag_name, local = %local_version, "up to date");
        return UpdateEvent::UpToDate;
    }
    let version = normalize_version(&release.tag_name);
    info!(target: "phoenix_gui::updater", %version, "newer release available");

    // A portable build stops here, before a byte of installer is fetched. It
    // deliberately doesn't care whether the release ships an installer asset:
    // it isn't going to run one, and the user is being pointed at the download
    // page (which carries the ZIP too).
    if depth == Depth::Report {
        return UpdateEvent::Available { version };
    }

    // Locate the installer asset and its checksum sibling.
    let installer = match release.assets.iter().find(|a| {
        let n = a.name.to_ascii_lowercase();
        n.starts_with("simulacra-setup-") && n.ends_with(".exe")
    }) {
        Some(a) => a,
        None => return UpdateEvent::Failed("release has no installer asset".to_string()),
    };
    let sha_name = format!("{}.sha256", installer.name);
    let sha_asset = match release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(&sha_name))
    {
        Some(a) => a,
        // The user chose SHA-256 AND Authenticode — a release with no published
        // checksum is untrusted; refuse rather than fall back to signature-only.
        None => {
            return UpdateEvent::Failed(
                "release is missing its checksum — not installing".to_string(),
            )
        }
    };

    let expected_sha = match fetch_expected_sha(&agent, &sha_asset.browser_download_url) {
        Ok(s) => s,
        Err(FetchErr::NoNetwork) => return UpdateEvent::NoNetwork,
        Err(FetchErr::Other(m)) => return UpdateEvent::Failed(m),
    };

    match download_and_verify(
        &agent,
        &installer.browser_download_url,
        &expected_sha,
        &version,
        tx,
    ) {
        Ok(staged) => UpdateEvent::Ready(staged),
        Err(m) => UpdateEvent::Failed(m),
    }
}

fn fetch_latest(agent: &ureq::Agent) -> Result<GhRelease, FetchErr> {
    let body = agent
        .get(LATEST_RELEASE_URL)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(classify)?
        .into_string()
        .map_err(|e| FetchErr::Other(format!("could not read update response: {e}")))?;
    serde_json::from_str::<GhRelease>(&body)
        .map_err(|e| FetchErr::Other(format!("could not parse update response: {e}")))
}

fn fetch_expected_sha(agent: &ureq::Agent, url: &str) -> Result<String, FetchErr> {
    let text = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(classify)?
        .into_string()
        .map_err(|e| FetchErr::Other(format!("could not read checksum: {e}")))?;
    // Format is "<hex>  <filename>"; take the first whitespace-delimited token.
    let hash = text.split_whitespace().next().unwrap_or("");
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(FetchErr::Other("published checksum is malformed".to_string()));
    }
    Ok(hash.to_string())
}

fn download_and_verify(
    agent: &ureq::Agent,
    url: &str,
    expected_sha: &str,
    version: &str,
    tx: &mpsc::Sender<UpdateEvent>,
) -> Result<StagedUpdate, String> {
    let dir = phoenix_core::appdata::updates_dir();
    let final_name = format!("Simulacra-Setup-{version}.exe");
    let final_path = dir.join(&final_name);
    let part_path = dir.join(format!("{final_name}.part"));

    let resp = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match classify(e) {
            FetchErr::NoNetwork => "download failed — connection lost".to_string(),
            FetchErr::Other(m) => m,
        })?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part_path)
        .map_err(|e| format!("cannot create staging file: {e}"))?;
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
        file.write_all(&buf[..n])
            .map_err(|e| format!("cannot write update: {e}"))?;
        hasher.update(&buf[..n]);
        downloaded += n as u64;
        // Report at most ~once per MB to keep the channel quiet.
        if downloaded - last_report >= 1_000_000 {
            last_report = downloaded;
            let _ = tx.send(UpdateEvent::Progress { downloaded, total });
        }
    }
    drop(file);

    // 1) SHA-256 must match the published value.
    let got = phoenix_core::hash::hex_encode(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_sha) {
        let _ = std::fs::remove_file(&part_path);
        warn!(target: "phoenix_gui::updater", %version, "checksum mismatch; discarded");
        return Err("update failed verification (checksum mismatch) — discarded".to_string());
    }

    // 2) Authenticode signature must verify.
    if let Err(e) = verify_authenticode(&part_path) {
        let _ = std::fs::remove_file(&part_path);
        warn!(target: "phoenix_gui::updater", %version, error = %e, "signature check failed; discarded");
        return Err(e);
    }

    // Only a fully verified file gets the runnable (non-`.part`) name.
    std::fs::rename(&part_path, &final_path)
        .map_err(|e| format!("cannot finalize update: {e}"))?;
    info!(target: "phoenix_gui::updater", %version, path = %final_path.display(), "update staged");
    Ok(StagedUpdate {
        version: version.to_string(),
        installer: final_path,
    })
}

/// A bare `x.y.z` string for display and file naming (strips a leading `v`).
fn normalize_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

// --- Authenticode via WinVerifyTrust -----------------------------------------

#[cfg(windows)]
fn verify_authenticode(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut file_info: WINTRUST_FILE_INFO = unsafe { std::mem::zeroed() };
    file_info.cbStruct = std::mem::size_of::<WINTRUST_FILE_INFO>() as u32;
    file_info.pcwszFilePath = wide.as_ptr();

    let mut wtd: WINTRUST_DATA = unsafe { std::mem::zeroed() };
    wtd.cbStruct = std::mem::size_of::<WINTRUST_DATA>() as u32;
    wtd.dwUIChoice = WTD_UI_NONE;
    wtd.fdwRevocationChecks = WTD_REVOKE_NONE;
    wtd.dwUnionChoice = WTD_CHOICE_FILE;
    wtd.dwStateAction = WTD_STATEACTION_VERIFY;
    wtd.Anonymous.pFile = &mut file_info;

    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // NULL hwnd: no UI (paired with WTD_UI_NONE).
    let status = unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            &mut wtd as *mut _ as *mut core::ffi::c_void,
        )
    };

    // Always release the state data, whatever the verdict.
    wtd.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            &mut wtd as *mut _ as *mut core::ffi::c_void,
        );
    }

    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "update signature could not be verified (0x{:08X}) — discarded",
            status as u32
        ))
    }
}

#[cfg(not(windows))]
fn verify_authenticode(_path: &Path) -> Result<(), String> {
    Err("Authenticode verification is only available on Windows".to_string())
}

// --- Applying a staged update -------------------------------------------------

/// Launch the staged installer detached and silent. Called from `on_exit` after
/// the app has done its own teardown: the child outlives the exiting process
/// (so the running exe no longer blocks the in-place upgrade) and the
/// single-instance mutex has been released by the time it relaunches.
#[cfg(windows)]
pub fn launch_installer(path: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(path)
        .args(["/VERYSILENT", "/NORESTART", "/SUPPRESSMSGBOXES"])
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
}

#[cfg(not(windows))]
pub fn launch_installer(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Same silent install as [`launch_installer`], but relaunch `app` once the
/// installer has finished — the "Restart to update" link's exit path.
///
/// `cmd.exe` does the waiting because nothing of ours can: this process is on
/// its way out, and a helper spawned from our own install directory would be
/// the very file the installer needs to replace. `DETACHED_PROCESS` leaves it
/// with no console, so the install runs with nothing on screen. The relaunched
/// app inherits this process's elevated token, so the user is not asked to
/// consent a second time.
///
/// The chain is `&`, not `&&`: the user asked for the app back, so a failed or
/// refused install still returns them to the version they were running, which
/// comes up still holding its staged update and still saying so in the banner.
#[cfg(windows)]
pub fn launch_installer_and_restart(installer: &Path, app: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    // `cmd /c` does not parse its command line the way the C runtime does, so
    // it is handed over verbatim with `raw_arg` rather than through Rust's
    // arg escaping. The outer quote pair is the one cmd strips off (the `&`
    // puts it on that branch of its documented rule), leaving the two quoted
    // paths intact. `start ""` (empty title) hands the app off so cmd exits
    // instead of lingering as its parent.
    let line = format!(
        r#"/c ""{}" /VERYSILENT /NORESTART /SUPPRESSMSGBOXES & start "" "{}"""#,
        installer.display(),
        app.display()
    );
    info!(target: "phoenix_gui::updater", installer = %installer.display(), app = %app.display(), "applying update with restart");
    std::process::Command::new("cmd.exe")
        .raw_arg(line)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|_| ())
}

#[cfg(not(windows))]
pub fn launch_installer_and_restart(_installer: &Path, _app: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Housekeeping to run once at startup: drop a staged installer that's already
/// been applied (`staged_version` <= running), and delete any leftover `.part`
/// files from an interrupted download. Persists `state` if it changed.
pub fn cleanup_stale(state: &mut phoenix_core::appdata::UpdateState, running_version: &str) {
    // A staged installer at or below the running version means the update was
    // already applied (or superseded) — forget it and remove the file.
    let stale = state
        .staged_version
        .as_deref()
        .map(|v| !is_newer(v, running_version))
        .unwrap_or(false);
    if stale {
        if let Some(p) = state.staged_installer.take() {
            let _ = std::fs::remove_file(&p);
        }
        state.staged_version = None;
        let _ = state.save();
    }

    // Sweep orphaned partial downloads.
    if let Ok(entries) = std::fs::read_dir(phoenix_core::appdata::updates_dir()) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .map(|e| e.eq_ignore_ascii_case("part"))
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_with_and_without_v() {
        assert_eq!(parse_ver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_ver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_ver("v0.2"), Some((0, 2, 0)));
        assert_eq!(parse_ver("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_ver("garbage"), None);
    }

    #[test]
    fn newer_is_strict_and_fail_safe() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0")); // equal is not newer
        assert!(!is_newer("0.1.0", "0.2.0")); // older
        assert!(!is_newer("nope", "0.1.0")); // unparseable remote -> never update
        assert!(!is_newer("0.2.0", "nope")); // unparseable local -> don't guess
    }

    #[test]
    fn normalizes_display_version() {
        assert_eq!(normalize_version("v0.2.0"), "0.2.0");
        assert_eq!(normalize_version("0.2.0"), "0.2.0");
    }
}
