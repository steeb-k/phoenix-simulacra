use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::disk::{CaptureMode, FilesystemKind};
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
        serde_json::to_vec_pretty(self)
            .map_err(|e| PhoenixError::Manifest(e.to_string()))
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
    }
}

pub fn capture_mode_to_string(m: CaptureMode) -> &'static str {
    match m {
        CaptureMode::Raw => "raw",
        CaptureMode::UsedBlocks => "used-blocks",
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
        _ => FilesystemKind::Unknown,
    }
}
