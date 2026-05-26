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

    /// Confirm that every extent we plan to stream lands inside its
    /// chosen target partition. Required because `validate_against_backup`
    /// only looks at total `used_bytes`, but on a *shrink* the source's
    /// used clusters can still be physically positioned past the new
    /// partition boundary even when the byte total fits. Without this
    /// guard `restore_raw` would happily seek past the partition end and
    /// scribble over whatever lives there next on the disk (the next
    /// partition, the backup GPT, etc.) — a much louder kind of failure
    /// than "your volume came up RAW".
    ///
    /// For grows (target ≥ source) every extent trivially fits, so we
    /// short-circuit. For shrinks we read the per-partition stream
    /// header (cheap — small, near the start of the .phnx) and walk its
    /// extent table.
    pub fn validate_extents_fit(
        &self,
        reader: &mut phoenix_core::container::PhnxReader,
    ) -> Result<()> {
        for entry in &self.entries {
            if !entry.restore {
                continue;
            }
            let idx_entry = reader
                .index
                .iter()
                .find(|e| e.index == entry.source_partition_index)
                .cloned()
                .ok_or_else(|| {
                    PhoenixError::Plan(format!(
                        "partition {} not in backup index",
                        entry.source_partition_index
                    ))
                })?;
            if entry.target_size_bytes >= idx_entry.original_size {
                continue;
            }
            let stream = reader.read_stream_header(&idx_entry)?;
            let sector_size = idx_entry.sector_size as u64;
            if sector_size == 0 {
                return Err(PhoenixError::Plan(format!(
                    "partition {} has zero sector_size in backup index",
                    entry.source_partition_index
                )));
            }

            // NTFS shrinks now go through relocation (Phase B). Run the
            // builder as a pre-flight: if it returns an error, the
            // shrink is genuinely impossible (insufficient free space)
            // and we surface that here. If it returns Ok(_), the
            // shrink is feasible and we let the actual restore path
            // re-run the build with the same inputs to apply it. This
            // duplicates the build work once, but keeps validate_*
            // pure (it doesn't take ownership of the map) and lets us
            // fail before any disk write happens.
            if matches!(idx_entry.fs_kind, phoenix_core::disk::FilesystemKind::Ntfs) {
                let cluster_size = stream.bytes_per_cluster as u64;
                match crate::relocation::build_relocation_map(
                    &stream.extents,
                    sector_size,
                    cluster_size,
                    entry.target_size_bytes,
                ) {
                    Ok(_) => {
                        tracing::info!(
                            partition = entry.source_partition_index,
                            target_size = entry.target_size_bytes,
                            "NTFS shrink relocation pre-flight OK"
                        );
                        continue;
                    }
                    Err(e) => {
                        return Err(PhoenixError::Plan(format!(
                            "NTFS partition {} cannot be shrunk to {} bytes: {e}. Pick a larger \
                             target size, or restore at the original size.",
                            entry.source_partition_index, entry.target_size_bytes
                        )));
                    }
                }
            }

            let max_sector = entry.target_size_bytes / sector_size;

            // Collect every overflowing extent up front rather than bailing
            // on the first one. This lets us:
            //   1. Tell the user how *many* extents are out of bounds (a
            //      single one is "bad luck", a hundred is "your manifest
            //      may be corrupted").
            //   2. Embed a few sample offenders directly in the error
            //      message, so the user has actionable detail without
            //      needing to re-run `phoenix-cli inspect`.
            //   3. Surface the entire extent table at WARN level for the
            //      same forensic reason — a single overflow with a value
            //      near `u64::MAX` is the smoking gun for manifest
            //      corruption / a capture-side bug rather than a real
            //      shrink-doesn't-fit case.
            let mut offending = Vec::new();
            for (idx, ext) in stream.extents.iter().enumerate() {
                let last_sector = ext.start_sector.saturating_add(ext.sector_count);
                if last_sector > max_sector {
                    offending.push((idx, ext.start_sector, ext.sector_count, last_sector));
                }
            }
            if !offending.is_empty() {
                tracing::warn!(
                    partition = entry.source_partition_index,
                    target_size = entry.target_size_bytes,
                    max_sector,
                    total_extents = stream.extents.len(),
                    overflowing_extents = offending.len(),
                    "validate_extents_fit: extents past target boundary; dumping full extent table at WARN level for forensics"
                );
                for (idx, ext) in stream.extents.iter().enumerate() {
                    let last = ext.start_sector.saturating_add(ext.sector_count);
                    tracing::warn!(
                        partition = entry.source_partition_index,
                        extent_index = idx,
                        start_sector = ext.start_sector,
                        sector_count = ext.sector_count,
                        last_sector = last,
                        "extent dump"
                    );
                }
                let suspicious_threshold = u64::MAX - (1u64 << 50);
                let any_suspicious = offending.iter().any(|(_, s, c, l)| {
                    *s > suspicious_threshold
                        || *c > suspicious_threshold
                        || *l > suspicious_threshold
                });
                let sample = offending
                    .iter()
                    .take(3)
                    .map(|(i, s, c, l)| {
                        format!(
                            "extent[{i}]: start={s}, count={c}, last_sector={l}"
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let suspicious_hint = if any_suspicious {
                    " — one or more values are within 1 PiB of u64::MAX, which is impossible \
                     for any real storage device and almost certainly indicates a corrupted \
                     manifest. Run `phoenix-cli inspect <backup.phnx>` to dump the full extent \
                     table"
                } else {
                    ""
                };
                return Err(PhoenixError::Plan(format!(
                    "partition {} cannot shrink to {} bytes: {} of {} extents reach past the \
                     new partition end at sector {} (sample: {}){}",
                    entry.source_partition_index,
                    entry.target_size_bytes,
                    offending.len(),
                    stream.extents.len(),
                    max_sector,
                    sample,
                    suspicious_hint,
                )));
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
