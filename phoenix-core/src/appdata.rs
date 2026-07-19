//! Persisted application data: job history and user settings.
//!
//! Both live under `%LOCALAPPDATA%\PhoenixSimulacra` as JSON. Reads never panic on
//! a corrupt or missing file: settings fall back to defaults, and a corrupt
//! history file is renamed aside and a fresh one started, so a damaged file can
//! never take down the app.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `%LOCALAPPDATA%\PhoenixSimulacra` (created on demand). Falls back to the temp
/// dir if `LOCALAPPDATA` is unset (e.g. some service contexts).
pub fn appdata_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join("PhoenixSimulacra");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn history_path() -> PathBuf {
    appdata_dir().join("history.json")
}

fn settings_path() -> PathBuf {
    appdata_dir().join("settings.json")
}

fn update_state_path() -> PathBuf {
    appdata_dir().join("update.json")
}

/// `%LOCALAPPDATA%\PhoenixSimulacra\updates` (created on demand) — where the
/// self-updater stages a downloaded installer while it verifies it.
pub fn updates_dir() -> PathBuf {
    let dir = appdata_dir().join("updates");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Seconds since the Unix epoch, right now. Timestamps are stored as plain
/// integers so the on-disk format doesn't depend on a datetime crate version.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Kind of operation a history record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobKindTag {
    Backup,
    Restore,
    Verify,
    Clone,
    BootRepair,
}

/// Terminal outcome of a recorded job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobOutcome {
    Success,
    Failed(String),
    Cancelled,
}

/// One completed operation, appended to the history log.
///
/// `source` and `target` are the two ends of the job, already phrased for a
/// human — the History page pairs them with the kind to write its Details
/// line ("Imaged Samsung SSD 990 PRO (Disk 1) → D:\Backups\work.phnx"). They
/// are captured when the job is spawned, because by the time it finishes the
/// page that named them has usually been reset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: Uuid,
    pub kind: JobKindTag,
    /// Start time as a Unix timestamp (seconds).
    pub started_unix: i64,
    pub duration_secs: u64,
    /// What the job read: the disk a backup captured or a clone copied, or
    /// the `.phnx` a restore or verify opened.
    pub source: String,
    /// What the job wrote: the `.phnx` a backup produced, or the disk a
    /// restore or clone landed on. Empty for a verify — it has no target.
    pub target: String,
    pub outcome: JobOutcome,
    /// Bytes the job moved (captured, restored, cloned, or hashed).
    pub bytes_processed: u64,
    /// On-disk size of the `.phnx` this job wrote or read; 0 when the job has
    /// no backup file (a clone). This is the size the History page shows next
    /// to the file name.
    pub image_bytes: u64,
}

impl JobRecord {
    /// A finished job that ran for `duration_secs` starting at `started_unix`.
    /// The ends of the job and its byte counts go on via the `with_*` builders.
    pub fn new(
        kind: JobKindTag,
        outcome: JobOutcome,
        started_unix: i64,
        duration_secs: u64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            started_unix,
            duration_secs,
            source: String::new(),
            target: String::new(),
            outcome,
            bytes_processed: 0,
            image_bytes: 0,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = target.into();
        self
    }

    pub fn with_bytes(mut self, processed: u64, image: u64) -> Self {
        self.bytes_processed = processed;
        self.image_bytes = image;
        self
    }
}

/// Bumped whenever [`JobRecord`] changes shape. A history file written under
/// an older schema is set aside on load rather than shown: pre-v2 records
/// stored only a status line — no disk, no file, no size — so there is nothing
/// the History page could put in its columns.
pub const HISTORY_SCHEMA: u32 = 2;

/// The rolling history log. Capped so it can't grow without bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub schema: u32,
    pub records: Vec<JobRecord>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            schema: HISTORY_SCHEMA,
            records: Vec::new(),
        }
    }
}

/// Keep at most this many records (newest kept).
pub const HISTORY_CAP: usize = 500;

impl History {
    /// Load from `%LOCALAPPDATA%`. A missing file yields an empty history; an
    /// outdated or corrupt file is renamed aside and an empty history returned,
    /// so neither bad data nor a format bump can take the app down.
    pub fn load() -> Self {
        Self::load_from(&history_path())
    }

    fn load_from(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return History::default();
        };
        // Two different kinds of unusable file, kept apart so a format bump
        // doesn't look like data loss: a file from an older schema is simply
        // *old* (set aside as `.json.old`), while one we can't parse at all is
        // *corrupt* (`.json.corrupt`). Either way the app starts a fresh log
        // rather than failing to open.
        let aside = |ext: &str| {
            let _ = std::fs::rename(path, path.with_extension(ext));
            History::default()
        };
        match file_schema(&bytes) {
            Some(HISTORY_SCHEMA) => match serde_json::from_slice::<History>(&bytes) {
                Ok(h) => h,
                Err(_) => aside("json.corrupt"),
            },
            Some(_) => aside("json.old"),
            None => aside("json.corrupt"),
        }
    }

    /// Append a record (trimming to the cap) and persist atomically.
    pub fn append(&mut self, record: JobRecord) -> std::io::Result<()> {
        self.records.push(record);
        let overflow = self.records.len().saturating_sub(HISTORY_CAP);
        if overflow > 0 {
            self.records.drain(0..overflow);
        }
        self.save()
    }

    /// Drop the record at `index`, returning whether one was there. Does NOT
    /// persist — the caller decides (the GUI's `--demo-history` rows must never
    /// reach the real history file). Out-of-range is a no-op: the History
    /// page's rows can outlive the record they point at by a frame.
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.records.len() {
            return false;
        }
        self.records.remove(index);
        true
    }

    /// Persist to disk atomically (write temp + rename).
    pub fn save(&self) -> std::io::Result<()> {
        save_atomic(&history_path(), self)
    }
}

/// The `schema` a history file declares, or `None` if it isn't even JSON.
/// Read on its own — before the records — so a schema bump doesn't have to
/// keep every old record shape deserializable just to find out it's old.
fn file_schema(bytes: &[u8]) -> Option<u32> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        schema: u32,
    }
    serde_json::from_slice::<Probe>(bytes)
        .ok()
        .map(|p| p.schema)
}

/// How the GUI should choose its theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}

/// User-configurable preferences. Every field has a serde default so an older
/// or partial settings file still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The folder the last backup was written to — what the Backup page opens
    /// on. Not a setting the user picks: the GUI writes it after every backup
    /// whose destination it has already validated.
    pub default_backup_dir: Option<String>,
    /// No UI exposes this any more — verifying is what makes an image worth
    /// having, so the GUI always verifies. Kept as a hand-editable escape
    /// hatch, and read by the GUI when it builds a backup job.
    pub verify_after_backup: bool,
    pub default_verify_quick: bool,
    pub clone_readback_verify: bool,
    pub theme: ThemeChoice,
    /// Rescue/PE ISOs the Virtualize page has attached before, newest first,
    /// so the ISO picker can offer them as a dropdown. Capped by the GUI.
    pub vm_iso_history: Vec<String>,
    /// The Virtualize page's scratch-drive choice: `None` = same drive as the
    /// image (the default), `Some('C')` = that drive. Persisted because a
    /// fresh launch resetting it silently redirected new sessions.
    pub vm_scratch_drive: Option<char>,
    /// The Virtualize page's memory slider (MiB). `None` = the built-in default.
    pub vm_memory_mib: Option<u64>,
    /// The Virtualize page's processor slider. `None` = the built-in default.
    pub vm_cpus: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_backup_dir: None,
            // On by default: re-read the source after capture and confirm the
            // backup matches it. Integrity is the point of making images.
            verify_after_backup: true,
            default_verify_quick: true,
            clone_readback_verify: true,
            theme: ThemeChoice::System,
            vm_iso_history: Vec::new(),
            vm_scratch_drive: None,
            vm_memory_mib: None,
            vm_cpus: None,
        }
    }
}

impl Settings {
    /// Load settings, falling back to defaults on a missing or unreadable file.
    pub fn load() -> Self {
        Self::load_from(&settings_path())
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        save_atomic(&settings_path(), self)
    }
}

/// Self-updater state, persisted separately from user [`Settings`] because the
/// app writes it (last-check throttle, staged installer) rather than the user.
///
/// Every field has a serde default, so a missing or partial file loads cleanly.
/// A `staged_installer` path is recorded *only* after the download has passed
/// both SHA-256 and Authenticode verification — on close the app trusts it and
/// runs it silently, so nothing unverified may ever land here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpdateState {
    /// Unix seconds of the last completed update check. Drives the once-a-day
    /// throttle on the silent startup check.
    pub last_check_unix: i64,
    /// Version string (bare `x.y.z`) of the staged installer, if any.
    pub staged_version: Option<String>,
    /// Absolute path to the verified staged installer to run on exit, if any.
    pub staged_installer: Option<String>,
}

impl UpdateState {
    /// Load update state, falling back to defaults on a missing or unreadable
    /// file (same forgiving contract as [`Settings::load`]).
    pub fn load() -> Self {
        Self::load_from(&update_state_path())
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<UpdateState>(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        save_atomic(&update_state_path(), self)
    }

    /// Forget any staged installer (keeps `last_check_unix`). Called once the
    /// installer has been handed off on exit, or when a staged update is found
    /// to be stale (already installed).
    pub fn clear_staged(&mut self) {
        self.staged_version = None;
        self.staged_installer = None;
    }
}

/// Serialize `value` as pretty JSON to a temp file next to `path`, then rename
/// it into place so a crash mid-write can't leave a half-written file.
fn save_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(i: u32) -> JobRecord {
        JobRecord::new(JobKindTag::Backup, JobOutcome::Success, i as i64, 1)
            .with_source("Samsung SSD 990 PRO (Disk 1)")
            .with_target(r"D:\Backups\work.phnx")
            .with_bytes(100, 60)
    }

    #[test]
    fn history_roundtrips_via_json() {
        let mut h = History::default();
        h.records.push(rec(1));
        h.records.push(rec(2));
        let json = serde_json::to_vec(&h).unwrap();
        let back: History = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.records.len(), 2);
        assert_eq!(back.schema, HISTORY_SCHEMA);
        assert_eq!(back.records[0], h.records[0]);
    }

    #[test]
    fn older_schema_is_set_aside_not_shown() {
        let dir = std::env::temp_dir().join(format!("cphist_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        // A v1 file: valid JSON, records in the old shape, no `schema` field.
        std::fs::write(
            &path,
            br#"{"records":[{"id":"00000000-0000-0000-0000-000000000001","kind":"Backup",
                "started_unix":1,"duration_secs":2,"source":"","target":"Backup completed",
                "backup_id":null,"outcome":"Success","bytes_processed":10}]}"#,
        )
        .unwrap();
        let h = History::load_from(&path);
        assert!(h.records.is_empty());
        assert_eq!(h.schema, HISTORY_SCHEMA);
        // Kept, not destroyed.
        assert!(path.with_extension("json.old").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_drops_one_record_and_ignores_bad_index() {
        let mut h = History::default();
        h.records.extend([rec(1), rec(2), rec(3)]);
        let kept = (h.records[0].id, h.records[2].id);
        assert!(h.remove(1));
        assert_eq!(h.records.len(), 2);
        assert_eq!((h.records[0].id, h.records[1].id), kept);
        assert!(!h.remove(9));
        assert_eq!(h.records.len(), 2);
    }

    #[test]
    fn append_trims_to_cap() {
        let dir = std::env::temp_dir().join(format!("cphist_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let mut h = History::default();
        for i in 0..(HISTORY_CAP as u32 + 25) {
            h.records.push(rec(i));
        }
        // Simulate the trim `append` performs.
        let overflow = h.records.len().saturating_sub(HISTORY_CAP);
        h.records.drain(0..overflow);
        assert_eq!(h.records.len(), HISTORY_CAP);
        // First remaining record should be index 25 (oldest 25 dropped).
        assert_eq!(h.records[0].started_unix, 25);
        save_atomic(&path, &h).unwrap();
        let back = History::load_from(&path);
        assert_eq!(back.records.len(), HISTORY_CAP);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_history_recovers_to_empty() {
        let dir = std::env::temp_dir().join(format!("cphist_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        let h = History::load_from(&path);
        assert!(h.records.is_empty());
        // The corrupt file was renamed aside.
        assert!(path.with_extension("json.corrupt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settings_defaults_on_missing_file() {
        let s = Settings::load_from(Path::new("does-not-exist.json"));
        assert_eq!(s, Settings::default());
        assert!(s.default_verify_quick);
    }

    #[test]
    fn settings_partial_file_uses_defaults() {
        // Only one field present; the rest must default (serde(default)).
        let json = br#"{"theme":"Dark"}"#;
        let s: Settings = serde_json::from_slice(json).unwrap();
        assert_eq!(s.theme, ThemeChoice::Dark);
        assert!(s.default_verify_quick); // defaulted
    }

    #[test]
    fn update_state_defaults_on_missing_file() {
        let u = UpdateState::load_from(Path::new("does-not-exist.json"));
        assert_eq!(u, UpdateState::default());
        assert_eq!(u.last_check_unix, 0);
        assert!(u.staged_installer.is_none());
    }

    #[test]
    fn update_state_roundtrips_and_clears() {
        let mut u = UpdateState {
            last_check_unix: 1_700_000_000,
            staged_version: Some("0.2.0".into()),
            staged_installer: Some(r"C:\path\Simulacra-Setup-0.2.0.exe".into()),
        };
        let json = serde_json::to_vec(&u).unwrap();
        let back: UpdateState = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, u);
        // Clearing the staged fields leaves the throttle timestamp intact.
        u.clear_staged();
        assert_eq!(u.last_check_unix, 1_700_000_000);
        assert!(u.staged_version.is_none());
        assert!(u.staged_installer.is_none());
    }
}
