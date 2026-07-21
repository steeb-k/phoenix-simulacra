//! The guest-tools / driver ISO (virtio-win): local discovery, background
//! download with progress, and a lightweight update check.
//!
//! The ISO lives next to the application binary (`virtio-win.iso`) so one
//! download serves every backup and survives app updates. A sidecar
//! (`virtio-win.iso.meta`) records the server's `ETag`/`Last-Modified` from
//! the download, so "check for update" is a single HEAD request compared
//! against it — no re-download unless the server's copy actually changed.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Fedora's stable virtio-win alias — always points at the latest stable ISO.
const ISO_URL: &str =
    "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso";
const ISO_NAME: &str = "virtio-win.iso";

/// Events posted by the download / update-check workers.
pub enum DlEvent {
    Progress {
        downloaded: u64,
        total: u64,
    },
    /// Download finished (fresh install or update).
    Done,
    /// Update check ran and the local ISO is already current.
    UpToDate,
    Failed(String),
}

/// Where the ISO lives: next to the application binary.
pub fn local_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(ISO_NAME))
}

/// The ISO's path if it is present on this machine.
pub fn installed() -> Option<PathBuf> {
    local_path().filter(|p| p.is_file())
}

fn meta_path() -> Option<PathBuf> {
    Some(local_path()?.with_extension("iso.meta"))
}

/// What the server said about the copy we downloaded.
#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
struct IsoMeta {
    etag: String,
    last_modified: String,
    length: u64,
}

fn server_meta(resp_headers: &ureq::Response) -> IsoMeta {
    IsoMeta {
        etag: resp_headers.header("ETag").unwrap_or_default().to_string(),
        last_modified: resp_headers
            .header("Last-Modified")
            .unwrap_or_default()
            .to_string(),
        length: resp_headers
            .header("Content-Length")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    }
}

fn load_meta() -> Option<IsoMeta> {
    let text = std::fs::read_to_string(meta_path()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn store_meta(meta: &IsoMeta) {
    if let (Some(path), Ok(json)) = (meta_path(), serde_json::to_string_pretty(meta)) {
        let _ = std::fs::write(path, json);
    }
}

/// Download the ISO (streaming, with progress events) on a background thread.
pub fn spawn_download(tx: Sender<DlEvent>) {
    std::thread::spawn(move || {
        let _ = tx.send(match download(&tx) {
            Ok(()) => DlEvent::Done,
            Err(m) => DlEvent::Failed(m),
        });
    });
}

/// Check whether the server has a newer ISO than the local copy; if it does,
/// download it (same progress events). Runs on a background thread.
pub fn spawn_update_check(tx: Sender<DlEvent>) {
    std::thread::spawn(move || {
        let agent = crate::updater::agent();
        let head = match agent
            .head(ISO_URL)
            .set("User-Agent", crate::updater::USER_AGENT)
            .call()
        {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(DlEvent::Failed(format!("update check failed: {e}")));
                return;
            }
        };
        let remote = server_meta(&head);

        // No sidecar (hand-placed ISO): fall back to comparing sizes, and
        // adopt the server's metadata if they match so the next check is exact.
        let current = match load_meta() {
            Some(m) => m,
            None => {
                let local_len = installed()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .unwrap_or(0);
                if local_len == remote.length && local_len > 0 {
                    store_meta(&remote);
                    let _ = tx.send(DlEvent::UpToDate);
                    return;
                }
                IsoMeta::default()
            }
        };

        if current == remote {
            let _ = tx.send(DlEvent::UpToDate);
            return;
        }
        let _ = tx.send(match download(&tx) {
            Ok(()) => DlEvent::Done,
            Err(m) => DlEvent::Failed(m),
        });
    });
}

fn download(tx: &Sender<DlEvent>) -> Result<(), String> {
    let final_path = local_path().ok_or("cannot locate the application folder")?;
    let part_path = final_path.with_extension("iso.part");

    let agent = crate::updater::agent();
    let resp = agent
        .get(ISO_URL)
        .set("User-Agent", crate::updater::USER_AGENT)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let meta = server_meta(&resp);
    let total = meta.length;

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part_path).map_err(|e| {
        format!(
            "cannot write next to the app ({}): {e}",
            part_path.display()
        )
    })?;
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
            .map_err(|e| format!("cannot write ISO: {e}"))?;
        downloaded += n as u64;
        // At most ~one event per MB, so the channel stays quiet.
        if downloaded - last_report >= 1_000_000 {
            last_report = downloaded;
            let _ = tx.send(DlEvent::Progress { downloaded, total });
        }
    }
    drop(file);

    if total > 0 && downloaded != total {
        let _ = std::fs::remove_file(&part_path);
        return Err(format!(
            "download truncated ({downloaded} of {total} bytes) — discarded"
        ));
    }

    std::fs::rename(&part_path, &final_path).map_err(|e| format!("cannot finalize ISO: {e}"))?;
    store_meta(&meta);
    tracing::info!(path = %final_path.display(), bytes = downloaded, "virtio-win ISO downloaded");
    Ok(())
}
