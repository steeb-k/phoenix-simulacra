use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::disk::{BitlockerState, CaptureMode, FilesystemKind};
use crate::error::{PhoenixError, Result};
use crate::hash;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format_version: u32,
    pub backup_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_backup_id: Option<Uuid>,
    pub hostname: String,
    pub disk: DiskManifest,
    pub partitions: Vec<PartitionManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskManifest {
    pub style: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_guid: Option<String>,
    pub sector_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionManifest {
    pub index: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_guid: Option<String>,
    pub fs: String,
    pub capture_mode: String,
    pub original_size: u64,
    pub used_bytes: u64,
    /// BitLocker state at capture time. `None`/absent → not a BitLocker
    /// volume (also the value in every pre-BitLocker-support backup).
    /// `"unlocked"` → the volume was BitLocker but unlocked, so the image
    /// holds **plaintext** and restores as a normal, unencrypted volume.
    /// `"locked"` → the image holds raw **ciphertext**; a restore
    /// reproduces the locked volume, which still needs the original
    /// BitLocker key/recovery password to unlock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitlocker: Option<String>,
    /// GPT partition **unique** GUID (`PartitionId`) as a dashed string.
    /// Restored verbatim so a cloned system disk keeps its BCD device
    /// references (the BCD identifies boot/OS partitions by this GUID on
    /// GPT). Absent for MBR sources and pre-fidelity backups — restore then
    /// leaves PartitionId zeroed and Windows generates a fresh one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_guid: Option<String>,
    /// GPT `Attributes` bits (PlatformRequired / Hidden / NoDriveLetter …)
    /// captured from the source partition table and restored verbatim, so
    /// e.g. a Recovery partition stays hidden/no-auto-mount after restore.
    /// Absent for MBR sources and pre-fidelity backups (restored as 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpt_attributes: Option<u64>,
    pub chunks: Vec<ChunkRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitmap_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub chunk_index: u32,
    pub extent_index: u32,
    pub uncompressed_len: u32,
    pub blake3: String,
}

impl BackupManifest {
    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|e| PhoenixError::Manifest(e.to_string()))
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| PhoenixError::Manifest(e.to_string()))
    }

    pub fn manifest_hash(bytes: &[u8]) -> [u8; 32] {
        hash::hash_bytes(bytes)
    }
}

pub fn fs_kind_to_string(k: FilesystemKind) -> &'static str {
    match k {
        FilesystemKind::Unknown => "unknown",
        FilesystemKind::Ntfs => "ntfs",
        FilesystemKind::Fat => "fat",
        FilesystemKind::Exfat => "exfat",
        FilesystemKind::Efi => "efi",
        FilesystemKind::Msr => "msr",
        FilesystemKind::Bitlocker => "bitlocker",
        FilesystemKind::Refs => "refs",
    }
}

pub fn capture_mode_to_string(m: CaptureMode) -> &'static str {
    match m {
        CaptureMode::Raw => "raw",
        CaptureMode::UsedBlocks => "used-blocks",
    }
}

/// Manifest encoding of [`BitlockerState`]; `None` for non-BitLocker
/// partitions so the field is omitted from the JSON entirely (keeping old
/// and new manifests byte-compatible for the common case).
pub fn bitlocker_state_to_manifest(s: BitlockerState) -> Option<String> {
    match s {
        BitlockerState::None => None,
        BitlockerState::Unlocked => Some("unlocked".to_string()),
        BitlockerState::Locked => Some("locked".to_string()),
    }
}

pub fn bitlocker_state_from_manifest(s: Option<&str>) -> BitlockerState {
    match s {
        Some("unlocked") => BitlockerState::Unlocked,
        Some("locked") => BitlockerState::Locked,
        _ => BitlockerState::None,
    }
}

pub fn fs_kind_from_string(s: &str) -> FilesystemKind {
    match s {
        "ntfs" => FilesystemKind::Ntfs,
        "fat" => FilesystemKind::Fat,
        "exfat" => FilesystemKind::Exfat,
        "efi" => FilesystemKind::Efi,
        "msr" => FilesystemKind::Msr,
        "bitlocker" => FilesystemKind::Bitlocker,
        "refs" => FilesystemKind::Refs,
        _ => FilesystemKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-BitLocker-support manifest (no `bitlocker` field) must still
    /// deserialize, with the state defaulting to "not BitLocker".
    #[test]
    fn partition_manifest_without_bitlocker_field_deserializes() {
        let json = r#"{
            "index": 0,
            "name": "Basic data partition",
            "fs": "ntfs",
            "capture_mode": "used-blocks",
            "original_size": 1048576,
            "used_bytes": 4096,
            "chunks": []
        }"#;
        let pm: PartitionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(pm.bitlocker, None);
        assert_eq!(
            bitlocker_state_from_manifest(pm.bitlocker.as_deref()),
            BitlockerState::None
        );
    }

    #[test]
    fn bitlocker_state_manifest_roundtrip() {
        for state in [
            BitlockerState::None,
            BitlockerState::Unlocked,
            BitlockerState::Locked,
        ] {
            let encoded = bitlocker_state_to_manifest(state);
            assert_eq!(bitlocker_state_from_manifest(encoded.as_deref()), state);
        }
        // Unrecognized values from a future format degrade safely.
        assert_eq!(
            bitlocker_state_from_manifest(Some("suspended")),
            BitlockerState::None
        );
    }

    #[test]
    fn fs_kind_string_roundtrip_covers_every_kind() {
        for kind in [
            FilesystemKind::Unknown,
            FilesystemKind::Ntfs,
            FilesystemKind::Fat,
            FilesystemKind::Exfat,
            FilesystemKind::Efi,
            FilesystemKind::Msr,
            FilesystemKind::Bitlocker,
            FilesystemKind::Refs,
        ] {
            assert_eq!(kind, fs_kind_from_string(fs_kind_to_string(kind)));
        }
        assert_eq!(fs_kind_to_string(FilesystemKind::Refs), "refs");
        // Old readers see an unknown string, not a crash.
        assert_eq!(fs_kind_from_string("zfs"), FilesystemKind::Unknown);
    }
}
