use chrono::Utc;
use phoenix_core::container::{Extent, Header, PhnxWriter, FORMAT_VERSION};
use phoenix_core::disk::{
    enumerate_disks, refine_partition_fs, CaptureMode, FilesystemKind, PartitionInfo,
};
use phoenix_core::error::Result;
use phoenix_core::manifest::{
    capture_mode_to_string, fs_kind_to_string, BackupManifest, DiskManifest, PartitionManifest,
};
use phoenix_core::disk::guid_to_string;
use phoenix_core::ProgressHandle;
use tracing::info;
use uuid::Uuid;

use crate::fat::{capture_exfat, capture_fat};
use crate::ntfs::{capture_ntfs, ntfs_extents};
use crate::raw::{capture_raw, raw_extent_for_partition};
use crate::reader::PartitionReader;

pub struct BackupOptions {
    pub disk_index: u32,
    pub partition_indices: Vec<u32>,
    pub output: std::path::PathBuf,
    pub use_vss: bool,
    pub progress: Option<ProgressHandle>,
}

pub fn run_backup(opts: BackupOptions) -> Result<()> {
    let mut disks = enumerate_disks()?;
    for disk in &mut disks {
        for part in &mut disk.partitions {
            refine_partition_fs(part);
        }
    }

    let disk = disks
        .into_iter()
        .find(|d| d.index == opts.disk_index)
        .ok_or_else(|| phoenix_core::error::PhoenixError::Disk("disk not found".into()))?;

    let selected: Vec<&PartitionInfo> = disk
        .partitions
        .iter()
        .filter(|p| opts.partition_indices.contains(&p.index))
        .collect();

    if selected.is_empty() {
        return Err(phoenix_core::error::PhoenixError::Disk(
            "no partitions selected".into(),
        ));
    }

    let backup_id = Uuid::new_v4();
    let header = Header {
        version: FORMAT_VERSION,
        flags: if disk.is_gpt { 1 } else { 0 },
        timestamp: Utc::now().timestamp() as u64,
        backup_id,
        disk_signature: disk.disk_signature,
        partition_count: 0,
    };

    // Estimate the total bytes we'll actually stream. For UsedBlocks
    // partitions we can use the OS-reported used bytes from
    // `GetDiskFreeSpaceExW`, which is dramatically smaller than the raw
    // partition size on a half-empty drive. Falling back to `size_bytes`
    // keeps progress bounded even when the volume isn't mounted (raw / EFI /
    // BitLocker-locked partitions all stream the full size).
    let total_bytes: u64 = selected
        .iter()
        .map(|p| match p.capture_mode {
            CaptureMode::UsedBlocks => p
                .usage
                .map(|u| u.used_bytes())
                .filter(|n| *n > 0)
                .unwrap_or(p.size_bytes),
            CaptureMode::Raw => p.size_bytes,
        })
        .sum();
    if let Some(ref progress) = opts.progress {
        progress.begin(total_bytes.max(1), "Backup");
    }

    let mut writer =
        PhnxWriter::create_with_progress(&opts.output, header, opts.progress.clone())?;
    let mut partition_manifests = Vec::new();
    let partition_count = selected.len();
    // Owns every shadow we create across all selected partitions; its Drop
    // tears them down even if we error or panic out of this loop.
    let mut vss = phoenix_vss::VssSession::new();

    for (part_idx, part) in selected.iter().enumerate() {
        // Short-circuit between partitions so a cancel during a long
        // VSS snapshot setup or partition reader open doesn't have to
        // wait until the next chunk write to honor the user's click.
        if let Some(ref progress) = opts.progress {
            if progress.is_cancelled() {
                return Err(phoenix_core::error::PhoenixError::Cancelled);
            }
            progress.set_phase(format!(
                "Partition {} — {} ({}/{})",
                part.index,
                part.name,
                part_idx + 1,
                partition_count
            ));
        }

        info!(
            "Backing up partition {} ({})",
            part.index, part.name
        );

        let read_path = if opts.use_vss {
            if let Some(ref vol) = part.volume_path {
                vss.snapshot_volume(vol)?
            } else {
                disk.path.clone()
            }
        } else if let Some(ref vol) = part.volume_path {
            vol.clone()
        } else {
            disk.path.clone()
        };

        let mut reader = if read_path.contains("PhysicalDrive") {
            PartitionReader::open_disk_partition(&read_path, part.offset_bytes, part.size_bytes)?
        } else {
            PartitionReader::open_volume(&read_path)?
        };

        let (extents, bytes_per_cluster, capture_mode, fs_kind) =
            plan_capture(part, &mut reader)?;

        let mut stream = writer.begin_partition_stream(
            part.index,
            part.type_guid,
            part.name.clone(),
            part.size_bytes,
            fs_kind,
            capture_mode,
            disk.sector_size,
            0,
            &extents,
            bytes_per_cluster,
        )?;

        let (used_bytes, bitmap_hash) = match (fs_kind, capture_mode) {
            (FilesystemKind::Ntfs, CaptureMode::UsedBlocks) => {
                capture_ntfs(&mut reader, &mut stream)?
            }
            (FilesystemKind::Fat, CaptureMode::UsedBlocks) => {
                capture_fat(&mut reader, &mut stream, false)?
            }
            (FilesystemKind::Exfat, CaptureMode::UsedBlocks) => {
                capture_exfat(&mut reader, &mut stream)?
            }
            _ => {
                capture_raw(&mut reader, &mut stream)?;
                (reader.length(), None)
            }
        };

        let chunks = crate::raw::finish_stream(stream)?;

        writer.set_last_partition_used_bytes(used_bytes);

        partition_manifests.push(PartitionManifest {
            index: part.index,
            name: part.name.clone(),
            type_guid: Some(guid_to_string(&part.type_guid)),
            fs: fs_kind_to_string(fs_kind).to_string(),
            capture_mode: capture_mode_to_string(capture_mode).to_string(),
            original_size: part.size_bytes,
            used_bytes,
            chunks,
            bitmap_hash,
        });
    }

    let hostname = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into());
    let manifest = BackupManifest {
        format_version: 1,
        backup_id,
        parent_backup_id: None,
        hostname,
        disk: DiskManifest {
            style: if disk.is_gpt {
                "gpt".into()
            } else {
                "mbr".into()
            },
            disk_guid: disk.disk_guid.map(|g| guid_to_string(&g)),
            sector_size: disk.sector_size,
        },
        partitions: partition_manifests,
    };

    if let Some(ref progress) = opts.progress {
        progress.set_phase("Finalizing backup file");
    }
    writer.finalize(&manifest)?;
    if let Some(ref progress) = opts.progress {
        progress.end();
    }
    info!("Backup written to {}", opts.output.display());
    Ok(())
}

fn plan_capture(
    part: &PartitionInfo,
    reader: &mut PartitionReader,
) -> Result<(Vec<Extent>, u32, CaptureMode, FilesystemKind)> {
    let fs = part.fs_kind;
    let mode = part.capture_mode;
    match (fs, mode) {
        (FilesystemKind::Ntfs, CaptureMode::UsedBlocks) => {
            let extents = ntfs_extents(reader)?;
            Ok((extents, 4096, mode, fs))
        }
        (FilesystemKind::Fat | FilesystemKind::Exfat, CaptureMode::UsedBlocks) => {
            Ok((vec![Extent {
                start_sector: 0,
                sector_count: part.size_bytes / 512,
            }], 0, mode, fs))
        }
        _ => Ok((
            raw_extent_for_partition(part.size_bytes, 512),
            0,
            CaptureMode::Raw,
            fs,
        )),
    }
}
