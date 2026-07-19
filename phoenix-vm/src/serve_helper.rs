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

/// A running serve-helper process and the path it serves the parent VHDX at.
pub struct ServeProcess {
    child: std::process::Child,
    parent_path: PathBuf,
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
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        // Safety net if `stop` wasn't called (e.g. an error path unwound past it).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a serve-helper subprocess for `backup`, serving at `scratch`, and wait
/// (bounded) for it to report `READY`. Returns the running process plus the
/// served parent path.
pub fn spawn_serve(backup: &std::path::Path, scratch: &std::path::Path) -> anyhow::Result<ServeProcess> {
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
                Ok(ServeProcess { child, parent_path })
            } else {
                let _ = child.kill();
                anyhow::bail!("serve helper did not report READY (said: {:?})", line.trim_end());
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
    Some(run(std::path::Path::new(&backup), std::path::Path::new(&scratch)))
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
