use serde::{Deserialize, Serialize};

use phoenix_core::error::{PhoenixError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    pub backup_path: String,
    pub target_disk_index: u32,
    pub entries: Vec<RestorePlanEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlanEntry {
    pub source_partition_index: u32,
    pub restore: bool,
    pub target_offset_bytes: u64,
    pub target_size_bytes: u64,
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

    pub fn validate_against_backup(
        &self,
        manifest: &phoenix_core::manifest::BackupManifest,
    ) -> Result<()> {
        for entry in &self.entries {
            if !entry.restore {
                continue;
            }
            let part = manifest
                .partitions
                .iter()
                .find(|p| p.index == entry.source_partition_index)
                .ok_or_else(|| {
                    PhoenixError::Plan(format!(
                        "partition {} not in backup",
                        entry.source_partition_index
                    ))
                })?;
            if part.used_bytes > entry.target_size_bytes {
                return Err(PhoenixError::PartitionTooSmall {
                    partition_index: entry.source_partition_index,
                    target_size: entry.target_size_bytes,
                    required: part.used_bytes,
                });
            }
        }
        Ok(())
    }
}

pub fn default_plan_from_backup(
    backup_path: &str,
    reader: &phoenix_core::container::PhnxReader,
    target_disk_index: u32,
    target_disk_size: u64,
) -> RestorePlan {
    let mut offset = if reader.header.flags & 1 != 0 {
        1024 * 1024 // 1 MiB alignment for GPT
    } else {
        1024 * 512
    };

    let entries: Vec<RestorePlanEntry> = reader
        .index
        .iter()
        .map(|e| {
            let entry = RestorePlanEntry {
                source_partition_index: e.index,
                restore: true,
                target_offset_bytes: offset,
                target_size_bytes: e.original_size,
            };
            offset += e.original_size;
            entry
        })
        .collect();

    let _ = (target_disk_index, target_disk_size);
    RestorePlan {
        backup_path: backup_path.to_string(),
        target_disk_index,
        entries,
    }
}
