//! Out-of-process WinFsp serve for a backup's parent VHDX.
//!
//! **Why a separate process.** Attaching a differencing child whose parent is
//! served by WinFsp works fine *during* I/O, but `DetachVirtualDisk` on that
//! child **deadlocks in the storage driver when the serve runs in the same
//! process** — the detach's Close IRP to the served parent can't complete, and
//! the process becomes unkillable (root-caused in `vm_overlay_lifecycle`; see
//! `docs/VIRTUALIZATION.md`). Separate *threads* don't help — WinFsp already
//! dispatches on its own threads. Moving the serve into a **separate process**
//! sends that Close IRP to a different process's dispatcher, so the detaching
//! process never self-deadlocks.
//!
//! Shape: the boot process re-execs *itself* with [`HELPER_SENTINEL`] as argv0'
//! to become a serve helper. The helper serves the parent, prints a `READY`
//! line with the parent's path, then blocks reading stdin. The boot process
//! holds the write end of that stdin pipe: dropping it (or dying, or killing
//! the child) closes the pipe, the helper reads EOF and stops serving. So a
//! clean stop and a force-killed parent both tear the serve down — no orphans.

use std::path::PathBuf;

/// argv[1] value that turns a normal invocation into a serve helper.
pub const HELPER_SENTINEL: &str = "__vm_serve_helper";

/// Extension for the file recording a live helper's PID, so a later run can
/// reap one that outlived its parent (see [`reap_orphan_helpers`]).
const PID_EXT: &str = "helperpid";

/// A running serve-helper process and the path it serves the parent VHDX at.
pub struct ServeProcess {
    child: std::process::Child,
    parent_path: PathBuf,
    /// PID file removed on clean stop; its presence later means an orphan.
    pid_file: Option<PathBuf>,
}

impl ServeProcess {
    /// Path to the served parent VHDX (a differencing child names this as its
    /// parent).
    pub fn parent_path(&self) -> &std::path::Path {
        &self.parent_path
    }

    /// Stop the serve: kill the helper process and reap it. Safe because the
    /// helper only serves (no VHD attach of its own), so the OS tears its WinFsp
    /// filesystem down cleanly on death. Call only AFTER the differencing child
    /// has been detached — the detach still needs the parent served.
    pub fn stop(mut self) {
        // Closing stdin signals a graceful stop; the kill is the guarantee.
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(p) = self.pid_file.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        // Safety net if `stop` wasn't called (e.g. an error path unwound past it).
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(p) = self.pid_file.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Kill serve helpers left running by a previous run that died before it could
/// stop them.
///
/// The helper normally self-exits when its parent closes the stdin pipe (which
/// the OS does even on a force-kill), so orphans should be rare — this is the
/// belt-and-braces path for when that doesn't happen. Each live helper records
/// its PID in `scratch`; a PID file still present at startup means nobody
/// cleaned up. PIDs get recycled, so we only terminate a process whose image
/// name is actually ours.
pub fn reap_orphan_helpers(scratch: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return;
    };
    let ours = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_ascii_lowercase()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(PID_EXT) {
            continue;
        }
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            if let Some(ours) = &ours {
                if terminate_if_named(pid, ours) {
                    tracing::warn!("reaped orphaned serve helper (pid {pid})");
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Terminate `pid` only if its image file name matches `expected_name`
/// (case-insensitive). Returns true if it was terminated.
fn terminate_if_named(pid: u32, expected_name: &std::ffi::OsString) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    // SAFETY: a failed open returns null, which we check before any use.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false; // already gone (or not ours to touch)
    }

    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: handle is live; buf/len describe a valid writable buffer.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT::default(),
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    let mut terminated = false;
    if ok != 0 {
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let name = std::path::Path::new(&full)
            .file_name()
            .map(|f| f.to_ascii_lowercase());
        if name.as_ref() == Some(expected_name) {
            // SAFETY: handle was opened with PROCESS_TERMINATE.
            terminated = unsafe { TerminateProcess(handle, 1) } != 0;
        }
    }
    // SAFETY: handle came from OpenProcess and is not used after this.
    unsafe { CloseHandle(handle) };
    terminated
}

/// Spawn a serve-helper subprocess for `backup`, serving at `scratch`, and wait
/// (bounded) for it to report `READY`. Returns the running process plus the
/// served parent path.
pub fn spawn_serve(
    backup: &std::path::Path,
    scratch: &std::path::Path,
) -> anyhow::Result<ServeProcess> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("locate current exe: {e}"))?;
    spawn_serve_with_exe(&exe, backup, scratch)
}

/// As [`spawn_serve`], but with an explicit helper binary instead of
/// `current_exe()`. Used by tests (which re-exec the built `simulacra-cli`,
/// since a test binary doesn't route the sentinel) — the frontend must always
/// route [`HELPER_SENTINEL`] through [`maybe_run`].
pub fn spawn_serve_with_exe(
    exe: &std::path::Path,
    backup: &std::path::Path,
    scratch: &std::path::Path,
) -> anyhow::Result<ServeProcess> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let mut child = Command::new(exe)
        .arg(HELPER_SENTINEL)
        .arg(backup)
        .arg(scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn serve helper {}: {e}", exe.display()))?;

    // Read the READY handshake off stdout on a thread so we can bound the wait:
    // if WinFsp init hangs, we don't block the boot forever.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("serve helper stdout unavailable"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let res = reader.read_line(&mut line).map(|_| line);
        let _ = tx.send(res);
    });

    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(line)) => {
            if let Some(path) = line.trim_end().strip_prefix("READY\t") {
                let parent_path = PathBuf::from(path);
                // Record the PID so a later run can reap this helper if we die
                // before stopping it.
                let pid_file = scratch.join(format!("{}.{PID_EXT}", child.id()));
                let pid_file = std::fs::write(&pid_file, child.id().to_string())
                    .ok()
                    .map(|_| pid_file);
                Ok(ServeProcess {
                    child,
                    parent_path,
                    pid_file,
                })
            } else {
                let _ = child.kill();
                anyhow::bail!(
                    "serve helper did not report READY (said: {:?})",
                    line.trim_end()
                );
            }
        }
        Ok(Err(e)) => {
            let _ = child.kill();
            anyhow::bail!("reading serve helper handshake: {e}");
        }
        Err(_) => {
            let _ = child.kill();
            anyhow::bail!("serve helper did not become ready within 30s");
        }
    }
}

/// If this process was launched as a serve helper (argv[1] == [`HELPER_SENTINEL`]),
/// run the serve and block until the parent closes our stdin, then return
/// `Some(result)`. Otherwise return `None` (this is a normal invocation).
///
/// The frontend (`simulacra-cli`, and later the GUI) must call this at the very
/// top of `main`, before any argument parsing, so the sentinel re-exec is
/// handled instead of rejected.
#[cfg(feature = "winfsp")]
pub fn maybe_run(args: &[String]) -> Option<anyhow::Result<()>> {
    if args.get(1).map(String::as_str) != Some(HELPER_SENTINEL) {
        return None;
    }
    let backup = args.get(2).cloned().unwrap_or_default();
    let scratch = args.get(3).cloned().unwrap_or_default();
    Some(run(
        std::path::Path::new(&backup),
        std::path::Path::new(&scratch),
    ))
}

/// Non-winfsp builds can't serve, so the sentinel is never handled (VM boot
/// requires the feature anyway).
#[cfg(not(feature = "winfsp"))]
pub fn maybe_run(_args: &[String]) -> Option<anyhow::Result<()>> {
    None
}

#[cfg(feature = "winfsp")]
fn run(backup: &std::path::Path, scratch: &std::path::Path) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    std::fs::create_dir_all(scratch).ok();
    let serve = phoenix_mount::WinFspServe::serve_for_vm(backup, scratch)
        .map_err(|e| anyhow::anyhow!("serve_for_vm: {e}"))?;
    let parent = serve.image_path();

    // Handshake: tell the parent we're up and exactly where the parent VHDX is.
    println!("READY\t{}", parent.display());
    std::io::stdout().flush().ok();

    // Block until the parent closes our stdin (clean stop, or the parent died /
    // was force-killed — the OS closes the pipe either way). Then stop serving.
    let mut buf = [0u8; 1];
    let _ = std::io::stdin().read(&mut buf);

    drop(serve);
    Ok(())
}
