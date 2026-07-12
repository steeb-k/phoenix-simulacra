use chrono::Utc;
use phoenix_core::container::{
    Extent, Header, PhnxWriter, CHUNK_SIZE, EXTENT_LBA_BYTES, FORMAT_VERSION,
};
use phoenix_core::disk::guid_to_string;
use phoenix_core::disk::{
    enumerate_disks, parse_drive_letter, refine_partition_fs, BitlockerState, CaptureMode,
    FilesystemKind, PartitionInfo,
};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;
use phoenix_core::manifest::{
    bitlocker_state_to_manifest, capture_mode_to_string, fs_kind_to_string, BackupManifest,
    DiskManifest, PartitionManifest,
};
use phoenix_core::ProgressHandle;
use tracing::{info, warn};
use uuid::Uuid;

use crate::fat::{capture_exfat, capture_fat, exfat_plan, fat_plan};
use crate::ntfs::{capture_ntfs, ntfs_plan};
use crate::raw::{capture_raw, raw_extent_for_partition};
use crate::reader::{BlockSource, PartitionReader};

pub struct BackupOptions {
    pub disk_index: u32,
    pub partition_indices: Vec<u32>,
    pub output: std::path::PathBuf,
    pub use_vss: bool,
    /// After capture, re-read the source and confirm every chunk still hashes
    /// to what was recorded — proving the backup faithfully captured the disk.
    /// Done while the VSS snapshot / volume lock is still held so the source
    /// can't have changed. Defaults to on; roughly doubles source read time.
    pub verify_after: bool,
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
    /// and re-iterated by `capture_ntfs` / `capture_fat` / `capture_raw`.
    /// For NTFS this is the bitmap-derived used-cluster set; for FAT/exFAT
    /// it is the FAT-derived used-cluster set from `fat_plan` (the
    /// previous "single placeholder extent covering the whole partition"
    /// behavior was a correctness bug — see `capture_fat`'s doc comment);
    /// for raw mode it is a single full-partition extent.
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
    /// True when the bytes this reader serves CANNOT change for the
    /// backup's duration: a VSS shadow, a volume held under
    /// `FSCTL_LOCK_VOLUME`, or a BitLocker-locked partition's static
    /// ciphertext. Drives the verify-after strategy: frozen sources get the
    /// full re-read-the-source verification; unfrozen ones (un-lettered
    /// volumes Windows refuses to lock or snapshot — e.g. the ESP on some
    /// systems — and raw physical reads like the MSR) get image-integrity
    /// verification instead, because byte-comparing a mutable source
    /// against a point-in-time image is a guaranteed spurious failure.
    frozen: bool,
}

pub fn run_backup(opts: BackupOptions) -> Result<()> {
    let op_start = std::time::Instant::now();
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
    let partition_count = selected.len();

    // Declare the full ordered step plan up front so the GUI can render
    // upcoming steps grayed out: prepare → one step per partition →
    // finalize → verify (when enabled). The per-partition `set_step` calls
    // below walk this list.
    if let Some(ref progress) = opts.progress {
        let mut steps = Vec::with_capacity(partition_count + 3);
        steps.push("Preparing volumes".to_string());
        for (idx, part) in selected.iter().enumerate() {
            steps.push(format!(
                "Backing up partition {} of {} — {}",
                idx + 1,
                partition_count,
                part.name
            ));
        }
        steps.push("Finalizing backup file".to_string());
        if opts.verify_after {
            steps.push("Verifying backup".to_string());
        }
        progress.set_steps(steps);
        progress.begin(total_bytes.max(1), "Backup");
        progress.set_step(0);
    }

    let mut writer = PhnxWriter::create_with_progress(&opts.output, header, opts.progress.clone())?;
    let mut partition_manifests = Vec::new();
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
        progress.set_step(0);
    }
    let mut prepared: Vec<PreparedPartition<'_>> = Vec::with_capacity(partition_count);
    for (idx, part) in selected.iter().enumerate() {
        if let Some(ref progress) = opts.progress {
            if progress.is_cancelled() {
                return Err(phoenix_core::error::PhoenixError::Cancelled);
            }
        }

        if part.bitlocker == BitlockerState::Locked {
            warn!(
                partition = part.index,
                name = part.name.as_str(),
                "partition is BitLocker-LOCKED: capturing raw ciphertext. The backup will \
                 restore to an encrypted volume that still requires the original BitLocker \
                 key/recovery password. Unlock the volume and re-run for a normal, \
                 unencrypted (and typically much smaller) backup."
            );
        }

        let original_volume = part.volume_path.clone();
        // A locked BitLocker volume must be read through the PHYSICAL disk
        // handle at the partition offset: fvevol rejects reads through the
        // volume device with FVE_E_LOCKED_VOLUME (0x80310000) until the
        // volume is unlocked (observed live in the T2 bitlocker systest).
        // That also rules out VSS (no mounted filesystem to snapshot) and
        // FSCTL_LOCK_VOLUME (nothing is mounted; and `is_live` below is
        // false for the PhysicalDrive path). The ciphertext can't change
        // underneath us unless someone unlocks and writes mid-backup — and
        // verify-after-backup re-reads the source, so even that tampering
        // window fails loudly instead of corrupting the image silently.
        let locked_bitlocker = part.bitlocker == BitlockerState::Locked;
        let read_path = if locked_bitlocker {
            disk.path.clone()
        } else if opts.use_vss {
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

        // Frozen-source bookkeeping: a VSS shadow is frozen by definition,
        // and a locked-BitLocker partition's ciphertext is static while it
        // stays locked. A live volume becomes frozen only if the lock below
        // succeeds.
        let mut frozen = original_volume
            .as_deref()
            .map(|orig| orig != read_path.as_str())
            .unwrap_or(false)
            || locked_bitlocker;

        if is_live {
            // Use the original volume path as the user-facing label
            // (e.g. `\\.\E:`); we don't want shadow GLOBALROOT junk in the
            // error message even though we'd never actually take this
            // branch with a shadow path.
            let label = original_volume
                .as_deref()
                .unwrap_or(read_path.as_str())
                .to_string();
            if let Some(ref progress) = opts.progress {
                // Sub-message under the "Preparing volumes" step — keep the
                // step itself fixed and surface the per-volume lock as detail.
                progress.set_detail(format!(
                    "Locking {} for exclusive backup access ({}/{})",
                    label,
                    idx + 1,
                    partition_count
                ));
            }
            match reader.lock_volume(&label) {
                Ok(()) => frozen = true,
                Err(e) => {
                    // Lettered volumes: a lock failure means files are open
                    // and the filesystem is actively mutating — abort before
                    // any bytes are streamed (`prepared` drops, releasing
                    // earlier locks). Un-lettered volumes (ESP/Recovery via
                    // their \\?\Volume{GUID} device): Windows can refuse the
                    // lock outright (observed: ERROR_ACCESS_DENIED on the
                    // ESP), and there is no user-closeable "open file" to
                    // remedy — capture unfrozen instead, and verify-after
                    // checks the image's integrity rather than re-reading
                    // the (mutable) source.
                    if parse_drive_letter(&label).is_some() {
                        return Err(e);
                    }
                    warn!(
                        volume = label.as_str(),
                        error = %e,
                        "cannot lock un-lettered volume; capturing without a frozen source \
                         (verify-after will check image integrity instead of source match)"
                    );
                }
            }
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
            frozen,
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
            // Step 0 is "Preparing volumes", so partition `part_idx` maps to
            // step `part_idx + 1` in the plan declared above.
            progress.set_step(part_idx + 1);
            progress.set_detail(String::new());
        }

        info!("Backing up partition {} ({})", part.index, part.name);

        let mut stream = writer.begin_partition_stream(
            part.index,
            part.type_guid,
            part.name.clone(),
            part.size_bytes,
            fs_kind,
            capture_mode,
            // The index entry's `sector_size` is the extent addressing unit,
            // which is a fixed 512-byte LBA for every filesystem planner
            // (`ntfs_plan`, `fat_plan`, `raw_extent_for_partition` all emit
            // 512-byte-sector extents). It is NOT the disk's physical sector
            // size — restore multiplies `extent.start_sector` by this value,
            // so on a 4Kn disk passing 4096 here would inflate every offset
            // 8x. The real physical sector size is recorded in the manifest's
            // disk block and used only for I/O alignment.
            EXTENT_LBA_BYTES,
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
                // Same shape as NTFS now: planned extents from `fat_plan`
                // are the manifest's extent table, and the streamer just
                // walks them. Eliminates the previous extent-table-vs-
                // chunk-index mismatch that broke FAT/exFAT restores.
                capture_fat(reader, &mut stream, &prep.extents, prep.bitmap_hash.clone())?
            }
            (FilesystemKind::Exfat, CaptureMode::UsedBlocks) => {
                capture_exfat(reader, &mut stream, &prep.extents, prep.bitmap_hash.clone())?
            }
            _ => {
                capture_raw(reader, &mut stream)?;
                // A partition's used bytes can't exceed its own size. A volume
                // handle can report a length that disagrees with the GPT
                // partition size (observed on exFAT); cap so downstream
                // used_bytes-vs-target checks stay sane.
                (reader.length().min(part.size_bytes), None)
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
            bitlocker: bitlocker_state_to_manifest(part.bitlocker),
            unique_guid: if part.unique_guid.iter().all(|&b| b == 0) {
                None
            } else {
                Some(guid_to_string(&part.unique_guid))
            },
            gpt_attributes: if disk.is_gpt {
                Some(part.gpt_attributes)
            } else {
                None
            },
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
        // Last step in the plan: "Finalizing backup file".
        progress.set_step(partition_count + 1);
        progress.set_detail(String::new());
    }
    writer.finalize(&manifest)?;

    // Timing lines so multi-hour runs leave a durable record of what each
    // phase cost — the numbers that justify (or indict) throughput work.
    let capture_secs = op_start.elapsed().as_secs_f64();
    let captured_bytes: u64 = manifest.partitions.iter().map(|p| p.used_bytes).sum();
    info!(
        "capture phase done (incl. snapshot setup): {} in {}",
        phoenix_core::progress::format_rate(captured_bytes, capture_secs),
        phoenix_core::progress::format_elapsed(capture_secs),
    );

    // ---- PHASE 3: verify (default on), strategy matched to consistency ----
    //
    // FROZEN sources (VSS shadow, volume held under FSCTL_LOCK_VOLUME,
    // locked-BitLocker ciphertext): re-read the source and confirm every
    // recorded chunk still hashes to what capture wrote — the strongest
    // guarantee (image == the frozen source). The snapshot / lock from
    // phase 1 is still held (`vss` and `prepared` live until return).
    //
    // UNFROZEN sources (un-lettered volumes Windows refused to lock or
    // snapshot, raw physical reads like the MSR): the source may legally
    // have changed since capture, so re-reading it produces spurious
    // "mismatch" failures (observed live: the ESP during a 4-hour boot-disk
    // capture). Verify the IMAGE instead — decompress and BLAKE3-check every
    // chunk of those partitions against the manifest, proving the backup
    // file faithfully holds what capture read.
    if opts.verify_after {
        let verify_start = std::time::Instant::now();
        if let Some(ref progress) = opts.progress {
            // Last step in the plan: "Verifying backup". Restart the byte
            // counter over the bytes we're about to re-read so the progress
            // bar tracks the verify pass instead of sitting pinned at 100%.
            progress.set_step(partition_count + 2);
            progress.begin(captured_bytes.max(1), "Verifying backup");
            progress.set_detail("Verifying backup against source…".to_string());
        }
        let mut image_verify: Vec<(u32, u64)> = Vec::new();
        for (prep, pm) in prepared.iter_mut().zip(manifest.partitions.iter()) {
            if let Some(ref progress) = opts.progress {
                if progress.is_cancelled() {
                    return Err(phoenix_core::error::PhoenixError::Cancelled);
                }
            }
            if !prep.frozen {
                image_verify.push((prep.part.index, pm.used_bytes));
                continue;
            }
            verify_partition_against_source(
                &mut prep.reader,
                &prep.extents,
                prep.capture_mode,
                &pm.chunks,
                opts.progress.as_ref(),
            )
            .map_err(|e| match e {
                PhoenixError::Cancelled => PhoenixError::Cancelled,
                e => phoenix_core::error::PhoenixError::Other(format!(
                    "verify-after-backup failed for partition {}: {e}",
                    prep.part.index
                )),
            })?;
        }
        if !image_verify.is_empty() {
            warn!(
                partitions = ?image_verify,
                "these partitions were captured without a frozen source (no snapshot/lock \
                 available); verifying image integrity (chunk BLAKE3) instead of source match"
            );
            if let Some(ref progress) = opts.progress {
                progress
                    .set_detail("Verifying image integrity for unfrozen partitions…".to_string());
            }
            let mut image_reader = phoenix_core::container::PhnxReader::open(&opts.output)?;
            for (idx, used_bytes) in image_verify {
                if let Some(ref progress) = opts.progress {
                    if progress.is_cancelled() {
                        return Err(phoenix_core::error::PhoenixError::Cancelled);
                    }
                }
                image_reader.verify_partition(idx, false).map_err(|e| {
                    phoenix_core::error::PhoenixError::Other(format!(
                        "image-integrity verify failed for partition {idx}: {e}"
                    ))
                })?;
                if let Some(ref progress) = opts.progress {
                    // Coarse (per-partition) credit for the image-verified
                    // bytes; the source-verify path bumps per chunk.
                    progress.bump(used_bytes);
                }
            }
        }
        let verify_secs = verify_start.elapsed().as_secs_f64();
        info!(
            "verify-after-backup: all partitions verified — {} in {}",
            phoenix_core::progress::format_rate(captured_bytes, verify_secs),
            phoenix_core::progress::format_elapsed(verify_secs),
        );
    }

    if let Some(ref progress) = opts.progress {
        progress.end();
    }
    info!(
        "Backup written to {} — total {}",
        opts.output.display(),
        phoenix_core::progress::format_elapsed(op_start.elapsed().as_secs_f64()),
    );
    Ok(())
}

/// Re-read a partition's source and confirm every recorded chunk still hashes
/// to its captured value. Chunk byte offsets are reconstructed by accumulating
/// each chunk's `uncompressed_len` within its extent — NOT by assuming
/// `CHUNK_SIZE` alignment, because capture advances by the actual bytes read
/// and raw volume reads can be short (which shifts later chunks). Reads fill the
/// exact recorded length so a short device read doesn't cause a false mismatch.
fn verify_partition_against_source(
    reader: &mut impl BlockSource,
    extents: &[Extent],
    capture_mode: CaptureMode,
    expected: &[phoenix_core::manifest::ChunkRecord],
    progress: Option<&ProgressHandle>,
) -> Result<()> {
    // Count of proven-transient torn reads (each one WARN-logged and
    // re-read-confirmed against the recorded hash — direct proof the image
    // matches the source for that chunk). Not capped: on slow media through
    // a VSS shadow these happen once per block the live volume modifies
    // mid-backup (the read races volsnap's just-completed copy-on-write), so
    // the count scales with elapsed time and live write churn, not with
    // device health. Genuine problems still fail via the stable-divergence
    // and unstable-re-read verdicts below.
    let mut torn_reads = 0u64;

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut cur_extent: Option<u32> = None;
    let mut offset = 0u64;
    for rec in expected {
        if let Some(p) = progress {
            if p.is_cancelled() {
                return Err(PhoenixError::Cancelled);
            }
        }
        let len = rec.uncompressed_len as usize;
        if len == 0 || len > CHUNK_SIZE {
            return Err(PhoenixError::Other(format!(
                "chunk (extent {}, index {}) has invalid length {len}",
                rec.extent_index, rec.chunk_index
            )));
        }
        // Reset to the extent's base whenever we enter a new extent; within an
        // extent, chunks are contiguous, so we accumulate their lengths.
        if cur_extent != Some(rec.extent_index) {
            cur_extent = Some(rec.extent_index);
            offset = if capture_mode == CaptureMode::Raw {
                0
            } else {
                let ext = extents.get(rec.extent_index as usize).ok_or_else(|| {
                    PhoenixError::Other(format!(
                        "chunk references extent {} of {}",
                        rec.extent_index,
                        extents.len()
                    ))
                })?;
                ext.start_sector * EXTENT_LBA_BYTES as u64
            };
        }

        // Read exactly `len` bytes (looping over short reads).
        let mut got = 0usize;
        while got < len {
            let n = reader.read_at(offset + got as u64, &mut buf[got..len])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        if got != len {
            return Err(PhoenixError::Other(format!(
                "source read {got} bytes, expected {len} at offset {offset} (extent {}, chunk {})",
                rec.extent_index, rec.chunk_index
            )));
        }
        let got_hash = hash::hash_hex(&buf[..len]);
        if got_hash != rec.blake3 {
            // Distinguish a transient torn read from real divergence before
            // failing. Observed live on VSS shadow-device reads: one verify
            // read returns wrong bytes, and immediate re-reads return the
            // recorded content — the image is faithful, the *verify read*
            // hiccuped (volsnap racing the live volume's copy-on-write). In
            // that proven-transient case, retry instead of failing the whole
            // backup — loudly, and only up to a small budget, so genuine
            // instability still fails.
            let probe = |reader: &mut dyn FnMut(u64, &mut [u8]) -> Result<usize>| -> String {
                let mut pbuf = vec![0u8; len];
                let mut pgot = 0usize;
                while pgot < len {
                    match reader(offset + pgot as u64, &mut pbuf[pgot..len]) {
                        Ok(0) => return "short-read".into(),
                        Ok(n) => pgot += n,
                        Err(e) => return format!("read-error({e})"),
                    }
                }
                hash::hash_hex(&pbuf[..len])
            };
            let mut f = |off: u64, b: &mut [u8]| reader.read_at(off, b);
            let reread1 = probe(&mut f);
            let reread2 = probe(&mut f);

            if reread1 == rec.blake3 || reread2 == rec.blake3 {
                // Transient: a re-read reproduces exactly what capture
                // recorded, so the source is intact and the image matches it.
                torn_reads += 1;
                tracing::warn!(
                    extent = rec.extent_index,
                    chunk = rec.chunk_index,
                    offset,
                    torn_reads,
                    "verify-after-backup: transient torn read from the source device \
                     (re-read matches the recorded hash); continuing"
                );
                offset += len as u64;
                if let Some(p) = progress {
                    p.bump(len as u64);
                }
                continue;
            }

            let verdict = if reread1 == got_hash && reread2 == got_hash {
                "re-reads are stable but differ from capture — source content changed after capture"
            } else {
                "re-reads are UNSTABLE — source device is returning inconsistent data"
            };
            return Err(PhoenixError::Other(format!(
                "extent {}, chunk {} at offset {offset} (len {len}) does not match the source — \
                 the backup does not faithfully represent the disk. recorded={}, verify-read={}, \
                 reread1={}, reread2={}. Diagnosis: {verdict}",
                rec.extent_index, rec.chunk_index, rec.blake3, got_hash, reread1, reread2
            )));
        }
        offset += len as u64;
        if let Some(p) = progress {
            p.bump(len as u64);
        }
    }
    if torn_reads > 0 {
        info!(
            torn_reads,
            "verify-after-backup: partition verified; {torn_reads} transient torn read(s) \
             were re-read-confirmed against the recorded hashes"
        );
    }
    Ok(())
}

/// Compute everything `run_backup` needs to know about a partition before
/// it starts streaming bytes, in particular the used-data extent list (for
/// NTFS, this requires reading the allocation bitmap on an UNLOCKED handle
/// — see [`PreparedPartition`] for why ordering matters). The returned
/// `Option<String>` carries the bitmap hash for NTFS partitions and is
/// `None` for FAT/exFAT/raw, where the per-FS hash (if any) is computed
/// during the streaming step inside `capture_fat` / `capture_exfat`.
#[allow(clippy::type_complexity)]
pub fn plan_capture(
    part: &PartitionInfo,
    reader: &mut PartitionReader,
) -> Result<(
    Vec<Extent>,
    u32,
    CaptureMode,
    FilesystemKind,
    Option<String>,
)> {
    let fs = part.fs_kind;
    let mode = part.capture_mode;
    match (fs, mode) {
        (FilesystemKind::Ntfs, CaptureMode::UsedBlocks) => {
            let (extents, bitmap_hash) = ntfs_plan(reader)?;
            Ok((extents, 4096, mode, fs, bitmap_hash))
        }
        (FilesystemKind::Fat, CaptureMode::UsedBlocks) => {
            // FAT12/16/32: walk the FAT to derive extents up front so the
            // manifest's extent table matches the chunks the streamer
            // produces (see `capture_fat` for the rationale).
            let (extents, bitmap_hash, bytes_per_cluster) = fat_plan(reader, false)?;
            Ok((extents, bytes_per_cluster, mode, fs, bitmap_hash))
        }
        (FilesystemKind::Exfat, CaptureMode::UsedBlocks) => {
            // exFAT records contiguous files with the "NoFatChain" flag, so
            // their clusters are NOT in the FAT — they're tracked only in the
            // allocation bitmap. `exfat_plan` reads that bitmap (authoritative
            // for all allocation) to compute used-cluster extents. On ANY
            // parse failure we fall back to a full raw capture rather than
            // risk silently dropping data from a volume we couldn't fully
            // understand.
            match exfat_plan(reader) {
                Ok((extents, bitmap_hash, bytes_per_cluster)) => Ok((
                    extents,
                    bytes_per_cluster,
                    CaptureMode::UsedBlocks,
                    fs,
                    bitmap_hash,
                )),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "exFAT allocation-bitmap plan failed; falling back to full raw capture"
                    );
                    Ok((
                        raw_extent_for_partition(part.size_bytes, 512),
                        0,
                        CaptureMode::Raw,
                        fs,
                        None,
                    ))
                }
            }
        }
        _ => Ok((
            raw_extent_for_partition(part.size_bytes, 512),
            0,
            CaptureMode::Raw,
            fs,
            None,
        )),
    }
}
