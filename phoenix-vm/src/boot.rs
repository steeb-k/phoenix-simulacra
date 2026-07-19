//! Boot orchestration — the productized form of the boot smoke.
//!
//! Ties the pieces together: read the manifest, resolve a [`VmConfig`], open or
//! resume the [`Session`], serve the backup on demand, materialize the write
//! overlay, build the QEMU command line, launch, and tear down. Gated behind
//! `winfsp` because the serve path needs it (and libclang + WinFsp to build).

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Spawn console-subsystem children (qemu-img, qemu-system) without a console
/// window — launched from the windowed GUI they otherwise each pop one up, and
/// qemu-system's error text vanishes with it when the launch dies early. Their
/// output goes to handles we set explicitly, which work fine without a console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use anyhow::{bail, Context, Result};
use phoenix_core::container::PhnxReader;
use phoenix_mount::{create_differencing_vhdx, set_disk_offline, AttachedDisk};

use crate::config::{BootSpec, DiskSource, VmConfig};
use crate::qemu::Qemu;
use crate::serve_helper::spawn_serve;
use crate::session::{SessionManager, WriteLayer};
use crate::{now_rfc3339, HostOptions};

/// What a boot did.
#[derive(Debug, Clone)]
pub struct BootOutcome {
    pub exit_ok: bool,
    /// Size of the session overlay after teardown — how much the guest wrote.
    pub overlay_bytes: u64,
    /// True if the backing `.phnx` still passes a structural check afterward.
    pub backup_intact: bool,
    /// True if this boot resumed an existing overlay rather than creating one.
    pub resumed: bool,
    /// True if the resumed session was left dirty by a previous crashed boot.
    pub resumed_dirty: bool,
    /// The tail of QEMU's own output (the session's `qemu.log`) when it exited
    /// non-zero — the actual reason a launch died, for surfacing in the UI.
    pub qemu_log_tail: Option<String>,
}

/// Boot `backup` as a VM. Blocks until the guest window is closed (the serve
/// must outlive the guest). The session's overlay + varstore are kept for
/// resume; call [`SessionManager::discard`] to throw the session away.
#[allow(clippy::too_many_arguments)]
pub fn boot(
    backup: &Path,
    host: &HostOptions,
    write_layer: WriteLayer,
    fresh: bool,
    iso: Option<&Path>,
    boot_iso_first: bool,
    drivers_iso: Option<&Path>,
    helper_disk: Option<&Path>,
    qemu: &Qemu,
    sessions: &SessionManager,
    scratch_dir: &Path,
) -> Result<BootOutcome> {
    // --- manifest → config + session identity -------------------------------
    let reader = PhnxReader::open(backup).with_context(|| format!("open {}", backup.display()))?;
    let backup_id = reader.header.backup_id;
    let manifest = reader.manifest.clone();
    drop(reader);

    let cfg = VmConfig::from_manifest(&manifest, host)?;
    let mut session = sessions.open_or_create(backup_id, backup, write_layer, now_rfc3339())?;

    // `--fresh` throws away the kept overlay + varstore so the next boot starts
    // clean (recovery from a dirty/broken session), while keeping the session's
    // identity and created-at.
    if fresh && session.overlay_exists() {
        std::fs::remove_file(session.overlay_path()).ok();
        std::fs::remove_file(session.firmware_vars_path()).ok();
        tracing::info!("--fresh: discarded the existing session overlay");
    }

    let resuming = session.overlay_exists();
    let resumed_dirty = resuming && !session.meta().clean_shutdown;
    if resumed_dirty {
        tracing::warn!(
            "resuming a session left DIRTY by a previous crashed boot — the overlay is \
             intact but may hold a torn last write; use `--fresh` to start over if the \
             guest won't boot"
        );
    }

    // --- serve the backup on demand, IN A SEPARATE PROCESS ------------------
    // The serve MUST NOT run in this process: DetachVirtualDisk on the child
    // (below, at teardown) deadlocks against an in-process WinFsp-served parent.
    // The helper serves at the same deterministic path, so resume still works.
    std::fs::create_dir_all(scratch_dir).ok();
    let serve = spawn_serve(backup, scratch_dir).context("start the serve helper for VM boot")?;
    let parent = serve.parent_path().to_path_buf();
    let parent_str = parent
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 served parent path"))?;

    // --- firmware (UEFI: per-session writable copies) -----------------------
    let (fw_code, fw_vars) = match cfg.firmware {
        crate::config::Firmware::Uefi => {
            if !qemu.has_uefi_firmware() {
                bail!(
                    "QEMU install at {} is missing the edk2 UEFI firmware needed for this \
                     GPT backup",
                    qemu.share.display()
                );
            }
            session.ensure_uefi_firmware(&qemu.ovmf_code(), &qemu.ovmf_vars_template())?;
            (
                Some(session.firmware_code_path().display().to_string()),
                Some(session.firmware_vars_path().display().to_string()),
            )
        }
        crate::config::Firmware::Bios => (None, None),
    };

    // --- write overlay + disk source ----------------------------------------
    // `_attached` (if any) must outlive the guest: dropping it detaches the disk.
    let overlay = session.overlay_path();
    let (_attached, disk): (Option<AttachedDisk>, DiskSource) = match write_layer {
        WriteLayer::Qcow2 => {
            if !resuming {
                let out = Command::new(&qemu.img)
                    .args(["create", "-f", "qcow2", "-F", "vhdx", "-b"])
                    .arg(&parent)
                    .arg(&overlay)
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                    .context("qemu-img create qcow2 overlay")?;
                if !out.status.success() {
                    bail!(
                        "qemu-img create failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
            (
                None,
                DiskSource::File {
                    path: overlay.display().to_string(),
                    format: "qcow2".to_string(),
                },
            )
        }
        WriteLayer::Avhdx => {
            let child_str = overlay
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 overlay path"))?;
            if !resuming {
                // Fresh differencing child over the served parent. On resume we
                // reuse the existing child (it records this same parent path).
                create_differencing_vhdx(child_str, parent_str)
                    .context("create differencing session overlay")?;
            }
            let attached = AttachedDisk::attach_readwrite_opts(child_str, false)
                .context("attach session overlay read-write")?;
            let drive = attached
                .physical_drive_number()
                .context("resolve attached overlay's physical drive number")?;
            set_disk_offline(drive).context("force session overlay disk offline")?;
            (Some(attached), DiskSource::RawPhysicalDrive(drive))
        }
    };

    // --- assemble + launch --------------------------------------------------
    // A QMP control socket so `vm stop` can shut the guest down gracefully
    // (ACPI) instead of killing QEMU. The port is recorded in the session so a
    // separate process can find it.
    let qmp_port = crate::qmp::pick_free_port().ok();
    let qga_port = crate::qmp::pick_free_port().ok();
    session.mark_booting(now_rfc3339())?;
    if let Some(port) = qmp_port {
        let _ = std::fs::write(session.qmp_port_file(), port.to_string());
    }
    if let Some(port) = qga_port {
        let _ = std::fs::write(session.qga_port_file(), port.to_string());
    }
    tracing::info!(
        "booting {} ({} write layer, {}resume)",
        backup.display(),
        match write_layer {
            WriteLayer::Avhdx => "avhdx",
            WriteLayer::Qcow2 => "qcow2",
        },
        if resuming { "" } else { "no " },
    );

    // WHPX cannot reset a virtual processor in place: a guest-initiated reboot
    // (Windows Restart, exiting a PE session, a bugcheck auto-restart) surfaces
    // as "WHPX: Unexpected VP exit code 4" and QEMU exits *cleanly* instead of
    // resetting. Relaunching against the same session IS the reboot — the
    // overlay, varstore, serve, and QMP port all persist. The one-shot ISO
    // boot is consumed by the first launch (`boot_iso_now` drops to false), so
    // restarting out of a rescue ISO lands on the disk — exactly what "next
    // boot only" means.
    let mut boot_iso_now = boot_iso_first;
    let mut launches = 0u32;
    /// Reboot-loop backstop: a guest legitimately restarts a handful of times
    /// (driver installs, updates); a guest that resets endlessly should stop
    /// coming back.
    const MAX_RELAUNCHES: u32 = 32;
    /// A reset this soon after launch is a crash loop (the guest can't even
    /// reach Windows in that time), not a user-driven restart — stop.
    const MIN_UPTIME_FOR_RELAUNCH: std::time::Duration = std::time::Duration::from_secs(15);

    // QEMU's console output goes to the session's `qemu.log` (truncated per
    // launch) — from the GUI there is no console to inherit (Windows would pop
    // one up and any early error would vanish with it), and a persistent log
    // is the better story from the CLI too.
    let qemu_log_path = session.dir().join("qemu.log");
    let status = loop {
        launches += 1;
        let spec = BootSpec {
            name: format!("Phoenix Simulacra — {}", manifest.hostname),
            firmware_code: fw_code.clone(),
            firmware_vars: fw_vars.clone(),
            disk: disk.clone(),
            iso: iso.map(|p| p.display().to_string()),
            boot_iso_first: boot_iso_now,
            qmp_port,
            qga_port,
            drivers_iso: drivers_iso.map(|p| p.display().to_string()),
            helper_disk: helper_disk.map(|p| p.display().to_string()),
        };
        let args = cfg.qemu_args(&spec);

        let log_out = std::fs::File::create(&qemu_log_path)
            .with_context(|| format!("create {}", qemu_log_path.display()))?;
        let log_err = log_out.try_clone().context("clone qemu.log handle")?;
        let started = std::time::Instant::now();
        let mut child = Command::new(&qemu.system)
            .args(&args)
            .envs(host.gtk_env())
            .stdin(Stdio::null())
            .stdout(log_out)
            .stderr(log_err)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .with_context(|| format!("launch {}", qemu.system.display()))?;
        maximize_window_of(child.id(), host.dark_window);
        if !host.network {
            unplug_network(spec.qmp_port);
        }
        spawn_reset_watchdog(child.id(), qemu_log_path.clone());
        let status = child.wait().context("wait for QEMU")?;

        let log_text = std::fs::read_to_string(&qemu_log_path).unwrap_or_default();
        let guest_reset = log_text.contains("Unexpected VP exit code");
        if guest_reset
            && launches < MAX_RELAUNCHES
            && started.elapsed() >= MIN_UPTIME_FOR_RELAUNCH
        {
            tracing::info!(
                "guest restart (WHPX cannot reset in place) — relaunching to continue \
                 the session (launch {})",
                launches + 1
            );
            boot_iso_now = false;
            continue;
        }
        if !status.success() {
            tracing::warn!(
                "QEMU exited with {:?}; its output is in {}",
                status.code(),
                qemu_log_path.display()
            );
        }
        break status;
    };

    // --- teardown: detach the child, THEN stop the serve helper -------------
    // Order still matters: the child's detach reads/closes the parent, so the
    // serve helper must stay alive across the detach. But because the serve is
    // in a SEPARATE process now, this detach no longer deadlocks — its Close IRP
    // is serviced by the helper's dispatcher, not this thread.
    // Detach by DROPPING the attach (CloseHandle → async detach). Never call
    // DetachVirtualDisk explicitly here: that synchronous call deadlocks in the
    // storage driver when the parent lives on a WinFsp filesystem, regardless of
    // which process serves it (measured). Then poll until the device is really
    // gone, so stopping the serve can't race an in-flight detach.
    let drive_to_wait = _attached
        .as_ref()
        .and_then(|att| att.physical_drive_number().ok());
    drop(_attached);
    let detached = match drive_to_wait {
        Some(drive) => phoenix_mount::wait_for_device_gone(drive, std::time::Duration::from_secs(15)),
        None => true,
    };
    if !detached {
        tracing::warn!("session overlay did not detach within the timeout; stopping serve anyway");
    }
    serve.stop(); // kills the helper process — clean, it holds no attach
    let _ = std::fs::remove_file(session.qmp_port_file());
    let _ = std::fs::remove_file(session.qga_port_file());
    session.mark_clean()?;

    let overlay_bytes = std::fs::metadata(&overlay).map(|m| m.len()).unwrap_or(0);
    // Cheap integrity signal: opening a .phnx validates its footer CRC, manifest
    // and index hashes. A full `verify_all` re-reads the whole backup and does
    // not belong on the exit path of every boot.
    let backup_intact = PhnxReader::open(backup).is_ok();

    let qemu_log_tail = if status.success() {
        None
    } else {
        Some(log_tail(&qemu_log_path, 1500))
    };

    Ok(BootOutcome {
        exit_ok: status.success(),
        overlay_bytes,
        backup_intact,
        resumed: resuming,
        resumed_dirty,
        qemu_log_tail,
    })
}

/// The line QEMU prints when a guest reset hits WHPX's unsupported-reset path.
const WHPX_RESET_MARKER: &str = "Unexpected VP exit code";

/// A WHPX guest reset does not always exit QEMU: sometimes only part of the
/// VPs hit the unsupported-reset path and QEMU wedges ALIVE with scanout off
/// (the GTK window shows "no video output"), stranding the relaunch loop,
/// which only acts on process exit. Watch this launch's qemu.log for the
/// reset marker; if QEMU is still running a grace period after it appears,
/// kill it — the marker in the log then makes the relaunch loop treat the
/// exit as a guest reset and bring the session straight back up.
fn spawn_reset_watchdog(pid: u32, log_path: std::path::PathBuf) {
    const POLL: std::time::Duration = std::time::Duration::from_secs(2);
    const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

    std::thread::spawn(move || loop {
        if !is_this_qemu_alive(pid) {
            return; // exited on its own — the launch loop handles it
        }
        let logged = std::fs::read_to_string(&log_path)
            .map(|t| t.contains(WHPX_RESET_MARKER))
            .unwrap_or(false);
        if logged {
            // Give QEMU the chance to exit by itself (the common mode).
            std::thread::sleep(GRACE);
            if is_this_qemu_alive(pid) {
                tracing::warn!(
                    "guest reset wedged QEMU alive with no video output — killing it so \
                     the relaunch loop can continue the session"
                );
                kill_this_qemu(pid);
            }
            return;
        }
        std::thread::sleep(POLL);
    });
}

/// True while `pid` is a live process whose image is really QEMU — PIDs get
/// recycled, so never trust the number alone.
fn is_this_qemu_alive(pid: u32) -> bool {
    qemu_process_handle_op(pid, false)
}

/// Terminate `pid` if it is (still) really QEMU.
fn kill_this_qemu(pid: u32) {
    qemu_process_handle_op(pid, true);
}

/// Shared open-check(-terminate) on `pid`: returns whether the process exists
/// AND its image name is `qemu-system-x86_64.exe`; when `terminate` is set and
/// it matches, it is killed.
fn qemu_process_handle_op(pid: u32, terminate: bool) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    let access = if terminate {
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE
    } else {
        PROCESS_QUERY_LIMITED_INFORMATION
    };
    // SAFETY: a failed open returns null, checked before any use.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: handle is live; buf/len describe a valid writable buffer.
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_FORMAT::default(), buf.as_mut_ptr(), &mut len)
    };
    let mut matches = false;
    if ok != 0 {
        let full = String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
        matches = full.ends_with("qemu-system-x86_64.exe");
        if matches && terminate {
            // SAFETY: handle was opened with PROCESS_TERMINATE.
            unsafe { TerminateProcess(handle, 1) };
        }
    }
    // SAFETY: handle came from OpenProcess and is not used after this.
    unsafe { CloseHandle(handle) };
    matches
}

/// Maximize QEMU's window once it appears, and optionally darken its title
/// bar. QEMU has no "start maximized" switch (`full-screen=on` is borderless
/// fullscreen — a different thing), so a watcher thread waits for the
/// freshly-spawned process's main window and asks the OS to maximize it.
/// Paired with `zoom-to-fit=on` in the display args so the guest picture
/// fills the window.
///
/// `dark_titlebar` is a separate lever from `GTK_THEME`: GTK on Windows uses
/// the *native* frame, so the theme darkens the menu bar but leaves the
/// title bar light. Only DWM can repaint that, per window.
fn maximize_window_of(pid: u32, dark_titlebar: bool) {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowThreadProcessId, IsWindowVisible, ShowWindow, GW_OWNER,
        SW_MAXIMIZE,
    };

    struct Search {
        pid: u32,
        hwnd: HWND,
    }
    unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: `lp` is the &mut Search passed to EnumWindows below, which
        // outlives the call.
        let search = unsafe { &mut *(lp as *mut Search) };
        let mut wpid = 0u32;
        // SAFETY: hwnd is live for the duration of the callback.
        unsafe { GetWindowThreadProcessId(hwnd, &mut wpid) };
        // SAFETY: as above; a visible, unowned top-level window is the display.
        if wpid == search.pid
            && unsafe { IsWindowVisible(hwnd) } != 0
            && unsafe { GetWindow(hwnd, GW_OWNER) }.is_null()
        {
            search.hwnd = hwnd;
            return 0; // found — stop enumerating
        }
        1
    }

    std::thread::spawn(move || {
        // GTK can take a moment to realize the window; give it plenty.
        for _ in 0..150 {
            let mut search = Search {
                pid,
                hwnd: std::ptr::null_mut(),
            };
            // SAFETY: the callback only runs during this call and only touches
            // `search`, which outlives it.
            unsafe { EnumWindows(Some(enum_cb), &mut search as *mut Search as LPARAM) };
            if !search.hwnd.is_null() {
                if dark_titlebar {
                    set_dark_titlebar(search.hwnd);
                }
                // SAFETY: worst case the window died since — ShowWindow then
                // fails harmlessly.
                unsafe { ShowWindow(search.hwnd, SW_MAXIMIZE) };
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
}

/// Pull the guest's network cable as soon as QEMU will answer QMP.
///
/// Networking is opt-in, but neither `e1000e` nor `virtio-net-pci` exposes a
/// link-down property to start with (checked), so the cable can only be
/// pulled once the machine exists. The window this leaves is milliseconds
/// wide and sits in the firmware phase, long before any guest OS brings up a
/// network stack — but it is a window, not an airtight guarantee, and it is
/// the reason this runs before anything else touches the VM.
fn unplug_network(qmp_port: Option<u16>) {
    let Some(port) = qmp_port else {
        tracing::warn!("no QMP port: the guest starts with its network connected");
        return;
    };
    std::thread::spawn(move || {
        // QMP binds with the process, but "spawned" and "listening" are not
        // the same instant; retry briefly rather than lose the race.
        for _ in 0..100 {
            match crate::qmp::set_link(port, crate::config::NET_DEVICE_ID, false) {
                Ok(()) => {
                    tracing::info!("guest network starts unplugged");
                    return;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        tracing::error!("could not unplug the guest network — it may have a connection");
    });
}

/// Ask DWM to paint this window's title bar dark.
///
/// The attribute number moved: Windows 10 1809–1903 used 19, and 20 from
/// 2004 onwards (and on 11). Passing the wrong one is simply refused with
/// an error HRESULT and no side effect, so trying 20 and falling back to 19
/// is both safe and the cheapest way to cover every version — there is no
/// supported build-number query that maps cleanly onto this.
#[cfg(windows)]
fn set_dark_titlebar(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    const USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const USE_IMMERSIVE_DARK_MODE_PRE_20H1: u32 = 19;

    let on: i32 = 1;
    for attr in [USE_IMMERSIVE_DARK_MODE, USE_IMMERSIVE_DARK_MODE_PRE_20H1] {
        // SAFETY: `hwnd` is live here, and the value is the BOOL-sized i32
        // the attribute expects. A version that doesn't know `attr` returns
        // a failure HRESULT and changes nothing.
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attr,
                &on as *const i32 as *const std::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            )
        };
        if hr >= 0 {
            return;
        }
    }
}

/// The last `max_bytes` of a log file as lossy UTF-8, trimmed — what gets
/// quoted to the user when QEMU dies before the guest appears.
fn log_tail(path: &Path, max_bytes: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}
