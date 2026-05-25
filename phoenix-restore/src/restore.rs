use std::path::Path;

use phoenix_capture::fat::restore_fat;
use phoenix_capture::ntfs::restore_ntfs;
use phoenix_capture::raw::{restore_raw, PartitionWriter};
use phoenix_core::container::PhnxReader;
use phoenix_core::disk::{enumerate_disks, FilesystemKind};
use phoenix_core::error::Result;
use phoenix_core::ProgressHandle;
use phoenix_core::manifest::fs_kind_from_string;
use tracing::info;

use crate::partition_table::apply_gpt_layout;
use crate::plan::RestorePlan;

pub struct RestoreOptions {
    pub backup_path: std::path::PathBuf,
    pub plan: RestorePlan,
    pub verify_on_restore: bool,
    pub progress: Option<ProgressHandle>,
}

pub fn run_restore(opts: RestoreOptions) -> Result<()> {
    let mut reader = PhnxReader::open(&opts.backup_path)?;
    opts.plan
        .validate_against_backup(&reader.manifest)?;

    let disks = enumerate_disks()?;
    let disk = disks
        .into_iter()
        .find(|d| d.index == opts.plan.target_disk_index)
        .ok_or_else(|| phoenix_core::error::PhoenixError::Disk("target disk not found".into()))?;

    info!("Restoring to disk {} ({})", disk.index, disk.path);

    if disk.is_gpt {
        if let Some(ref p) = opts.progress {
            p.set_phase("Writing partition layout");
        }
        apply_gpt_layout(&disk.path, &opts.plan)?;
    }

    let restoring: Vec<_> = opts.plan.entries.iter().filter(|e| e.restore).collect();
    let total_chunks: u64 = restoring
        .iter()
        .filter_map(|entry| {
            reader
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == entry.source_partition_index)
        })
        .map(|p| p.chunks.len() as u64)
        .sum();

    if let Some(ref p) = opts.progress {
        p.begin(total_chunks.max(1), "Restore");
    }

    let mut chunks_done = 0u64;

    for entry in &opts.plan.entries {
        if !entry.restore {
            continue;
        }
        // Honor cancel between partitions — restore reopens the disk
        // handle for each partition, so a check here lets us bail before
        // any extra Win32 setup work.
        if let Some(ref p) = opts.progress {
            if p.is_cancelled() {
                return Err(phoenix_core::error::PhoenixError::Cancelled);
            }
        }
        let idx_entry = reader
            .index
            .iter()
            .find(|e| e.index == entry.source_partition_index)
            .cloned()
            .ok_or_else(|| {
                phoenix_core::error::PhoenixError::Plan("partition index missing".into())
            })?;

        let part_manifest = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == entry.source_partition_index)
            .unwrap();

        let fs = fs_kind_from_string(&part_manifest.fs);
        let mut writer =
            PartitionWriter::open_disk(&disk.path, entry.target_offset_bytes)?;

        if let Some(ref p) = opts.progress {
            p.set_phase(format!(
                "Restoring partition {} ({})",
                entry.source_partition_index, idx_entry.name
            ));
        }

        info!(
            "Restoring partition {} ({}) to offset {}",
            entry.source_partition_index, idx_entry.name, entry.target_offset_bytes
        );

        match fs {
            FilesystemKind::Ntfs => {
                chunks_done += restore_ntfs(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    entry.target_size_bytes,
                    opts.verify_on_restore,
                    opts.progress.as_ref(),
                    chunks_done,
                )?;
            }
            FilesystemKind::Fat | FilesystemKind::Exfat => {
                chunks_done += restore_fat(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    entry.target_size_bytes,
                    opts.verify_on_restore,
                    opts.progress.as_ref(),
                    chunks_done,
                )?;
            }
            _ => {
                chunks_done += restore_raw(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    opts.verify_on_restore,
                    opts.progress.as_ref(),
                    chunks_done,
                )?;
            }
        }
    }

    if let Some(ref p) = opts.progress {
        p.end();
    }
    info!("Restore complete");
    Ok(())
}

pub fn verify_backup(path: &Path, quick: bool) -> Result<()> {
    verify_backup_with_progress(path, quick, None)
}

pub fn verify_backup_with_progress(
    path: &Path,
    quick: bool,
    progress: Option<ProgressHandle>,
) -> Result<()> {
    let mut reader = PhnxReader::open(path)?;
    reader.verify_all_with_progress(quick, progress)?;
    Ok(())
}
