//! Persisted application data: job history and user settings.
//!
//! Both live under `%LOCALAPPDATA%\CarbonPhoenix` as JSON. Reads never panic on
//! a corrupt or missing file: settings fall back to defaults, and a corrupt
//! history file is renamed aside and a fresh one started, so a damaged file can
//! never take down the app.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `%LOCALAPPDATA%\CarbonPhoenix` (created on demand). Falls back to the temp
/// dir if `LOCALAPPDATA` is unset (e.g. some service contexts).
pub fn appdata_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join("CarbonPhoenix");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn history_path() -> PathBuf {
    appdata_dir().join("history.json")
}

fn settings_path() -> PathBuf {
    appdata_dir().join("settings.json")
}

/// Kind of operation a history record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobKindTag {
    Backup,
    Restore,
    Verify,
    Clone,
    Mount,
}

/// Terminal outcome of a recorded job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobOutcome {
    Success,
    Failed(String),
    Cancelled,
}

/// One completed operation, appended to the history log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: Uuid,
    pub kind: JobKindTag,
    /// Start time as a Unix timestamp (seconds). Stored as an integer so the
    /// format doesn't depend on a particular datetime crate version.
    pub started_unix: i64,
    pub duration_secs: u64,
    pub source: String,
    pub target: String,
    pub backup_id: Option<Uuid>,
    pub outcome: JobOutcome,
    pub bytes_processed: u64,
}

impl JobRecord {
    /// Build a record for a job that just finished `duration_secs` ago, filling
    /// in a fresh id and computing `started_unix` from the current time.
    #[allow(clippy::too_many_arguments)]
    pub fn now(
        kind: JobKindTag,
        duration_secs: u64,
        source: String,
        target: String,
        backup_id: Option<Uuid>,
        outcome: JobOutcome,
        bytes_processed: u64,
    ) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self {
            id: Uuid::new_v4(),
            kind,
            started_unix: now - duration_secs as i64,
            duration_secs,
            source,
            target,
            backup_id,
            outcome,
            bytes_processed,
        }
    }
}

/// The rolling history log. Capped so it can't grow without bound.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    pub records: Vec<JobRecord>,
}

/// Keep at most this many records (newest kept).
pub const HISTORY_CAP: usize = 500;

impl History {
    /// Load from `%LOCALAPPDATA%`. A missing file yields an empty history; a
    /// corrupt file is renamed to `history.json.corrupt` and an empty history
    /// is returned so the app never crashes on bad data.
    pub fn load() -> Self {
        Self::load_from(&history_path())
    }

    fn load_from(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<History>(&bytes) {
                Ok(h) => h,
                Err(_) => {
                    let aside = path.with_extension("json.corrupt");
                    let _ = std::fs::rename(path, aside);
                    History::default()
                }
            },
            Err(_) => History::default(),
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

    /// Persist to disk atomically (write temp + rename).
    pub fn save(&self) -> std::io::Result<()> {
        save_atomic(&history_path(), self)
    }
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
    pub default_backup_dir: Option<String>,
    pub verify_after_backup: bool,
    pub default_verify_quick: bool,
    pub clone_readback_verify: bool,
    pub theme: ThemeChoice,
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
        JobRecord {
            id: Uuid::new_v4(),
            kind: JobKindTag::Backup,
            started_unix: i as i64,
            duration_secs: 1,
            source: "src".into(),
            target: "dst".into(),
            backup_id: None,
            outcome: JobOutcome::Success,
            bytes_processed: 100,
        }
    }

    #[test]
    fn history_roundtrips_via_json() {
        let mut h = History::default();
        h.records.push(rec(1));
        h.records.push(rec(2));
        let json = serde_json::to_vec(&h).unwrap();
        let back: History = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.records.len(), 2);
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
}
