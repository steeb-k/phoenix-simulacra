//! Persistent VM sessions.
//!
//! Unlike the writable mount's ephemeral child (deleted on unmount), a VM
//! session is **kept**: its overlay, firmware, and metadata live in a stable
//! directory keyed by the backup id, so you can shut the guest down and resume
//! it later — or discard it explicitly. Everything here is pure filesystem +
//! JSON, so it is unit-tested without WinFsp or elevation. The overlay's
//! creation and attach (which need the served parent and admin) live in
//! `boot.rs`; this module owns the layout, the metadata, and the lifecycle.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where guest writes go. Both keep the backing `.phnx` immutable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteLayer {
    /// A differencing `.avhdx` child attached offline; QEMU does raw I/O
    /// against the resulting physical device. Enables boot-it-or-mount-it
    /// interop (the same overlay can be opened by the writable mount). Windows-
    /// only, needs elevation. The committed default.
    Avhdx,
    /// A qcow2 overlay backing onto the served VHDX. QEMU-native, no attach,
    /// no elevation; the portable fallback.
    Qcow2,
}

impl WriteLayer {
    fn overlay_file(self) -> &'static str {
        match self {
            WriteLayer::Avhdx => "session.avhdx",
            WriteLayer::Qcow2 => "session.qcow2",
        }
    }
}

/// On-disk session metadata (`session.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub backup_id: String,
    pub backup_path: String,
    pub write_layer: WriteLayer,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_booted_at: Option<String>,
    /// False while a guest is (or was) running — set true only after a clean
    /// teardown. A session found dirty at startup had its host process die
    /// mid-boot; the overlay is intact but may hold a torn last write.
    #[serde(default)]
    pub clean_shutdown: bool,
}

/// One VM session on disk.
#[derive(Debug, Clone)]
pub struct Session {
    dir: PathBuf,
    meta: SessionMeta,
}

impl Session {
    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write_layer(&self) -> WriteLayer {
        self.meta.write_layer
    }

    /// The overlay file (may not exist yet — `boot.rs` creates it on first boot).
    pub fn overlay_path(&self) -> PathBuf {
        self.dir.join(self.meta.write_layer.overlay_file())
    }

    pub fn overlay_exists(&self) -> bool {
        self.overlay_path().exists()
    }

    /// Per-session UEFI code flash (writable copy of the OVMF master).
    pub fn firmware_code_path(&self) -> PathBuf {
        self.dir.join("code.fd")
    }

    /// Per-session UEFI NVRAM varstore (boot order persists here).
    pub fn firmware_vars_path(&self) -> PathBuf {
        self.dir.join("nvram-vars.fd")
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    /// Copy the UEFI firmware into the session if not already present. Idempotent
    /// across resumes — the varstore is only seeded once so boot-order changes
    /// survive. Pass the QEMU master blobs.
    pub fn ensure_uefi_firmware(&self, code_master: &Path, vars_template: &Path) -> Result<()> {
        let code = self.firmware_code_path();
        // The code flash is stateless firmware, but WHPX needs it writable, so
        // a per-session writable copy is refreshed each call.
        std::fs::copy(code_master, &code)
            .with_context(|| format!("copy OVMF code flash from {}", code_master.display()))?;
        let vars = self.firmware_vars_path();
        if !vars.exists() {
            std::fs::copy(vars_template, &vars)
                .with_context(|| format!("seed NVRAM varstore from {}", vars_template.display()))?;
        }
        Ok(())
    }

    /// Persist the current metadata to `session.json`.
    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.meta)?;
        std::fs::write(self.meta_path(), json)
            .with_context(|| format!("write {}", self.meta_path().display()))?;
        Ok(())
    }

    /// Mark the session running (dirty) and stamp the boot time. Call before
    /// launching the guest, then `mark_clean` after a clean teardown.
    pub fn mark_booting(&mut self, now_rfc3339: String) -> Result<()> {
        self.meta.last_booted_at = Some(now_rfc3339);
        self.meta.clean_shutdown = false;
        self.save()
    }

    pub fn mark_clean(&mut self) -> Result<()> {
        self.meta.clean_shutdown = true;
        self.save()
    }
}

/// Owns the sessions root and creates / lists / discards sessions.
#[derive(Debug, Clone)]
pub struct SessionManager {
    root: PathBuf,
}

impl SessionManager {
    pub fn new(root: PathBuf) -> Self {
        SessionManager { root }
    }

    /// Default sessions root: `%LOCALAPPDATA%\PhoenixSimulacra\vm-sessions`,
    /// falling back to the temp dir if LOCALAPPDATA is unset.
    pub fn default_root() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("PhoenixSimulacra").join("vm-sessions")
    }

    pub fn with_default_root() -> Self {
        Self::new(Self::default_root())
    }

    fn session_dir(&self, backup_id: &Uuid) -> PathBuf {
        self.root.join(backup_id.simple().to_string())
    }

    /// Open the session for `backup_id`, creating it (directory + metadata) if
    /// it doesn't exist. Resume is implicit: an existing session keeps its
    /// overlay and varstore, so the next boot continues where it left off.
    ///
    /// `now_rfc3339` is passed in rather than read from the clock so callers
    /// control time (and tests stay deterministic).
    pub fn open_or_create(
        &self,
        backup_id: Uuid,
        backup_path: &Path,
        write_layer: WriteLayer,
        now_rfc3339: String,
    ) -> Result<Session> {
        let dir = self.session_dir(&backup_id);
        let meta_path = dir.join("session.json");
        if meta_path.exists() {
            let text = std::fs::read_to_string(&meta_path)
                .with_context(|| format!("read {}", meta_path.display()))?;
            let meta: SessionMeta = serde_json::from_str(&text)
                .with_context(|| format!("parse {}", meta_path.display()))?;
            return Ok(Session { dir, meta });
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create session dir {}", dir.display()))?;
        let meta = SessionMeta {
            backup_id: backup_id.simple().to_string(),
            backup_path: backup_path.display().to_string(),
            write_layer,
            created_at: now_rfc3339,
            last_booted_at: None,
            clean_shutdown: true,
        };
        let session = Session { dir, meta };
        session.save()?;
        Ok(session)
    }

    /// Every session under the root (best-effort; skips unreadable entries).
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for e in entries.flatten() {
            let meta_path = e.path().join("session.json");
            if let Ok(text) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<SessionMeta>(&text) {
                    out.push(meta);
                }
            }
        }
        out
    }

    /// Delete a session entirely (overlay, firmware, metadata). The backing
    /// `.phnx` is untouched — the session only ever held the throwaway overlay.
    pub fn discard(&self, backup_id: &Uuid) -> Result<()> {
        let dir = self.session_dir(backup_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("remove session dir {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        // Unique-ish without Math.random: use the test's overlay filename plus
        // a counter file would be overkill — a nested path per test name works.
        p.push(format!("phoenix-vm-test-{}", uuid::Uuid::new_v4().simple()));
        p
    }

    #[test]
    fn create_then_resume_keeps_same_dir_and_meta() {
        let root = tmp_root();
        let mgr = SessionManager::new(root.clone());
        let id = Uuid::from_u128(0x1234);
        let backup = Path::new(r"D:\test.phnx");

        let s1 = mgr
            .open_or_create(id, backup, WriteLayer::Avhdx, "2026-07-18T00:00:00Z".into())
            .unwrap();
        let dir1 = s1.dir().to_path_buf();
        assert!(s1.meta().clean_shutdown);
        assert_eq!(s1.write_layer(), WriteLayer::Avhdx);

        // Resume: same id -> same dir, metadata reloaded (not recreated).
        let s2 = mgr
            .open_or_create(id, backup, WriteLayer::Avhdx, "2026-07-19T00:00:00Z".into())
            .unwrap();
        assert_eq!(s2.dir(), dir1);
        // created_at is preserved from the first open, not overwritten.
        assert_eq!(s2.meta().created_at, "2026-07-18T00:00:00Z");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn overlay_path_matches_write_layer() {
        let root = tmp_root();
        let mgr = SessionManager::new(root.clone());
        let a = mgr
            .open_or_create(
                Uuid::from_u128(1),
                Path::new("a.phnx"),
                WriteLayer::Avhdx,
                "t".into(),
            )
            .unwrap();
        assert!(a.overlay_path().to_string_lossy().ends_with("session.avhdx"));
        let q = mgr
            .open_or_create(
                Uuid::from_u128(2),
                Path::new("b.phnx"),
                WriteLayer::Qcow2,
                "t".into(),
            )
            .unwrap();
        assert!(q.overlay_path().to_string_lossy().ends_with("session.qcow2"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn booting_then_clean_roundtrips_through_disk() {
        let root = tmp_root();
        let mgr = SessionManager::new(root.clone());
        let id = Uuid::from_u128(7);
        let mut s = mgr
            .open_or_create(id, Path::new("c.phnx"), WriteLayer::Qcow2, "t".into())
            .unwrap();
        s.mark_booting("2026-07-18T12:00:00Z".into()).unwrap();

        // A fresh manager re-reads the persisted dirty state.
        let reloaded = SessionManager::new(root.clone())
            .open_or_create(id, Path::new("c.phnx"), WriteLayer::Qcow2, "t".into())
            .unwrap();
        assert!(!reloaded.meta().clean_shutdown);
        assert_eq!(
            reloaded.meta().last_booted_at.as_deref(),
            Some("2026-07-18T12:00:00Z")
        );

        s.mark_clean().unwrap();
        let reloaded2 = SessionManager::new(root.clone())
            .open_or_create(id, Path::new("c.phnx"), WriteLayer::Qcow2, "t".into())
            .unwrap();
        assert!(reloaded2.meta().clean_shutdown);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_and_discard() {
        let root = tmp_root();
        let mgr = SessionManager::new(root.clone());
        mgr.open_or_create(Uuid::from_u128(10), Path::new("a"), WriteLayer::Avhdx, "t".into())
            .unwrap();
        mgr.open_or_create(Uuid::from_u128(11), Path::new("b"), WriteLayer::Qcow2, "t".into())
            .unwrap();
        assert_eq!(mgr.list().len(), 2);

        mgr.discard(&Uuid::from_u128(10)).unwrap();
        assert_eq!(mgr.list().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
