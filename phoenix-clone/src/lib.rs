//! Direct disk-to-disk cloning for Phoenix Simulacra.
//!
//! Unlike backup+restore, cloning streams each source partition's used blocks
//! straight to the target disk with no intermediate `.phnx` file and no
//! compression. It reuses the same building blocks the backup/restore engines
//! are built from — extent planning ([`phoenix_capture::plan_capture`]), the
//! raw [`PartitionWriter`], the NTFS shrink relocation map + metadata rewriter,
//! the FAT/exFAT boot patchers, and the GPT/MBR partition-table writers — so a
//! clone gets the same resize support (grow + NTFS shrink relocation) and the
//! same live-volume consistency (VSS) as a backup.

use tracing::{info, warn};

use phoenix_core::container::{Extent, CHUNK_SIZE, EXTENT_LBA_BYTES};
use phoenix_core::disk::{
    enumerate_disks, refine_partition_fs, CaptureMode, DiskInfo, FilesystemKind, PartitionInfo,
};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::relocation::RelocationMap;
use phoenix_core::sector::ConversionReport;
use phoenix_core::ProgressHandle;

use phoenix_capture::{
    finalize_fat_partition, finalize_ntfs_partition, plan_capture, PartitionReader, PartitionWriter,
};
use phoenix_restore::grow::extend_ntfs_volume;
use phoenix_restore::partition_table::{
    bring_disk_online, flush_disk, init_target_disk_as_gpt, init_target_disk_as_mbr,
    notify_disk_updated, update_partition_layout_existing_bytes, write_mbr_partition_layout,
    write_partition_layout, GptEntry, MbrEntry,
};
use phoenix_restore::relocation::build_relocation_map;

pub mod plan;
pub use plan::{CloneEntry, ClonePlan, CloneTableMode};

/// Whether to read each written region back off the target and compare it to
/// the source before moving on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneVerify {
    /// Trust the write (fast).
    None,
    /// Re-read every written block from the target and byte-compare against the
    /// source. Roughly doubles target I/O but proves the copy landed intact.
    ReadBack,
}

pub struct CloneOptions {
    pub source_disk_index: u32,
    pub target_disk_index: u32,
    pub plan: ClonePlan,
    pub verify: CloneVerify,
    /// Opt-in to a 4Kn → 512e cross-sector-size conversion (source disk uses
    /// 4096-byte logical sectors, target uses 512-byte). The filesystem boot
    /// sectors are rewritten so the cloned volumes mount instead of coming up
    /// RAW — a converted copy, not a byte-identical clone. Equivalent to the CLI
    /// `--convert-sector-size` flag / the GUI hazard-dialog checkbox. Ignored
    /// when the sector sizes already match.
    pub convert_sector_size: bool,
    pub progress: Option<ProgressHandle>,
    // NOTE: there is deliberately no `use_vss`. How a source is frozen is not a
    // user choice — the engine takes the strongest freeze each volume allows (see
    // `prepare_partition`), exactly as the backup path has since the "Use VSS"
    // switch was dropped. The old flag made VSS an all-or-nothing decision taken
    // *before* the lock was even tried, which is how a boot-disk clone ended up
    // hard-failing on the ESP: VSS cannot snapshot FAT32, `snapshot_volume` fell
    // back to the live path, and the lock that followed was refused.
}

#[derive(Debug, Clone, Default)]
pub struct CloneSummary {
    pub partitions_cloned: u32,
    pub partitions_resized: u32,
    pub bytes_copied: u64,
    /// Number of partitions whose boot sector was rewritten 4Kn → 512e.
    pub partitions_converted: u32,
    /// `Some(true/false)` when a 4Kn → 512e conversion ran: whether the ESP was
    /// among the converted set (i.e. the cloned disk should be bootable).
    /// `None` when no conversion occurred.
    pub conversion_bootable: Option<bool>,
    /// Human names of partitions converted 4Kn → 512e (Alert G summary).
    pub conversion_converted_names: Vec<String>,
    /// Human names of partitions left unconverted because v1 does not convert
    /// their filesystem (exFAT / BitLocker ciphertext) — Alert E.
    pub conversion_unconverted_names: Vec<String>,
}

/// Clone `source_disk_index` onto `target_disk_index` per the plan.
pub fn run_clone(opts: CloneOptions) -> Result<CloneSummary> {
    let op_start = std::time::Instant::now();
    let mut disks = enumerate_disks()?;
    for d in &mut disks {
        for p in &mut d.partitions {
            refine_partition_fs(p);
        }
    }

    if opts.source_disk_index == opts.target_disk_index {
        return Err(PhoenixError::Disk(
            "source and target are the same disk".into(),
        ));
    }
    let source = disks
        .iter()
        .find(|d| d.index == opts.source_disk_index)
        .ok_or_else(|| PhoenixError::Disk("source disk not found".into()))?
        .clone();
    let target = disks
        .iter()
        .find(|d| d.index == opts.target_disk_index)
        .ok_or_else(|| PhoenixError::Disk("target disk not found".into()))?
        .clone();

    // Guard against cloning a disk onto its own duplicate (USB enclosure that
    // surfaces the same physical media twice, etc.).
    if source.disk_signature != 0 && source.disk_signature == target.disk_signature {
        warn!(
            "source and target report the same disk signature ({:#x}); if these are the same \
             physical disk the clone will corrupt it",
            source.disk_signature
        );
    }

    // Validate the plan against both disks before touching anything.
    opts.plan.validate(&source, &target)?;

    let summary = clone_inner(&source, &target, &opts)?;
    let total_secs = op_start.elapsed().as_secs_f64();
    info!(
        partitions_cloned = summary.partitions_cloned,
        partitions_resized = summary.partitions_resized,
        elapsed = %phoenix_core::progress::format_elapsed(total_secs),
        // `bytes_copied` counts read-back re-reads too, matching the meter.
        throughput = %phoenix_core::progress::format_rate(summary.bytes_copied, total_secs),
        "Clone complete"
    );
    Ok(summary)
}

fn clone_inner(source: &DiskInfo, target: &DiskInfo, opts: &CloneOptions) -> Result<CloneSummary> {
    let mut summary = CloneSummary::default();

    // Cross-sector-size pre-flight, before anything is frozen or the target is
    // touched: gate/opt-in the 4Kn → 512e conversion, refuse an unsupported
    // mismatch or a converted-partition shrink, and name what will convert.
    let conversion = plan_sector_conversion(
        source,
        &opts.plan,
        target.sector_size,
        opts.convert_sector_size,
    )?;
    if let Some(report) = &conversion {
        log_conversion_banner(source.sector_size, target.sector_size, report);
    }
    let converting = conversion.is_some();

    // Total planned bytes for the progress meter (doubled when reading back).
    let per_pass: u64 = opts
        .plan
        .entries
        .iter()
        .filter_map(|e| {
            source
                .partitions
                .iter()
                .find(|p| p.index == e.source_partition_index)
        })
        .map(planned_bytes)
        .sum();
    let total = if opts.verify == CloneVerify::ReadBack {
        per_pass * 2
    } else {
        per_pass
    };

    if let Some(ref p) = opts.progress {
        let mut steps = vec!["Preparing disks".to_string()];
        for (i, e) in opts.plan.entries.iter().enumerate() {
            let name = source
                .partitions
                .iter()
                .find(|p| p.index == e.source_partition_index)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "partition".into());
            steps.push(format!(
                "Cloning partition {} of {} — {}",
                i + 1,
                opts.plan.entries.len(),
                name
            ));
        }
        steps.push("Writing partition table".to_string());
        p.set_steps(steps);
        p.begin(total.max(1), "Clone");
        p.set_step(0);
    }

    // --- PRE-FLIGHT: freeze and plan EVERY source partition, before the target
    // --- is touched at all.
    //
    // This runs first for a reason that cost a boot-disk clone: the target init
    // below WIPES the target's partition table, and the streaming loop used to do
    // its freezing inline, partition by partition. So a source volume that could
    // not be frozen — the ESP is the one that actually bites — failed only once
    // the engine reached it, by which point the target had already been
    // re-initialized and earlier partitions written over it. The user's disk was
    // destroyed to discover something knowable up front.
    //
    // Backup has always done it this way (`run_backup`'s `prepared` phase). Clone
    // now matches: every source partition is opened, frozen, and planned here, and
    // any failure returns before a single byte of the target changes. The
    // `prepared` Vec dropping on the error path releases every lock and shadow we
    // took along the way, so there is no cleanup to forget.
    //
    // The VssSession must outlive the streaming — dropping it deletes the shadows
    // we are reading through — so it lives here, not inside the loop.
    let mut vss = phoenix_vss::VssSession::new();
    let mut prepared: Vec<PreparedPartition> = Vec::with_capacity(opts.plan.entries.len());
    for entry in &opts.plan.entries {
        if let Some(ref p) = opts.progress {
            if p.is_cancelled() {
                return Err(PhoenixError::Cancelled);
            }
        }
        let part = source
            .partitions
            .iter()
            .find(|p| p.index == entry.source_partition_index)
            .ok_or_else(|| PhoenixError::Disk("source partition vanished".into()))?;
        prepared.push(prepare_partition(source, part, entry, &mut vss, opts)?);
    }

    // --- Initialize the target disk (GPT/MBR skeleton, no entries yet) ---
    // Same rationale as restore: defer the partition entries until after the
    // data is written so mountmgr can't lazy-mount empty volumes and bounce
    // our raw writes with ERROR_ACCESS_DENIED.
    //
    // UpdateExisting (partial clone) skips the init entirely — the target's
    // table stays live while data lands, so instead the covering volumes are
    // locked + dismounted below, exactly like a partial restore.
    let table_gpt = match &opts.plan.table_mode {
        CloneTableMode::ReinitMatchSource => source.is_gpt,
        CloneTableMode::ReinitAs { gpt } => *gpt,
        CloneTableMode::UpdateExisting { .. } => target.is_gpt,
    };
    let reinit = !matches!(opts.plan.table_mode, CloneTableMode::UpdateExisting { .. });
    let gpt_state = if reinit && table_gpt {
        Some(init_target_disk_as_gpt(
            &target.path,
            target.size_bytes,
            // A style switch from an MBR source has no source GUID to carry
            // over; a fresh one is generated.
            source.disk_guid.filter(|_| source.is_gpt),
        )?)
    } else if reinit {
        init_target_disk_as_mbr(&target.path, source.disk_signature as u32)?;
        None
    } else {
        None
    };

    // Partial clone writes into slots of a live partition table: any target
    // partition that is NOT preserved is either overwritten by a planned
    // entry or dropped from the table afterwards, and either way a mounted
    // volume on it must be locked + dismounted first or the raw writes (and
    // the in-place table rewrite) bounce with ERROR_ACCESS_DENIED. The
    // guards drop after the table is re-stamped so the grown-NTFS pass can
    // remount and extend.
    let mut locked_target_volumes: Vec<phoenix_core::disk::LockedVolume> = Vec::new();
    if let CloneTableMode::UpdateExisting {
        preserved_target_indices,
    } = &opts.plan.table_mode
    {
        for p in &target.partitions {
            if preserved_target_indices.contains(&p.index) {
                continue;
            }
            if let Some(vol) = &p.volume_path {
                let guard = phoenix_core::disk::LockedVolume::acquire(vol).map_err(|e| {
                    PhoenixError::Disk(format!(
                        "could not clear the volume {vol} occupying target partition {} before \
                         cloning over it (close any open files/handles on it and retry): {e}",
                        p.index
                    ))
                })?;
                info!(
                    volume = vol.as_str(),
                    partition = p.index,
                    "dismounted live target-slot volume for partial clone"
                );
                locked_target_volumes.push(guard);
            }
        }
    }

    let mut bytes_done = 0u64;
    let mut grown_ntfs: Vec<(u64, u64)> = Vec::new(); // (target_offset, target_size)

    // Every source is already frozen and planned; from here on we only write.
    for (i, mut prep) in prepared.into_iter().enumerate() {
        if let Some(ref p) = opts.progress {
            if p.is_cancelled() {
                return Err(PhoenixError::Cancelled);
            }
            p.set_step(i + 1);
        }
        let entry = prep.entry.clone();
        let part = source
            .partitions
            .iter()
            .find(|p| p.index == entry.source_partition_index)
            .ok_or_else(|| PhoenixError::Disk("source partition vanished".into()))?;

        let converted;
        (bytes_done, converted) = stream_one_partition(
            target, part, &entry, &mut prep, opts, bytes_done, converting,
        )?;
        if converted {
            summary.partitions_converted += 1;
        }

        summary.partitions_cloned += 1;
        if entry.target_size_bytes != part.size_bytes {
            summary.partitions_resized += 1;
        }
        // A grown NTFS partition needs FSCTL_EXTEND_VOLUME after the table is
        // in place and the volume mounts.
        if part.fs_kind == FilesystemKind::Ntfs && entry.target_size_bytes > part.size_bytes {
            grown_ntfs.push((entry.target_offset_bytes, entry.target_size_bytes));
        }
    }
    summary.bytes_copied = per_pass;

    if let Some(report) = &conversion {
        summary.conversion_bootable = Some(report.bootable);
        summary.conversion_converted_names = report.converted_names.clone();
        summary.conversion_unconverted_names = report.unconverted_names.clone();
    }

    // --- Plant the partition table on top of the freshly-written data ---
    flush_disk(&target.path);
    if let Some(ref p) = opts.progress {
        p.set_step(opts.plan.entries.len() + 1);
    }
    if let CloneTableMode::UpdateExisting {
        preserved_target_indices,
    } = &opts.plan.table_mode
    {
        // In-place rewrite of the existing table: cloned entries carry the
        // source partition's identity, preserved entries keep the live
        // target partition's, and everything else falls off the table.
        if target.is_gpt {
            let mut entries = build_gpt_entries(source, &opts.plan);
            entries.extend(
                target
                    .partitions
                    .iter()
                    .filter(|p| preserved_target_indices.contains(&p.index))
                    .map(|p| GptEntry {
                        offset_bytes: p.offset_bytes,
                        size_bytes: p.size_bytes,
                        type_guid: p.type_guid,
                        unique_guid: p.unique_guid,
                        attributes: p.gpt_attributes,
                        name: p.name.clone(),
                    }),
            );
            entries.sort_by_key(|e| e.offset_bytes);
            let disk_guid = target.disk_guid.ok_or_else(|| {
                PhoenixError::Disk(
                    "target disk GUID unavailable; cannot update its GPT in place".into(),
                )
            })?;
            update_partition_layout_existing_bytes(
                &target.path,
                &entries,
                disk_guid,
                target.size_bytes,
            )?;
        } else {
            // MBR: the IOCTL is total-rewrite but leaves the disk signature
            // alone, so this too is a pure entry rewrite.
            let mut entries = build_mbr_entries(source, &opts.plan);
            entries.extend(
                target
                    .partitions
                    .iter()
                    .filter(|p| preserved_target_indices.contains(&p.index))
                    .map(|p| MbrEntry {
                        offset_bytes: p.offset_bytes,
                        size_bytes: p.size_bytes,
                        partition_type: mbr_type_for(p.fs_kind),
                        bootable: false,
                    }),
            );
            entries.sort_by_key(|e| e.offset_bytes);
            write_mbr_partition_layout(&target.path, &entries)?;
        }
    } else if let Some(state) = gpt_state {
        let entries = build_gpt_entries(source, &opts.plan);
        write_partition_layout(&target.path, &entries, &state)?;
    } else {
        let entries = build_mbr_entries(source, &opts.plan);
        write_mbr_partition_layout(&target.path, &entries)?;
    }
    notify_disk_updated(&target.path);
    bring_disk_online(&target.path);

    // Layout is stamped: release the partial-clone volume locks so Windows
    // remounts the slots fresh (re-reading the boot sectors just written)
    // and the grown-NTFS pass below can extend mounted volumes.
    drop(locked_target_volumes);

    // --- Grow NTFS volumes that were cloned into a larger partition ---
    for (offset, size) in grown_ntfs {
        if let Err(e) = extend_ntfs_volume(target.index, offset, size) {
            warn!(offset, size, error = %e, "extend_ntfs_volume after clone failed (non-fatal)");
        }
    }

    if let Some(ref p) = opts.progress {
        p.end();
    }
    Ok(summary)
}

/// A source partition that has been resolved to a read path, opened, frozen (or
/// knowingly not), and planned — with **nothing written to the target yet**.
///
/// The clone's equivalent of `run_backup`'s `PreparedPartition`, and it exists for
/// the same reason: so a failure on partition N cannot strand an operation that has
/// already destroyed the target.
struct PreparedPartition {
    reader: PartitionReader,
    entry: CloneEntry,
    extents: Vec<Extent>,
    bytes_per_cluster: u32,
    capture_mode: CaptureMode,
    fs_kind: FilesystemKind,
}

/// Freeze and plan one source partition. Writes nothing.
///
/// The freeze strategy is **not a user choice** — the engine takes the strongest
/// one each volume allows, in the same order the backup path uses:
///
///   1. **Exclusive `FSCTL_LOCK_VOLUME`.** Cheapest and strongest: no shadow
///      storage, no VSS writers, and Windows flushes dirty cache pages before
///      granting it.
///   2. **A VSS shadow**, when the lock is refused because files are open — the
///      normal case for the running Windows volume.
///   3. **Neither: read unfrozen**, tolerated *only* for un-lettered volumes
///      (ESP/Recovery). A **lettered** volume that can be frozen neither way
///      aborts the clone, because a smeared read would silently corrupt it.
///
/// This ordering is the fix for a boot-disk clone failing on the ESP. The old code
/// decided VSS-or-not *up front* from a `use_vss` flag, and for the ESP that was
/// doubly wrong: VSS **cannot snapshot FAT32**, so `snapshot_volume` silently
/// handed back the live path (it never errors — see its docs), and the code then
/// hard-failed on a lock that Windows was never going to grant for a volume it
/// keeps mounted itself. The user saw "can't get exclusive access to the EFI
/// partition" and no sign of VSS — because VSS *had* been tried, and had quietly
/// declined.
fn prepare_partition(
    source: &DiskInfo,
    part: &PartitionInfo,
    entry: &CloneEntry,
    vss: &mut phoenix_vss::VssSession,
    opts: &CloneOptions,
) -> Result<PreparedPartition> {
    // A locked BitLocker volume must be read through the PHYSICAL disk handle:
    // fvevol rejects volume-device reads with FVE_E_LOCKED_VOLUME (0x80310000)
    // until unlock, and there's no filesystem for VSS to snapshot. There is also
    // nothing to freeze — ciphertext at rest doesn't move.
    let locked_bitlocker = part.bitlocker == phoenix_core::disk::BitlockerState::Locked;
    let volume = match &part.volume_path {
        Some(vol) if !locked_bitlocker => Some(vol.clone()),
        // No mounted volume (MSR, unformatted, BitLocker-locked): read the
        // partition's bytes straight off `\\.\PhysicalDriveN`. Nothing to lock and
        // nothing to snapshot.
        _ => None,
    };

    let mut reader = match &volume {
        Some(vol) => PartitionReader::open_volume(vol, part.sector_size)?,
        None => PartitionReader::open_disk_partition(
            &source.path,
            part.offset_bytes,
            part.size_bytes,
            part.sector_size,
        )?,
    };

    // Plan BEFORE locking: `FSCTL_GET_VOLUME_BITMAP` fails with ERROR_NOT_READY on
    // a locked NTFS volume on some builds (USB-attached NTFS reproduces reliably).
    // Same ordering constraint as backup.
    let (mut extents, mut bytes_per_cluster, mut capture_mode, mut fs_kind, mut _bitmap_hash) =
        plan_capture(part, &mut reader)?;

    if let Some(vol) = &volume {
        if let Some(ref p) = opts.progress {
            p.set_detail(format!("Locking {vol} for exclusive read access"));
        }
        match reader.lock_volume(vol) {
            Ok(()) => {}
            Err(lock_err) => {
                // Busy — the running Windows volume always is. Escalate to a shadow,
                // which freezes it without evicting anyone.
                if let Some(ref p) = opts.progress {
                    p.set_detail(format!("{vol} is in use — taking a VSS snapshot instead"));
                }
                // `snapshot_volume` never errors on Windows: on any failure it hands
                // back the live path it was given, so an unchanged path means "no
                // shadow for this volume" (FAT/exFAT — which is exactly the ESP —
                // removable media, or no shadow storage).
                let shadow = vss.snapshot_volume(vol)?;
                if shadow != *vol {
                    info!(
                        volume = vol.as_str(),
                        error = %lock_err,
                        "volume is in use; reading a VSS shadow instead of the live volume"
                    );
                    // Re-plan against the shadow: its bitmap is the frozen one we are
                    // about to stream, and it can differ from the live one we read a
                    // moment ago. Assigning `reader` drops the live handle.
                    reader = PartitionReader::open_volume(&shadow, part.sector_size)?;
                    (
                        extents,
                        bytes_per_cluster,
                        capture_mode,
                        fs_kind,
                        _bitmap_hash,
                    ) = plan_capture(part, &mut reader)?;
                } else if phoenix_core::disk::parse_drive_letter(vol).is_some()
                    && !phoenix_core::disk::is_efi_system_partition(&part.type_guid)
                {
                    // A lettered volume we could neither lock nor snapshot: files are
                    // open and the filesystem is actively mutating. Abort now, before
                    // the target is touched, rather than clone a torn filesystem the
                    // user would trust.
                    //
                    // The ESP is exempt even when it carries a drive letter (some
                    // OEM/Windows configs assign one): it is still the static boot
                    // partition Windows keeps mounted and refuses to lock, so it takes
                    // the read-unfrozen path below rather than aborting the clone.
                    return Err(lock_err);
                } else {
                    // Un-lettered volumes (ESP/Recovery, reached via their
                    // \\?\Volume{GUID} device) and the ESP even when lettered. Windows
                    // keeps the ESP mounted itself and refuses the lock (observed:
                    // ERROR_ACCESS_DENIED), and VSS will not snapshot FAT32 — and there
                    // is no open file the user could close to change either answer.
                    // These partitions are small, static, and not being written during
                    // a clone, so read them unfrozen rather than refuse to clone a boot
                    // disk at all.
                    warn!(
                        volume = vol.as_str(),
                        error = %lock_err,
                        "cannot lock or snapshot un-lettered volume (ESP/Recovery); cloning it \
                         without a frozen source"
                    );
                }
            }
        }
    }

    Ok(PreparedPartition {
        reader,
        entry: entry.clone(),
        extents,
        bytes_per_cluster,
        capture_mode,
        fs_kind,
    })
}

/// Stream one **already-prepared** partition to the target, applying shrink
/// relocation and the FS-specific finalization. Returns the running byte total.
/// Returns `(bytes_done, converted)` where `converted` is true when this
/// partition's boot sector was rewritten 4Kn → 512e.
#[allow(clippy::too_many_arguments)]
fn stream_one_partition(
    target: &DiskInfo,
    part: &PartitionInfo,
    entry: &CloneEntry,
    prep: &mut PreparedPartition,
    opts: &CloneOptions,
    mut bytes_done: u64,
    convert: bool,
) -> Result<(u64, bool)> {
    // Take the extents (the reader stays borrowed in place — `stream_extents` needs
    // it by &mut, and the rest are Copy).
    let extents = std::mem::take(&mut prep.extents);
    let bytes_per_cluster = prep.bytes_per_cluster;
    let capture_mode = prep.capture_mode;
    let fs_kind = prep.fs_kind;

    // --- Shrink relocation (NTFS only) ---
    let relocation: Option<RelocationMap> =
        if fs_kind == FilesystemKind::Ntfs && entry.target_size_bytes < part.size_bytes {
            build_relocation_map(
                &extents,
                EXTENT_LBA_BYTES as u64,
                bytes_per_cluster as u64,
                entry.target_size_bytes,
            )?
        } else {
            None
        };

    // --- Open the target writer and stream ---
    let mut writer =
        PartitionWriter::open_disk(&target.path, entry.target_offset_bytes, target.sector_size)?;

    let effective_extents = if capture_mode == CaptureMode::Raw {
        // Raw capture streams the whole partition as one extent.
        vec![Extent {
            start_sector: 0,
            sector_count: part.size_bytes.div_ceil(EXTENT_LBA_BYTES as u64),
        }]
    } else {
        extents
    };

    bytes_done = stream_extents(
        &mut prep.reader,
        &mut writer,
        &effective_extents,
        relocation.as_ref(),
        opts,
        bytes_done,
    )?;

    // --- FS-specific finalization (metadata rewrite / boot-sector patch) ---
    match fs_kind {
        FilesystemKind::Ntfs => {
            finalize_ntfs_partition(&mut writer, entry.target_size_bytes, relocation.as_ref())?;
        }
        FilesystemKind::Fat | FilesystemKind::Exfat => {
            finalize_fat_partition(&mut writer, entry.target_size_bytes, fs_kind)?;
        }
        _ => {}
    }

    // Cross-sector-size conversion, on the same handle that streamed the data.
    // Auto-detects NTFS/FAT32 from the just-landed boot sector (so it catches
    // the ESP, cloned through the raw `_` arm above as `Efi`); exFAT / MSR /
    // encrypted return Skipped. The ReadBack verify already ran per-block during
    // streaming, before this rewrite, so it cannot false-fail on the patched
    // boot sectors. Same offline window as the writes (table not yet planted).
    let mut converted = false;
    if convert {
        let outcome =
            phoenix_capture::apply_sector_conversion(&mut writer, entry.target_offset_bytes)?;
        converted = outcome.converted();
    }

    writer.flush()?;
    info!(
        partition = part.index,
        target_offset = entry.target_offset_bytes,
        converted,
        "cloned partition"
    );
    Ok((bytes_done, converted))
}

/// Copy every byte of `extents` from source to target in CHUNK_SIZE blocks,
/// translating destination offsets through the relocation map when shrinking,
/// and optionally reading each block back to verify it.
///
/// Double-buffered: a scoped reader thread walks the extents and reads ahead
/// through a small bounded channel while this thread writes (and, in ReadBack
/// mode, re-reads) the target — so on a disk-to-disk clone the source read
/// and the target write overlap instead of strictly alternating. The channel
/// depth bounds read-ahead memory to a few `CHUNK_SIZE` buffers, and the
/// write side preserves the exact serial semantics: same offsets, same
/// relocation splits, same short-read handling (`read_at` returning 0 ends
/// that extent early, matching the old `break`).
///
/// Public (rather than private to `run_clone`) so the T1 no-admin test can
/// drive it with a [`MemoryBlockSource`] against a temp-file-backed
/// [`PartitionWriter`]; production calls it with a [`PartitionReader`].
pub fn stream_extents(
    reader: &mut (impl phoenix_capture::BlockSource + Send),
    writer: &mut PartitionWriter,
    extents: &[Extent],
    relocation: Option<&RelocationMap>,
    opts: &CloneOptions,
    mut bytes_done: u64,
) -> Result<u64> {
    // Read-ahead depth: 4 chunks (16 MiB). Enough to hide read latency
    // behind writes without hoarding memory.
    const READ_AHEAD: usize = 4;

    std::thread::scope(|scope| -> Result<u64> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<
            std::result::Result<(u64, Vec<u8>), PhoenixError>,
        >(READ_AHEAD);

        scope.spawn(move || {
            for extent in extents {
                let base = extent.start_sector * EXTENT_LBA_BYTES as u64;
                let byte_len = extent.sector_count * EXTENT_LBA_BYTES as u64;
                let mut pos = 0u64;
                while pos < byte_len {
                    let to_read = (CHUNK_SIZE as u64).min(byte_len - pos) as usize;
                    let mut buf = vec![0u8; to_read];
                    match reader.read_at(base + pos, &mut buf) {
                        Ok(0) => break, // extent ends early (serial `break`)
                        Ok(n) => {
                            buf.truncate(n);
                            if tx.send(Ok((base + pos, buf))).is_err() {
                                return; // writer bailed (error/cancel)
                            }
                            pos += n as u64;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                }
            }
        });

        while let Ok(msg) = rx.recv() {
            if let Some(ref p) = opts.progress {
                if p.is_cancelled() {
                    return Err(PhoenixError::Cancelled);
                }
            }
            let (src_offset, data) = msg?;
            let n = data.len();

            match relocation {
                None => {
                    writer.write_at(src_offset, &data)?;
                    if opts.verify == CloneVerify::ReadBack {
                        verify_readback(writer, src_offset, &data)?;
                        bytes_done += n as u64;
                        bump(opts, bytes_done);
                    }
                }
                Some(map) => {
                    for seg in map.translate_write(src_offset, data.len()) {
                        let end = seg.source_offset + seg.len;
                        let chunk = &data[seg.source_offset..end];
                        writer.write_at(seg.dst_byte_offset, chunk)?;
                        if opts.verify == CloneVerify::ReadBack {
                            verify_readback(writer, seg.dst_byte_offset, chunk)?;
                        }
                    }
                    if opts.verify == CloneVerify::ReadBack {
                        bytes_done += n as u64;
                        bump(opts, bytes_done);
                    }
                }
            }
            bytes_done += n as u64;
            bump(opts, bytes_done);
        }
        Ok(bytes_done)
        // Early return drops `rx`; the reader thread's next send fails and it
        // exits, then the scope joins it.
    })
}

/// Re-read `len` bytes at `offset` from the target and compare to `expected`.
fn verify_readback(writer: &mut PartitionWriter, offset: u64, expected: &[u8]) -> Result<()> {
    let mut back = vec![0u8; expected.len()];
    writer.read_at(offset, &mut back)?;
    if blake3::hash(&back) != blake3::hash(expected) {
        return Err(PhoenixError::Other(format!(
            "clone read-back verification failed at target offset {offset}: the data written to \
             the target does not match the source"
        )));
    }
    Ok(())
}

fn bump(opts: &CloneOptions, bytes_done: u64) {
    if let Some(ref p) = opts.progress {
        p.set(bytes_done, String::new());
    }
}

fn planned_bytes(p: &PartitionInfo) -> u64 {
    match p.capture_mode {
        CaptureMode::UsedBlocks => p
            .usage
            .map(|u| u.used_bytes())
            .filter(|n| *n > 0)
            .unwrap_or(p.size_bytes),
        CaptureMode::Raw => p.size_bytes,
    }
}

/// Decide, name, and gate a cross-sector-size conversion for a clone. Pure and
/// side-effect-free so the GUI can describe the operation before it runs (it
/// passes `opt_in = true` because its hazard-dialog checkbox is the opt-in gate).
/// Mirrors the restore path's `plan_sector_conversion`:
///
/// * sizes match → `Ok(None)`;
/// * 4Kn → 512e with opt-in → `Ok(Some(report))` (refusing any
///   converted-partition shrink);
/// * 4Kn → 512e without opt-in → `Err(SectorConversionRequired)` (Alert A);
/// * unsupported mismatch → `Err(SectorSizeUnsupported)` (Alert B).
pub fn plan_sector_conversion(
    source: &DiskInfo,
    plan: &ClonePlan,
    target_sector_size: u32,
    opt_in: bool,
) -> Result<Option<ConversionReport>> {
    use phoenix_core::sector::{
        classify_sector_sizes, is_esp, partition_convert_role, PartitionConvertRole, SectorPlan,
    };

    match classify_sector_sizes(source.sector_size, target_sector_size)? {
        SectorPlan::Identical => Ok(None),
        SectorPlan::Convert4knTo512e => {
            if !opt_in {
                return Err(PhoenixError::SectorConversionRequired {
                    source_sector: source.sector_size,
                    target_sector: target_sector_size,
                });
            }
            let mut report = ConversionReport::default();
            for entry in &plan.entries {
                let Some(part) = source
                    .partitions
                    .iter()
                    .find(|p| p.index == entry.source_partition_index)
                else {
                    continue;
                };
                let name = format!("[{}] {}", part.index, part.name);
                match partition_convert_role(part.fs_kind) {
                    PartitionConvertRole::Convert => {
                        if entry.target_size_bytes < part.size_bytes {
                            return Err(PhoenixError::SectorConversionShrinkUnsupported {
                                partition_index: part.index,
                                target_size: entry.target_size_bytes,
                                source_size: part.size_bytes,
                            });
                        }
                        if is_esp(part.fs_kind) {
                            report.bootable = true;
                        }
                        report.converted_names.push(name);
                    }
                    PartitionConvertRole::NoFilesystem => {}
                    PartitionConvertRole::LeftUnconverted => report.unconverted_names.push(name),
                }
            }
            Ok(Some(report))
        }
    }
}

/// Emit the pre-flight conversion banner (Alerts D/E/F). Called by `clone_inner`
/// once the report is built; the GUI builds its hazard dialog from the same
/// report instead.
fn log_conversion_banner(source_ss: u32, target_ss: u32, report: &ConversionReport) {
    warn!(
        source_sector_size = source_ss,
        target_sector_size = target_ss,
        convert = report.converted_names.len(),
        "4Kn -> 512e conversion: filesystem boot sectors will be rewritten. The cloned disk is a \
         CONVERTED COPY, not a byte-identical clone."
    );
    for n in &report.converted_names {
        info!(partition = n.as_str(), "will convert 4Kn -> 512e");
    }
    if !report.bootable {
        warn!(
            "the EFI System Partition is not among the converted set: the cloned disk will hold \
             its data but will NOT boot"
        );
    }
    for n in &report.unconverted_names {
        warn!(
            partition = n.as_str(),
            "left unconverted (exFAT / encrypted): its bytes clone but the volume will be \
             unreadable on the 512e target"
        );
    }
}

fn build_gpt_entries(source: &DiskInfo, plan: &ClonePlan) -> Vec<GptEntry> {
    plan.entries
        .iter()
        .filter_map(|e| {
            let src = source
                .partitions
                .iter()
                .find(|p| p.index == e.source_partition_index)?;
            Some(GptEntry {
                offset_bytes: e.target_offset_bytes,
                size_bytes: e.target_size_bytes,
                type_guid: src.type_guid,
                // Preserve the source's partition unique GUID so a cloned
                // system disk keeps its BCD device references (BCD
                // identifies GPT partitions by this GUID).
                unique_guid: src.unique_guid,
                attributes: src.gpt_attributes,
                name: src.name.clone(),
            })
        })
        .collect()
}

fn build_mbr_entries(source: &DiskInfo, plan: &ClonePlan) -> Vec<MbrEntry> {
    plan.entries
        .iter()
        .filter_map(|e| {
            let src = source
                .partitions
                .iter()
                .find(|p| p.index == e.source_partition_index)?;
            Some(MbrEntry {
                offset_bytes: e.target_offset_bytes,
                size_bytes: e.target_size_bytes,
                partition_type: mbr_type_for(src.fs_kind),
                bootable: false,
            })
        })
        .collect()
}

fn mbr_type_for(fs: FilesystemKind) -> u8 {
    match fs {
        FilesystemKind::Ntfs | FilesystemKind::Exfat => 0x07,
        FilesystemKind::Fat => 0x0C, // FAT32 LBA; FAT16 volumes still mount.
        FilesystemKind::Efi => 0xEF,
        _ => 0x07,
    }
}
