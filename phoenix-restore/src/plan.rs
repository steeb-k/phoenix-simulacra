use serde::{Deserialize, Deserializer, Serialize};

use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{DiskInfo, FilesystemKind};
use phoenix_core::error::{PhoenixError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    pub backup_path: String,
    pub target_disk_index: u32,
    pub entries: Vec<RestorePlanEntry>,
    /// When true, the GUI built a full-disk layout from the backup.
    #[serde(default)]
    pub full_disk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlanEntry {
    #[serde(default, deserialize_with = "deserialize_source_partition")]
    pub source_partition_index: Option<u32>,
    #[serde(default)]
    pub target_partition_index: u32,
    #[serde(default = "default_true")]
    pub restore: bool,
    pub target_offset_bytes: u64,
    pub target_size_bytes: u64,
}

fn default_true() -> bool {
    true
}

/// TOML plans may use `source_partition_index = 2` (legacy integer) or omit it.
fn deserialize_source_partition<'de, D>(deserializer: D) -> std::result::Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SourceField {
        Present(u32),
        Opt(Option<u32>),
    }
    match SourceField::deserialize(deserializer)? {
        SourceField::Present(n) => Ok(Some(n)),
        SourceField::Opt(o) => Ok(o),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    FullDisk,
    Partial,
}

impl RestorePlan {
    pub fn from_toml(path: &std::path::Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| PhoenixError::Plan(format!("read plan: {e}")))?;
        toml::from_str(&s).map_err(|e| PhoenixError::Plan(format!("parse plan: {e}")))
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| PhoenixError::Plan(format!("serialize plan: {e}")))
    }

    pub fn mode(&self) -> RestoreMode {
        if self.full_disk {
            return RestoreMode::FullDisk;
        }
        let restoring = self.entries.iter().filter(|e| e.restore).count();
        let with_source = self
            .entries
            .iter()
            .filter(|e| e.restore && e.source_partition_index.is_some())
            .count();
        if restoring > 0 && with_source == restoring && self.covers_all_backup_partitions() {
            RestoreMode::FullDisk
        } else {
            RestoreMode::Partial
        }
    }

    fn covers_all_backup_partitions(&self) -> bool {
        // Heuristic without reader: full_disk flag is authoritative when set.
        false
    }

    pub fn layout_entries(&self) -> Vec<&RestorePlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.restore || self.mode() == RestoreMode::Partial)
            .collect()
    }

    pub fn validate_against_backup(
        &self,
        manifest: &phoenix_core::manifest::BackupManifest,
    ) -> Result<()> {
        for entry in &self.entries {
            if !entry.restore {
                continue;
            }
            let Some(src) = entry.source_partition_index else {
                return Err(PhoenixError::Plan(format!(
                    "target partition {} is marked restore but has no source mapping",
                    entry.target_partition_index
                )));
            };
            let part = manifest
                .partitions
                .iter()
                .find(|p| p.index == src)
                .ok_or_else(|| PhoenixError::Plan(format!("partition {src} not in backup")))?;
            if part.used_bytes > entry.target_size_bytes {
                return Err(PhoenixError::PartitionTooSmall {
                    partition_index: src,
                    target_size: entry.target_size_bytes,
                    required: part.used_bytes,
                });
            }
        }
        Ok(())
    }

    pub fn validate_extents_fit(
        &self,
        reader: &mut phoenix_core::container::PhnxReader,
    ) -> Result<()> {
        for entry in &self.entries {
            if !entry.restore {
                continue;
            }
            let Some(src) = entry.source_partition_index else {
                continue;
            };
            let idx_entry = reader
                .index
                .iter()
                .find(|e| e.index == src)
                .cloned()
                .ok_or_else(|| PhoenixError::Plan(format!("partition {src} not in backup index")))?;
            if entry.target_size_bytes >= idx_entry.original_size {
                continue;
            }
            let stream = reader.read_stream_header(&idx_entry)?;
            let sector_size = idx_entry.sector_size as u64;
            if sector_size == 0 {
                return Err(PhoenixError::Plan(format!(
                    "partition {src} has zero sector_size in backup index"
                )));
            }

            if matches!(idx_entry.fs_kind, FilesystemKind::Ntfs) {
                let cluster_size = stream.bytes_per_cluster as u64;
                match crate::relocation::build_relocation_map(
                    &stream.extents,
                    sector_size,
                    cluster_size,
                    entry.target_size_bytes,
                ) {
                    Ok(_) => continue,
                    Err(e) => {
                        return Err(PhoenixError::Plan(format!(
                            "NTFS partition {src} cannot be shrunk to {} bytes: {e}",
                            entry.target_size_bytes
                        )));
                    }
                }
            }

            let max_sector = entry.target_size_bytes / sector_size;
            let mut offending = Vec::new();
            for (idx, ext) in stream.extents.iter().enumerate() {
                let last_sector = ext.start_sector.saturating_add(ext.sector_count);
                if last_sector > max_sector {
                    offending.push((idx, ext.start_sector, ext.sector_count, last_sector));
                }
            }
            if !offending.is_empty() {
                let sample = offending
                    .iter()
                    .take(3)
                    .map(|(i, s, c, l)| format!("extent[{i}]: start={s}, count={c}, last_sector={l}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(PhoenixError::Plan(format!(
                    "partition {src} cannot shrink to {} bytes: {} of {} extents past sector {} ({sample})",
                    entry.target_size_bytes,
                    offending.len(),
                    stream.extents.len(),
                    max_sector,
                )));
            }
        }
        Ok(())
    }
}

pub fn alignment_bytes(reader: &PhnxReader) -> u64 {
    if reader.header.flags & 1 != 0 {
        1024 * 1024
    } else {
        1024 * 512
    }
}

pub fn align_up(value: u64, align: u64) -> u64 {
    if align == 0 {
        return value;
    }
    ((value + align - 1) / align) * align
}

pub fn default_plan_from_backup(
    backup_path: &str,
    reader: &PhnxReader,
    target_disk_index: u32,
    target_disk_size: u64,
) -> RestorePlan {
    build_full_disk_plan(backup_path, reader, target_disk_index, target_disk_size)
}

pub fn build_full_disk_plan(
    backup_path: &str,
    reader: &PhnxReader,
    target_disk_index: u32,
    target_disk_size: u64,
) -> RestorePlan {
    let align = alignment_bytes(reader);
    let mut offset = align;
    let mut entries: Vec<RestorePlanEntry> = Vec::new();

    for e in &reader.index {
        entries.push(RestorePlanEntry {
            source_partition_index: Some(e.index),
            target_partition_index: e.index,
            restore: true,
            target_offset_bytes: offset,
            target_size_bytes: e.original_size,
        });
        offset += e.original_size;
    }

    expand_ntfs_slack(&mut entries, reader, target_disk_size, align);

    RestorePlan {
        backup_path: backup_path.to_string(),
        target_disk_index,
        entries,
        full_disk: true,
    }
}

fn expand_ntfs_slack(
    entries: &mut [RestorePlanEntry],
    reader: &PhnxReader,
    target_disk_size: u64,
    align: u64,
) {
    let used: u64 = entries.iter().map(|e| e.target_size_bytes).sum();
    let slack = target_disk_size.saturating_sub(used);
    if slack < align {
        return;
    }

    let ntfs_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.restore
                && reader
                    .index
                    .iter()
                    .find(|i| i.index == e.source_partition_index.unwrap_or(u32::MAX))
                    .is_some_and(|i| i.fs_kind == FilesystemKind::Ntfs)
        })
        .map(|(i, _)| i)
        .collect();

    if ntfs_indices.is_empty() {
        return;
    }

    let total_ntfs_orig: u64 = ntfs_indices
        .iter()
        .map(|&i| entries[i].target_size_bytes)
        .sum();

    if ntfs_indices.len() == 1 {
        entries[ntfs_indices[0]].target_size_bytes =
            align_up(entries[ntfs_indices[0]].target_size_bytes + slack, align);
        return;
    }

    let mut remaining = slack;
    for (pos, &idx) in ntfs_indices.iter().enumerate() {
        let share = if pos + 1 == ntfs_indices.len() {
            remaining
        } else if total_ntfs_orig > 0 {
            slack * entries[idx].target_size_bytes / total_ntfs_orig
        } else {
            slack / ntfs_indices.len() as u64
        };
        remaining = remaining.saturating_sub(share);
        entries[idx].target_size_bytes = align_up(entries[idx].target_size_bytes + share, align);
    }
}

/// Build a plan covering every target partition; only mapped slots have `restore: true`.
pub fn build_partial_plan(
    backup_path: &str,
    target_disk_index: u32,
    target: &DiskInfo,
    assignments: &[(u32, u32, u64, u64)], // target_part, source_part, offset, size
) -> RestorePlan {
    let mut entries: Vec<RestorePlanEntry> = target
        .partitions
        .iter()
        .map(|p| {
            let mapped = assignments.iter().find(|(t, _, _, _)| *t == p.index);
            RestorePlanEntry {
                source_partition_index: mapped.map(|(_, s, _, _)| *s),
                target_partition_index: p.index,
                restore: mapped.is_some(),
                target_offset_bytes: mapped.map(|(_, _, o, _)| *o).unwrap_or(p.offset_bytes),
                target_size_bytes: mapped.map(|(_, _, _, sz)| *sz).unwrap_or(p.size_bytes),
            }
        })
        .collect();
    entries.sort_by_key(|e| e.target_offset_bytes);

    RestorePlan {
        backup_path: backup_path.to_string(),
        target_disk_index,
        entries,
        full_disk: false,
    }
}

pub fn partition_allows_resize(fs: FilesystemKind) -> bool {
    matches!(
        fs,
        FilesystemKind::Ntfs | FilesystemKind::Fat | FilesystemKind::Exfat
    )
}
