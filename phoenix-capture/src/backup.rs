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
use crate::ntfs::{capture_ntfs, ntfs_plan};
use crate::raw::{capture_raw, raw_extent_for_partition};
use crate::reader::PartitionReader;

pub struct BackupOptions {
    pub disk_index: u32,
    pub partition_indices: Vec<u32>,
    pub output: std::path::PathBuf,
    pub use_vss: bool,
    pub progress: Option<ProgressHandle>,
}

/// A partition that has been resolved to a concrete read path, opened,
/// planned (extents + capture mode + bitmap hash) and — if applicable —
/// volume-locked. `run_backup` builds one of these per selected partition
/// during its pre-flight phase before any data streaming starts, so a lock
/// failure or a planning failure on partition N can't strand a backup that
/// has already streamed partitions 1..N-1.
///
/// The planning step **must** run before [`PartitionReader::lock_volume`]:
/// `FSCTL_GET_VOLUME_BITMAP` (used by [`ntfs_plan`] to compute used-cluster
/// extents) returns `ERROR_NOT_READY` on a locked NTFS volume on at least
/// some Windows builds (USB-attached drives reproduce reliably). Doing the
/// bitmap query while the volume is still unlocked sidesteps that and also
/// resolves the previously-flagged two-bitmap-reads-can-disagree race —
/// `capture_ntfs` re-uses these extents instead of re-reading the bitmap.
struct PreparedPartition<'a> {
    part: &'a PartitionInfo,
    reader: PartitionReader,
    /// Sector-aligned used-data extents written into the partition manifest
    /// and re-iterated by `capture_ntfs` / `capture_raw`. For NTFS this is
    /// the bitmap-derived used-cluster set; for FAT/exFAT and raw mode it
    /// is the partition-spanning placeholder produced by `plan_capture`.
    extents: Vec<Extent>,
    bytes_per_cluster: u32,
    capture_mode: CaptureMode,
    fs_kind: FilesystemKind,
    /// Pre-computed blake3 hash of the NTFS allocation bitmap, propagated
    /// through to the partition manifest by `capture_ntfs`. `None` for
    /// non-NTFS partitions and for any path where the bitmap query
    /// degraded to the all-clusters-used fallback.
    bitmap_hash: Option<String>,
    /// True iff the reader is reading the live mounted volume (rather than
    /// a VSS shadow or a raw `\\.\PhysicalDriveN` handle). Live readers
    /// carry an `FSCTL_LOCK_VOLUME` for the duration of the backup; shadow
    /// / raw readers don't (the shadow is already a frozen snapshot, and
    /// the raw disk path isn't a mounted volume in this OS).
    #[allow(dead_code)]
    is_live: bool,
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

    // ---- PHASE 1: pre-flight ----
    //
    // For every selected partition: resolve the read path (VSS shadow when
    // available, otherwise live volume / raw disk), open the reader, and if
    // we ended up on a live mounted volume hold an exclusive
    // `FSCTL_LOCK_VOLUME` on it. We do this entirely up front so that a
    // lock failure on partition N can't strand a backup that has already
    // burned hours streaming partitions 1..N-1.
    //
    // On any error (cancel, lock failure, open failure) the partial
    // `prepared` Vec drops below, which releases every lock we already
    // acquired and closes every handle we already opened — no cleanup
    // bookkeeping needed at the call sites.
    if let Some(ref progress) = opts.progress {
        progress.set_phase("Preparing volumes");
    }
    let mut prepared: Vec<PreparedPartition<'_>> = Vec::with_capacity(partition_count);
    for (idx, part) in selected.iter().enumerate() {
        if let Some(ref progress) = opts.progress {
            if progress.is_cancelled() {
                return Err(phoenix_core::error::PhoenixError::Cancelled);
            }
        }

        let original_volume = part.volume_path.clone();
        let read_path = if opts.use_vss {
            if let Some(ref vol) = original_volume {
                vss.snapshot_volume(vol)?
            } else {
                disk.path.clone()
            }
        } else if let Some(ref vol) = original_volume {
            vol.clone()
        } else {
            disk.path.clone()
        };

        // Live iff we ended up reading the partition's original mounted
        // drive-letter path. When VSS Create succeeded `read_path` is
        // rewritten to `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`
        // (already a frozen snapshot — no lock needed). When VSS was off
        // or fell back, `read_path` equals `original_volume` and we want
        // the lock.
        let is_live = original_volume
            .as_deref()
            .map(|orig| orig == read_path.as_str())
            .unwrap_or(false);

        let mut reader = if read_path.contains("PhysicalDrive") {
            PartitionReader::open_disk_partition(&read_path, part.offset_bytes, part.size_bytes)?
        } else {
            PartitionReader::open_volume(&read_path)?
        };

        // Plan BEFORE locking. For NTFS, `plan_capture` issues
        // `FSCTL_GET_VOLUME_BITMAP` via the reader, and that IOCTL fails
        // with `ERROR_NOT_READY` on a locked volume on some Windows builds
        // (USB-attached NTFS reproduces reliably). For FAT/exFAT/raw the
        // ordering is irrelevant — they only do raw reads, which work on
        // either a locked or unlocked handle — but doing the planning
        // uniformly up front keeps the lifecycle simple to reason about.
        let (extents, bytes_per_cluster, capture_mode, fs_kind, bitmap_hash) =
            plan_capture(part, &mut reader)?;

        if is_live {
            // Use the original drive-letter path as the user-facing label
            // (e.g. `\\.\E:`); we don't want shadow GLOBALROOT junk in the
            // error message even though we'd never actually take this
            // branch with a shadow path.
            let label = original_volume
                .as_deref()
                .unwrap_or(read_path.as_str())
                .to_string();
            if let Some(ref progress) = opts.progress {
                progress.set_phase(format!(
                    "Locking {} for exclusive backup access ({}/{})",
                    label,
                    idx + 1,
                    partition_count
                ));
            }
            // Returns `PhoenixError::VolumeLockFailed` after exhausting all
            // retries; propagating it here aborts the whole backup before
            // any bytes are streamed, and `prepared` drops to release any
            // earlier-acquired locks.
            reader.lock_volume(&label)?;
        }

        prepared.push(PreparedPartition {
            part,
            reader,
            extents,
            bytes_per_cluster,
            capture_mode,
            fs_kind,
            bitmap_hash,
            is_live,
        });
    }

    // ---- PHASE 2: capture ----
    //
    // Stream each prepared partition using its already-planned extents and
    // the (possibly locked) reader. We deliberately do not re-run
    // `plan_capture` here — for NTFS that would reissue
    // `FSCTL_GET_VOLUME_BITMAP` on a now-locked handle and fail with
    // `ERROR_NOT_READY`, falling back to "all clusters used" and writing
    // the entire partition. For non-NTFS partitions there's nothing to
    // recompute either; the planned extents in `prep` are the source of
    // truth.
    for (part_idx, prep) in prepared.iter_mut().enumerate() {
        let part: &PartitionInfo = prep.part;
        let reader = &mut prep.reader;
        let fs_kind = prep.fs_kind;
        let capture_mode = prep.capture_mode;

        // Short-circuit between partitions so a cancel during a long
        // capture step doesn't have to wait until the next chunk write to
        // honor the user's click.
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

        info!("Backing up partition {} ({})", part.index, part.name);

        let mut stream = writer.begin_partition_stream(
            part.index,
            part.type_guid,
            part.name.clone(),
            part.size_bytes,
            fs_kind,
            capture_mode,
            disk.sector_size,
            0,
            &prep.extents,
            prep.bytes_per_cluster,
        )?;

        let (used_bytes, bitmap_hash) = match (fs_kind, capture_mode) {
            (FilesystemKind::Ntfs, CaptureMode::UsedBlocks) => {
                // Hand the planned extents and pre-computed bitmap hash
                // straight to the streamer so we don't reissue the
                // FSCTL bitmap query on a locked volume.
                capture_ntfs(reader, &mut stream, &prep.extents, prep.bitmap_hash.clone())?
            }
            (FilesystemKind::Fat, CaptureMode::UsedBlocks) => {
                capture_fat(reader, &mut stream, false)?
            }
            (FilesystemKind::Exfat, CaptureMode::UsedBlocks) => capture_exfat(reader, &mut stream)?,
            _ => {
                capture_raw(reader, &mut stream)?;
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

/// Compute everything `run_backup` needs to know about a partition before
/// it starts streaming bytes, in particular the used-data extent list (for
/// NTFS, this requires reading the allocation bitmap on an UNLOCKED handle
/// — see [`PreparedPartition`] for why ordering matters). The returned
/// `Option<String>` carries the bitmap hash for NTFS partitions and is
/// `None` for FAT/exFAT/raw, where the per-FS hash (if any) is computed
/// during the streaming step inside `capture_fat` / `capture_exfat`.
fn plan_capture(
    part: &PartitionInfo,
    reader: &mut PartitionReader,
) -> Result<(Vec<Extent>, u32, CaptureMode, FilesystemKind, Option<String>)> {
    let fs = part.fs_kind;
    let mode = part.capture_mode;
    match (fs, mode) {
        (FilesystemKind::Ntfs, CaptureMode::UsedBlocks) => {
            let (extents, bitmap_hash) = ntfs_plan(reader)?;
            Ok((extents, 4096, mode, fs, bitmap_hash))
        }
        (FilesystemKind::Fat | FilesystemKind::Exfat, CaptureMode::UsedBlocks) => Ok((
            vec![Extent {
                start_sector: 0,
                sector_count: part.size_bytes / 512,
            }],
            0,
            mode,
            fs,
            None,
        )),
        _ => Ok((
            raw_extent_for_partition(part.size_bytes, 512),
            0,
            CaptureMode::Raw,
            fs,
            None,
        )),
    }
}
