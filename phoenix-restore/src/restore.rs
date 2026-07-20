use std::path::Path;

use phoenix_capture::fat::restore_fat;
use phoenix_capture::ntfs::restore_ntfs;
use phoenix_capture::raw::{restore_raw, PartitionWriter, RestoreOpts};
use phoenix_core::container::{PartitionIndexEntry, PhnxReader};
use phoenix_core::disk::{enumerate_disks, DiskInfo, FilesystemKind};
use phoenix_core::error::Result;
use phoenix_core::manifest::fs_kind_from_string;
use phoenix_core::sector::ConversionReport;
use phoenix_core::ProgressHandle;
use tracing::{info, warn};

use crate::grow::extend_ntfs_volume;
use crate::partition_table::{
    bring_disk_online, flush_disk, init_target_disk_as_gpt, init_target_disk_as_mbr,
    notify_disk_updated, update_partition_layout_existing, write_mbr_partition_layout,
    write_partition_layout, GptEntry as GptLayoutEntry, GptInitState, MbrEntry as MbrLayoutEntry,
};
use crate::plan::{RestoreMode, RestorePlan, RestorePlanEntry};

pub struct RestoreOptions {
    pub backup_path: std::path::PathBuf,
    pub plan: RestorePlan,
    pub verify_on_restore: bool,
    /// Opt-in to a 4Kn → 512e cross-sector-size conversion. When the backup's
    /// source disk uses 4096-byte logical sectors and the target uses 512-byte
    /// sectors, the filesystem boot sectors must be rewritten or the restored
    /// volumes come up RAW. This is the explicit acknowledgement (equivalent to
    /// the CLI `--convert-sector-size` flag / the GUI hazard-dialog checkbox)
    /// that the result is a converted copy, not a byte-identical restore.
    /// Ignored when the sector sizes already match.
    pub convert_sector_size: bool,
    /// After the restore completes and the disk is online, detect a Windows
    /// installation on the target and rebuild its boot environment
    /// (`bcdboot`/`bootsect`, drive-local only — see [`crate::bootrepair`]).
    /// Best-effort: a repair failure is recorded in the summary, never
    /// propagated, because the restored data is already valid.
    pub repair_boot: bool,
    pub progress: Option<ProgressHandle>,
}

/// Outcome of a successful restore.
#[derive(Debug, Clone, Default)]
pub struct RestoreSummary {
    /// Partitions whose data was written to the target disk.
    pub partitions_restored: u32,
    /// Partitions whose target size differed from the backup (grow,
    /// shrink, or layout slack expansion on a larger disk).
    pub partitions_resized: u32,
    /// True if at least one restored partition was a BitLocker **locked**
    /// (ciphertext) image. The restored volume is encrypted and still needs
    /// the original key/recovery password to unlock; callers should surface
    /// this, and note that Windows may need a reboot or a disk reconnect
    /// before it re-recognizes the volume (see the post-restore rescan).
    pub restored_locked_bitlocker: bool,
    /// Number of partitions whose boot sector was rewritten 4Kn → 512e.
    pub partitions_converted: u32,
    /// `Some(true/false)` when a 4Kn → 512e conversion ran: whether the ESP was
    /// among the converted set (i.e. the restored disk should be bootable).
    /// `None` when no conversion occurred.
    pub conversion_bootable: Option<bool>,
    /// Human names of the partitions converted 4Kn → 512e (Alert G summary).
    pub conversion_converted_names: Vec<String>,
    /// Human names of partitions the conversion left untouched because v1 does
    /// not convert their filesystem (exFAT / BitLocker ciphertext) — restored
    /// but unreadable on the 512e target (Alert E).
    pub conversion_unconverted_names: Vec<String>,
    /// Outcome of the opt-in post-restore boot repair; `None` when it was
    /// not requested.
    pub boot_repair: Option<crate::bootrepair::BootRepairStatus>,
}

pub fn run_restore(opts: RestoreOptions) -> Result<RestoreSummary> {
    let op_start = std::time::Instant::now();
    let mut reader = PhnxReader::open(&opts.backup_path)?;
    opts.plan.validate_against_backup(&reader.manifest)?;
    // Pre-flight: refuse a shrink whose source has data physically
    // beyond the new partition's end *before* we touch the target disk.
    // This has to come before `init_target_disk_as_gpt` — a half-
    // initialized GPT plus an aborted restore would leave the user's
    // disk in worse shape than not starting at all.
    //
    // For NTFS this also proves the metadata rewrite will fit, adjusting the
    // relocation where it otherwise wouldn't. The resulting plans are what
    // the streaming loop below uses: rebuilding a map there would discard
    // any adjustment made here and re-introduce the failure this pre-flight
    // exists to prevent.
    let mut shrink_plans = opts
        .plan
        .validate_extents_fit(&mut reader, opts.progress.as_ref())?;

    let disks = enumerate_disks()?;
    let disk = disks
        .into_iter()
        .find(|d| d.index == opts.plan.target_disk_index)
        .ok_or_else(|| phoenix_core::error::PhoenixError::Disk("target disk not found".into()))?;

    // Reject an overlapping or oversized layout before touching the disk.
    // This catches a hand-edited plan.toml as well as any auto-layout bug.
    opts.plan.validate_layout(disk.size_bytes)?;

    // Cross-sector-size pre-flight: compare the backup's source logical sector
    // size against the target disk's. Returns `Some(report)` for an opted-in
    // 4Kn → 512e conversion, `None` when sizes match, and errors out for a
    // missing opt-in (SectorConversionRequired), an unsupported mismatch
    // (SectorSizeUnsupported), or a converted-partition shrink — all before the
    // disk is touched, so an aborted pre-flight leaves the target untouched.
    let conversion = plan_sector_conversion(
        &reader.manifest,
        &opts.plan,
        disk.sector_size,
        opts.convert_sector_size,
    )?;
    if let Some(report) = &conversion {
        log_conversion_banner(reader.manifest.disk.sector_size, disk.sector_size, report);
    }

    info!(
        "Restoring to disk {} ({}); source style={}, target was {}",
        disk.index,
        disk.path,
        reader.manifest.disk.style,
        if disk.is_gpt {
            "GPT"
        } else {
            "MBR/uninitialized"
        }
    );

    // PHASE 1 — initialize the target as GPT *without* any partition
    // entries. We deliberately split the GPT setup into "stamp the
    // header now" and "write entries later" because
    // `IOCTL_DISK_SET_DRIVE_LAYOUT_EX` is a partmgr-visible event:
    // populating partition entries here would cause mountmgr to lazy-
    // mount an empty volume on each one mid-write, after which raw
    // disk-handle writes to those regions get rejected with
    // `ERROR_ACCESS_DENIED` (Win32 5). Decision is keyed off the
    // *source* manifest style — a freshly `diskpart clean`-ed target
    // reports as MBR/RAW so `disk.is_gpt` would be `false` and we'd
    // skip the whole step.
    let mode = opts.plan.mode();
    let source_was_gpt = reader.manifest.disk.style.eq_ignore_ascii_case("gpt");

    // Partial-mode table re-initialization: the GUI's layout editor sets
    // `reinit_style` when the planned layout no longer derives from the
    // target's current table (blank layout / MBR↔GPT switch). Full-disk mode
    // re-inits from the source style regardless, so the field is ignored
    // there.
    let reinit_style = match opts.plan.reinit_style.as_deref() {
        None => None,
        Some(s) if s.eq_ignore_ascii_case("gpt") => Some(true),
        Some(s) if s.eq_ignore_ascii_case("mbr") => Some(false),
        Some(other) => {
            return Err(phoenix_core::error::PhoenixError::Plan(format!(
                "unknown reinit_style {other:?} (expected \"gpt\" or \"mbr\")"
            )));
        }
    };
    let reinit = mode == RestoreMode::Partial && reinit_style.is_some();
    // The style of the table this restore will leave on the disk.
    let table_is_gpt = if mode == RestoreMode::FullDisk {
        source_was_gpt
    } else {
        reinit_style.unwrap_or(disk.is_gpt)
    };

    // MBR entries are 32-bit sector fields: nothing may extend past ~2 TiB.
    // `default_plan_from_backup` already clamps its layout, but a hand-edited
    // plan.toml can still overshoot — and the failure mode is nasty (the data
    // restore succeeds, then the volume never mounts because the on-disk
    // table can't represent it), so refuse it up front. Partial mode now
    // rewrites the whole table, so every entry counts, not just restored
    // ones — and MBR holds at most four primaries (no extended support).
    if !table_is_gpt {
        let counted = if mode == RestoreMode::FullDisk {
            opts.plan.entries.iter().filter(|e| e.restore).count()
        } else {
            opts.plan.entries.len()
        };
        if counted > 4 {
            return Err(phoenix_core::error::PhoenixError::Plan(format!(
                "{counted} partitions planned, but an MBR table holds at most 4 primary \
                 partitions (extended partitions aren't supported)"
            )));
        }
        for entry in &opts.plan.entries {
            if mode == RestoreMode::FullDisk && !entry.restore {
                continue;
            }
            let end = entry
                .target_offset_bytes
                .saturating_add(entry.target_size_bytes);
            if end > crate::plan::MBR_ADDRESSABLE_BYTES {
                return Err(phoenix_core::error::PhoenixError::Plan(format!(
                    "partition {} would end at byte {end}, past the ~2 TiB limit MBR's \
                     32-bit sector fields can address; keep the layout below 2 TiB or \
                     use a GPT layout for larger targets",
                    entry.target_partition_index
                )));
            }
        }
    }

    // --- Build the ordered step plan up front so the GUI modal can render
    // upcoming steps grayed out. Steps are condition-aware: disk init only
    // runs for a full-disk restore, the layout write only when we (re)stamp
    // the partition table, and "Resizing partitions" only when some NTFS
    // volume is being grown. The `*_step` indices captured here are reused
    // by the `set_step` calls below as the worker advances. ---
    let restoring: Vec<_> = opts.plan.entries.iter().filter(|e| e.restore).collect();
    let restore_count = restoring.len();
    let any_grow = restoring.iter().any(|entry| {
        let Some(src) = entry.source_partition_index else {
            return false;
        };
        reader
            .index
            .iter()
            .find(|e| e.index == src)
            .map(|idx| {
                matches!(idx.fs_kind, FilesystemKind::Ntfs)
                    && entry.target_size_bytes > idx.original_size
            })
            .unwrap_or(false)
    });
    let needs_init = mode == RestoreMode::FullDisk || reinit;
    // Partial mode rewrites the table too now (GPT via the existing disk
    // GUID, MBR via a fresh full-table write), so the layout step is
    // unconditional; the only silent skip left is a GPT target whose disk
    // GUID we couldn't read.
    let writes_layout = needs_init || mode == RestoreMode::Partial;

    let mut steps: Vec<String> = Vec::new();
    steps.push("Preparing restore".to_string());
    let init_step = if needs_init {
        steps.push("Initializing target disk".to_string());
        Some(steps.len() - 1)
    } else {
        None
    };
    let restore_step_base = steps.len();
    for (i, entry) in restoring.iter().enumerate() {
        let name = entry
            .source_partition_index
            .and_then(|src| reader.index.iter().find(|e| e.index == src))
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "partition".to_string());
        steps.push(format!(
            "Restoring partition {} of {} — {}",
            i + 1,
            restore_count,
            name
        ));
    }
    let layout_step = if writes_layout {
        steps.push("Writing partition layout".to_string());
        Some(steps.len() - 1)
    } else {
        None
    };
    // Online comes BEFORE resize in both this list and execution: the extend
    // pass needs mounted volumes, and a restored disk whose (re-stamped)
    // signature trips the SAN/duplicate-signature policy stays offline — with
    // no volume devices at all — until `bring_disk_online` runs. Extending
    // first meant the poll always timed out on such disks (caught on a real
    // USB target whose volume appeared seconds after the online phase).
    steps.push("Bringing disk online".to_string());
    let online_step = steps.len() - 1;
    let resize_step = if any_grow {
        steps.push("Resizing partitions".to_string());
        Some(steps.len() - 1)
    } else {
        None
    };

    if let Some(ref p) = opts.progress {
        p.set_steps(steps);
        p.set_step(0);
    }

    let init_as_gpt = (mode == RestoreMode::FullDisk && source_was_gpt) || (reinit && table_is_gpt);
    let init_as_mbr =
        (mode == RestoreMode::FullDisk && !source_was_gpt) || (reinit && !table_is_gpt);
    let gpt_state: Option<GptInitState> = if init_as_gpt {
        if let Some(ref p) = opts.progress {
            if let Some(step) = init_step {
                p.set_step(step);
            }
        }
        // A reinit from an MBR-imaged source has no source GPT disk GUID —
        // `init_target_disk_as_gpt` generates a fresh one in that case.
        let source_disk_guid = parse_disk_guid(reader.manifest.disk.disk_guid.as_deref());
        Some(init_target_disk_as_gpt(
            &disk.path,
            disk.size_bytes,
            source_disk_guid,
        )?)
    } else {
        if init_as_mbr {
            if let Some(ref p) = opts.progress {
                if let Some(step) = init_step {
                    p.set_step(step);
                }
            }
            // Prefer the imaged disk's signature; a GPT-imaged source has
            // none, so derive a non-zero one rather than leaving 0 for
            // Windows to squat a collision dialog on.
            let sig = (reader.header.disk_signature & 0xFFFF_FFFF) as u32;
            let sig = if sig != 0 {
                sig
            } else {
                fallback_mbr_signature()
            };
            init_target_disk_as_mbr(&disk.path, sig)?;
        }
        None
    };

    // Sum the per-partition `used_bytes` from the manifest so the GUI's
    // progress bar — which renders `current` and `total` through
    // `format_bytes` — shows real-world scale ("1.6 GB / 21 GB") instead
    // of the chunk count it used to ("1.57 KB / 5.29 KB" for a 21 GB
    // partition, courtesy of `format_bytes(5419)`).
    let total_bytes: u64 = restoring
        .iter()
        .filter_map(|entry| {
            let src = entry.source_partition_index?;
            reader.manifest.partitions.iter().find(|p| p.index == src)
        })
        .map(|p| p.used_bytes)
        .sum();

    if let Some(ref p) = opts.progress {
        p.begin(total_bytes.max(1), "Restore");
    }

    let mut bytes_done = 0u64;
    let mut summary = RestoreSummary::default();
    // (target_offset, target_size) of every restored BitLocker-locked
    // (ciphertext) partition, so we can nudge Windows to re-read the volume
    // after bring-online (PHASE 6). Windows may have a stale, pre-restore
    // view of a re-appearing FVE volume cached; a dismount lets it remount
    // fresh and read the `-FVE-FS-` header we just wrote.
    let mut locked_bitlocker_parts: Vec<(u64, u64)> = Vec::new();
    // Sequence among restoring entries only, used to index into the
    // per-partition steps declared in the plan above.
    let mut restore_seq = 0usize;

    // A partial restore writes into an *existing* partition table whose target
    // slots may still carry live mounted volumes; Windows bounces raw writes to
    // a mounted volume's sectors with ERROR_ACCESS_DENIED (Win32 5). Before
    // writing each slot we lock + dismount its covering volume and keep the
    // guard here so nothing re-mounts mid-write. The guards are dropped after
    // PHASE 3 re-stamps the layout — PHASE 4's FSCTL_EXTEND_VOLUME needs the
    // volumes mounted again, and the remount then re-reads the boot sectors we
    // just wrote. (A full-disk restore never populates this: it plants the
    // table last, so no volume is mounted while data lands.)
    let mut locked_target_volumes: Vec<phoenix_core::disk::LockedVolume> = Vec::new();

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
        let src_index = entry.source_partition_index.ok_or_else(|| {
            phoenix_core::error::PhoenixError::Plan("restore entry missing source".into())
        })?;
        let idx_entry = reader
            .index
            .iter()
            .find(|e| e.index == src_index)
            .cloned()
            .ok_or_else(|| {
                phoenix_core::error::PhoenixError::Plan("partition index missing".into())
            })?;

        let part_manifest = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == src_index)
            .unwrap();

        let fs = fs_kind_from_string(&part_manifest.fs);
        if part_manifest.bitlocker.as_deref() == Some("locked") {
            summary.restored_locked_bitlocker = true;
            locked_bitlocker_parts.push((entry.target_offset_bytes, entry.target_size_bytes));
            warn!(
                partition = src_index,
                "this partition was captured from a BitLocker-LOCKED volume: restoring raw \
                 ciphertext. The restored volume will be encrypted and requires the original \
                 BitLocker key/recovery password to unlock."
            );
            if let Some(ref p) = opts.progress {
                p.set_detail(format!(
                    "Partition {src_index} is BitLocker ciphertext — the restored volume will \
                     need its original recovery key"
                ));
            }
        }
        // Partial restore only: if a live volume still covers this target
        // slot, lock + dismount it first so the raw writes below aren't
        // rejected with ERROR_ACCESS_DENIED (Win32 5). Full-disk restore
        // reaches the writes with the table not yet planted, so no volume is
        // mounted and there is nothing to clear. `volume_path` is populated by
        // `enumerate_disks` (drive-letter or `\\?\Volume{GUID}` device); `None`
        // means the slot is already unmounted (raw/empty or a prior restore
        // left it dismounted) and needs no guard. A table REINIT behaves like
        // full-disk here: PHASE 1 already wiped the partition table, so the
        // pre-init volume snapshot is stale (locking a vanished volume fails
        // with ERROR_NOT_READY) and a fresh slot index can coincidentally
        // match a partition of the destroyed table — skip the guard entirely.
        if mode == RestoreMode::Partial && !reinit {
            if let Some(vol) = disk
                .partitions
                .iter()
                .find(|p| p.index == entry.target_partition_index)
                .and_then(|p| p.volume_path.clone())
            {
                let guard = phoenix_core::disk::LockedVolume::acquire(&vol).map_err(|e| {
                    phoenix_core::error::PhoenixError::Disk(format!(
                        "could not clear the volume {vol} occupying target partition {} before \
                         restoring into it (close any open files/handles on it and retry): {e}",
                        entry.target_partition_index
                    ))
                })?;
                info!(
                    volume = vol.as_str(),
                    partition = entry.target_partition_index,
                    "dismounted live target-slot volume for partial restore"
                );
                locked_target_volumes.push(guard);
            }
        }

        let mut writer =
            PartitionWriter::open_disk(&disk.path, entry.target_offset_bytes, disk.sector_size)?;

        summary.partitions_restored += 1;
        if entry.target_size_bytes != idx_entry.original_size {
            summary.partitions_resized += 1;
        }

        if let Some(ref p) = opts.progress {
            p.set_step(restore_step_base + restore_seq);
            p.set_detail(String::new());
        }
        restore_seq += 1;

        info!(
            "Restoring partition {} ({}) to offset {}",
            src_index, idx_entry.name, entry.target_offset_bytes
        );

        // For NTFS shrink restores the relocation map comes from the
        // pre-flight above, which already proved the metadata rewrite fits
        // (and re-planned it where it didn't). It threads through
        // `restore_ntfs` -> `restore_raw` -> the metadata rewriter. `None`
        // is identical to the pre-relocation behavior, so non-shrink and
        // non-NTFS paths see no functional change.
        let relocation = match fs {
            FilesystemKind::Ntfs if entry.target_size_bytes < idx_entry.original_size => {
                let plan = shrink_plans.take(src_index).ok_or_else(|| {
                    phoenix_core::error::PhoenixError::Other(format!(
                        "internal error: partition {src_index} shrinks but the pre-flight \
                         produced no plan for it"
                    ))
                })?;
                info!(
                    partition = src_index,
                    entries = plan.map.entries.len(),
                    new_total_clusters = plan.map.new_total_clusters,
                    replanned_records = plan.replanned_records,
                    "using the pre-flighted NTFS relocation map for shrink"
                );
                Some(plan.map)
            }
            _ => None,
        };

        let restore_opts = RestoreOpts {
            verify: opts.verify_on_restore,
            progress: opts.progress.as_ref(),
            bytes_done,
        };
        match fs {
            FilesystemKind::Ntfs => {
                bytes_done += restore_ntfs(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    entry.target_size_bytes,
                    restore_opts,
                    relocation.as_ref(),
                )?;
            }
            FilesystemKind::Fat | FilesystemKind::Exfat => {
                bytes_done += restore_fat(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    entry.target_size_bytes,
                    fs,
                    restore_opts,
                )?;
            }
            FilesystemKind::Refs => {
                // ReFS restores raw (no boot-sector patching, no metadata
                // rewrite) but must never land in a smaller partition: there
                // is no offline shrink for ReFS and we don't rewrite its
                // allocators, so a smaller target would silently truncate
                // allocated clusters. The planner already refuses ReFS
                // resize (`partition_allows_resize`); this guard covers
                // hand-edited plans. Grow is fine — the volume mounts at
                // source size and PHASE 4 extends it via FSCTL_EXTEND_VOLUME.
                if entry.target_size_bytes < idx_entry.original_size {
                    return Err(phoenix_core::error::PhoenixError::Other(format!(
                        "partition {src_index} is ReFS and cannot be shrunk \
                         (target {} bytes < source {} bytes); restore it at \
                         its original size or larger",
                        entry.target_size_bytes, idx_entry.original_size
                    )));
                }
                bytes_done += restore_raw(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    restore_opts,
                    None,
                    entry.target_size_bytes,
                )?;
            }
            _ => {
                bytes_done += restore_raw(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    restore_opts,
                    None,
                    entry.target_size_bytes,
                )?;
            }
        }

        // Cross-sector-size conversion: rewrite the just-landed boot sector to
        // 512e so the volume mounts instead of coming up RAW. Auto-detects
        // NTFS/FAT32 from the boot sector (so it catches the ESP, which restores
        // through the raw path as `Efi`); exFAT / MSR / encrypted return Skipped.
        // Runs on the same handle that streamed the data, in the same offline
        // window (before the partition table is planted), so nothing has
        // re-mounted the volume underneath us. The resize finalizers only touch
        // the boot sector on *shrink*, which the pre-flight has already refused
        // for a converted partition, so there is no double-write to reconcile.
        if conversion.is_some() {
            let outcome =
                phoenix_capture::apply_sector_conversion(&mut writer, entry.target_offset_bytes)?;
            if outcome.converted() {
                summary.partitions_converted += 1;
            }
        }
    }

    if let Some(report) = &conversion {
        summary.conversion_bootable = Some(report.bootable);
        summary.conversion_converted_names = report.converted_names.clone();
        summary.conversion_unconverted_names = report.unconverted_names.clone();
    }

    // PHASE 3 — now that every partition's bytes are on disk, plant
    // the partition entry table on top of them. We held off on this
    // step until the data writes were done so partmgr/volmgr couldn't
    // notice the entries and lazy-mount empty volumes that would
    // otherwise bounce raw writes with `ERROR_ACCESS_DENIED`.
    //
    // Belt-and-suspenders flush: raw `\\.\PhysicalDriveN` writes are
    // already nominally uncached, but the cost of an explicit
    // `FlushFileBuffers` here is one IOCTL and the safety win against
    // a mountmgr-vs-data-write race when `IOCTL_DISK_UPDATE_PROPERTIES`
    // fires below is real.
    flush_disk(&disk.path);
    if let Some(state) = gpt_state {
        // Full-disk GPT restore or a partial reinit-as-GPT: the disk was
        // freshly initialized in PHASE 1, plant the whole table now.
        if let Some(ref p) = opts.progress {
            if let Some(step) = layout_step {
                p.set_step(step);
            }
        }
        let gpt_entries = build_gpt_layout_entries(&opts.plan, &reader, &disk, mode);
        write_partition_layout(&disk.path, &gpt_entries, &state)?;
        if let Some(ref p) = opts.progress {
            p.set_detail("Notifying Windows of the new partition layout".to_string());
        }
        notify_disk_updated(&disk.path);
    } else if init_as_mbr {
        // Full-disk MBR restore or a partial reinit-as-MBR.
        if let Some(ref p) = opts.progress {
            if let Some(step) = layout_step {
                p.set_step(step);
            }
        }
        let mbr_entries = build_mbr_layout_entries(&opts.plan, &reader, &disk, mode);
        write_mbr_partition_layout(&disk.path, &mbr_entries)?;
        notify_disk_updated(&disk.path);
    } else if mode == RestoreMode::Partial {
        // Rewrite the existing table in place. Entries deleted from the plan
        // simply aren't written — but a deleted partition may still carry a
        // live mounted volume, and partmgr can refuse (or worse, survive) a
        // table rewrite underneath one, so lock + dismount those first. The
        // guards join `locked_target_volumes` and drop right after this
        // phase.
        lock_removed_target_volumes(&opts.plan, &disk, &mut locked_target_volumes)?;
        if let Some(ref p) = opts.progress {
            if let Some(step) = layout_step {
                p.set_step(step);
            }
        }
        if disk.is_gpt {
            if let Some(guid_bytes) = disk.disk_guid {
                let disk_guid = bytes_to_guid(guid_bytes);
                let gpt_entries = build_gpt_layout_entries(&opts.plan, &reader, &disk, mode);
                update_partition_layout_existing(
                    &disk.path,
                    &gpt_entries,
                    &disk_guid,
                    disk.size_bytes,
                )?;
                notify_disk_updated(&disk.path);
            } else {
                warn!(
                    "target disk GUID unavailable; leaving the GPT partition table untouched \
                     (restored data is on disk, but moves/resizes/deletes did not reach the \
                     table)"
                );
            }
        } else {
            // MBR partial: `set_drive_layout_mbr` leaves the existing disk
            // signature alone (Signature = 0 on SET means keep), so this is
            // a pure entry rewrite, exactly like the GPT arm above.
            let mbr_entries = build_mbr_layout_entries(&opts.plan, &reader, &disk, mode);
            write_mbr_partition_layout(&disk.path, &mbr_entries)?;
            notify_disk_updated(&disk.path);
        }
    }

    // The restored slots are on disk and the layout has been (re)stamped, so
    // release the target-slot volume locks now. Windows re-mounts each volume
    // fresh — re-reading the boot sector we just restored — and PHASE 4's
    // FSCTL_EXTEND_VOLUME needs those volumes mounted to grow them. No-op for a
    // full-disk restore (the Vec is empty). Explicit drop so the ordering
    // against PHASE 4 is unmistakable rather than relying on end-of-scope.
    drop(locked_target_volumes);

    // PHASE 4 — clear the "offline" disk attribute so volumes
    // auto-mount with drive letters. Without this the user has to
    // right-click → Online in Disk Management every time. See
    // `bring_disk_online` for the full rationale (SAN policy +
    // duplicate-signature handling). Best-effort: a warn is logged on
    // failure, but the restored data is already valid on disk.
    //
    // This MUST precede the extend pass below: a restored disk whose
    // re-stamped signature keeps it offline has no volume devices, so
    // `extend_ntfs_volume`'s mount-wait can only time out (observed on a
    // real USB target — the volume appeared seconds after onlining).
    if let Some(ref p) = opts.progress {
        p.set_step(online_step);
        p.set_detail(String::new());
    }
    bring_disk_online(&disk.path);

    // PHASE 5 — for every NTFS/ReFS partition we wrote at a larger size
    // than the source, ask the filesystem to extend its allocation
    // metadata to fill the new slot. We deliberately left the
    // boot sector at source size in `patch_ntfs_size` so the volume
    // would actually mount (Windows refuses to mount NTFS where
    // `$Bitmap` doesn't cover the cluster range advertised by the
    // boot sector — the symptom that prompted this whole pass:
    // post-restore C: came up RAW). `FSCTL_EXTEND_VOLUME` is the same
    // mechanism Disk Management's "Extend Volume" uses, so we're
    // letting Windows do its own bookkeeping on a now-mounted volume
    // rather than patching `$Bitmap` ourselves.
    //
    // Degrades gracefully: a failure here means the partition is on
    // disk at the correct size with the source's NTFS still mounted
    // at the source size. The user can extend manually via Disk
    // Management. We log a warning but don't fail the restore.
    for entry in &opts.plan.entries {
        if !entry.restore {
            continue;
        }
        let Some(src_index) = entry.source_partition_index else {
            continue;
        };
        let idx_entry = match reader.index.iter().find(|e| e.index == src_index) {
            Some(i) => i,
            None => continue,
        };
        // ReFS joins NTFS here: FSCTL_EXTEND_VOLUME is filesystem-agnostic
        // (Disk Management's "Extend Volume" uses it for both) and ReFS
        // supports online extension. The restored ReFS boot sector was left
        // untouched, so like NTFS it mounts at source size until this runs.
        if !matches!(
            idx_entry.fs_kind,
            FilesystemKind::Ntfs | FilesystemKind::Refs
        ) {
            continue;
        }
        if entry.target_size_bytes <= idx_entry.original_size {
            continue;
        }
        if let Some(ref p) = opts.progress {
            if let Some(step) = resize_step {
                p.set_step(step);
            }
            p.set_detail(format!("Extending volume for partition {}", src_index));
        }
        if let Err(e) = extend_ntfs_volume(
            disk.index,
            entry.target_offset_bytes,
            entry.target_size_bytes,
        ) {
            warn!(
                partition = src_index,
                error = %e,
                "FSCTL_EXTEND_VOLUME failed; the partition was restored at source size and \
                 can be extended manually via Disk Management"
            );
        }
    }

    // PHASE 6 — if we restored any BitLocker-locked (ciphertext) partition,
    // nudge Windows to re-read its volume. The ciphertext bytes were written
    // before the volume mounted (the partition table is planted last), but
    // Windows can still serve a stale, pre-restore view of a re-appearing FVE
    // volume from its per-session cache, showing the volume as "decrypted /
    // unknown" instead of prompting for the key. Dismounting lets it remount
    // fresh. Best-effort and non-fatal — the on-disk ciphertext is already
    // correct regardless; on removable media a reboot or reconnect may still
    // be required, which we can't force from software.
    if !locked_bitlocker_parts.is_empty() {
        for (offset, length) in &locked_bitlocker_parts {
            rescan_restored_volume(disk.index, *offset, *length);
        }
        warn!(
            "restored an encrypted BitLocker volume; if Windows still shows it as \
             unrecognized/decrypted, reboot or reconnect the disk, then unlock it with the \
             original BitLocker key/recovery password"
        );
    }

    // PHASE 7 — opt-in boot repair: the layout is stamped and the disk is
    // online, so a Windows install on the target (if any) can have its boot
    // environment rebuilt. Best-effort by design — see `RestoreOptions::
    // repair_boot`.
    if opts.repair_boot {
        if let Some(ref p) = opts.progress {
            p.set_detail("Repairing boot files on the target disk".to_string());
        }
        summary.boot_repair = Some(crate::bootrepair::repair_target_disk_status(disk.index));
    }

    if let Some(ref p) = opts.progress {
        p.end();
    }
    let total_secs = op_start.elapsed().as_secs_f64();
    info!(
        partitions_restored = summary.partitions_restored,
        partitions_resized = summary.partitions_resized,
        partitions_converted = summary.partitions_converted,
        restored_locked_bitlocker = summary.restored_locked_bitlocker,
        elapsed = %phoenix_core::progress::format_elapsed(total_secs),
        throughput = %phoenix_core::progress::format_rate(bytes_done, total_secs),
        "Restore complete"
    );
    Ok(summary)
}

/// Decide, name, and gate a cross-sector-size conversion for a restore plan.
/// Pure and side-effect-free so the CLI/GUI can call it to describe the
/// operation *before* it runs (the GUI passes `opt_in = true` because its
/// hazard-dialog checkbox is the opt-in gate).
///
/// * sizes match → `Ok(None)`, restore verbatim;
/// * 4Kn → 512e with opt-in → `Ok(Some(report))` (having refused any
///   converted-partition shrink);
/// * 4Kn → 512e without opt-in → `Err(SectorConversionRequired)` (Alert A);
/// * unsupported mismatch (e.g. 512e → 4Kn) → `Err(SectorSizeUnsupported)`
///   (Alert B).
pub fn plan_sector_conversion(
    manifest: &phoenix_core::manifest::BackupManifest,
    plan: &RestorePlan,
    target_sector_size: u32,
    opt_in: bool,
) -> Result<Option<ConversionReport>> {
    use phoenix_core::sector::{
        classify_sector_sizes, is_esp, partition_convert_role, PartitionConvertRole, SectorPlan,
    };

    let source_ss = manifest.disk.sector_size;
    match classify_sector_sizes(source_ss, target_sector_size)? {
        SectorPlan::Identical => Ok(None),
        SectorPlan::Convert4knTo512e => {
            if !opt_in {
                return Err(
                    phoenix_core::error::PhoenixError::SectorConversionRequired {
                        source_sector: source_ss,
                        target_sector: target_sector_size,
                    },
                );
            }
            let mut report = ConversionReport::default();
            for entry in &plan.entries {
                if !entry.restore {
                    continue;
                }
                let Some(src) = entry.source_partition_index else {
                    continue;
                };
                let Some(part) = manifest.partitions.iter().find(|p| p.index == src) else {
                    continue;
                };
                let fs = fs_kind_from_string(&part.fs);
                let name = format!("[{}] {}", part.index, part.name);
                match partition_convert_role(fs) {
                    PartitionConvertRole::Convert => {
                        // v1 does not combine conversion with a shrink: the NTFS
                        // relocation / metadata rewrite path isn't sector-size-
                        // aware. Refuse before touching the disk.
                        if entry.target_size_bytes < part.original_size {
                            return Err(
                                phoenix_core::error::PhoenixError::SectorConversionShrinkUnsupported {
                                    partition_index: src,
                                    target_size: entry.target_size_bytes,
                                    source_size: part.original_size,
                                },
                            );
                        }
                        if is_esp(fs) {
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

/// Emit the pre-flight warning banner for a conversion (Alerts D/E/F surface
/// here in the CLI log stream). Called by `run_restore` once the report is in
/// hand; the GUI builds its hazard dialog from the same report instead.
fn log_conversion_banner(source_ss: u32, target_ss: u32, report: &ConversionReport) {
    warn!(
        source_sector_size = source_ss,
        target_sector_size = target_ss,
        convert = report.converted_names.len(),
        "4Kn -> 512e conversion: filesystem boot sectors will be rewritten. The restored disk is \
         a CONVERTED COPY, not a byte-identical restore."
    );
    for n in &report.converted_names {
        info!(partition = n.as_str(), "will convert 4Kn -> 512e");
    }
    if !report.bootable {
        warn!(
            "the EFI System Partition is not among the converted set: the restored disk will hold \
             its data but will NOT boot"
        );
    }
    for n in &report.unconverted_names {
        warn!(
            partition = n.as_str(),
            "left unconverted (exFAT / encrypted): its bytes restore but the volume will be \
             unreadable on the 512e target"
        );
    }
}

/// Best-effort: after bring-online, wait briefly for the mount manager to give
/// the restored partition a volume path, then `FSCTL_DISMOUNT_VOLUME` it so it
/// remounts fresh and re-reads its on-disk boot sector. Used only for restored
/// BitLocker-locked (ciphertext) partitions, where Windows can otherwise serve
/// a stale cached view of the re-appearing FVE volume. Silently gives up if no
/// volume path appears in the poll window — the restored bytes are correct
/// either way, and a reboot/reconnect remains the fallback.
fn rescan_restored_volume(disk_index: u32, offset: u64, length: u64) {
    use phoenix_core::disk::{dismount_volume, find_volume_for_partition};
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(vol) = find_volume_for_partition(disk_index, offset, length) {
            match dismount_volume(&vol) {
                Ok(()) => info!(
                    volume = vol.as_str(),
                    "dismounted restored BitLocker volume so Windows re-reads it"
                ),
                Err(e) => warn!(
                    volume = vol.as_str(),
                    error = %e,
                    "could not dismount restored BitLocker volume; a reboot/reconnect will refresh it"
                ),
            }
            return;
        }
        if std::time::Instant::now() >= deadline {
            info!(
                disk = disk_index,
                offset,
                "restored BitLocker volume did not get a drive path in time to rescan; \
                 a reboot/reconnect will let Windows recognize it"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Parse the `BackupManifest::disk.disk_guid` field (a 36-char dashed
/// UUID like `"01234567-89ab-cdef-0123-456789abcdef"`) back into raw
/// little-endian-stored bytes matching the on-disk GPT header layout —
/// the same byte order `phoenix-core::disk` reads via `mem::transmute`
/// from `windows-sys::core::GUID`. Returns `None` when the manifest
/// didn't record a GUID (MBR sources) or when the string is malformed.
fn bytes_to_guid(bytes: [u8; 16]) -> windows_sys::core::GUID {
    windows_sys::core::GUID {
        data1: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        data2: u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        data3: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
        data4: bytes[8..16].try_into().unwrap(),
    }
}

fn parse_disk_guid(s: Option<&str>) -> Option<[u8; 16]> {
    let s = s?;
    let parsed = uuid::Uuid::parse_str(s).ok()?;
    // `Uuid::as_bytes` returns big-endian (network order). The on-disk
    // GPT header — and the `disk_guid` we store at capture time — is the
    // little-endian "mixed" layout that Windows exposes through the
    // `GUID` struct (data1/data2/data3 in little-endian, data4 untouched).
    let be = parsed.as_bytes();
    let mut out = [0u8; 16];
    out[0] = be[3];
    out[1] = be[2];
    out[2] = be[1];
    out[3] = be[0];
    out[4] = be[5];
    out[5] = be[4];
    out[6] = be[7];
    out[7] = be[6];
    out[8..16].copy_from_slice(&be[8..16]);
    Some(out)
}

/// Build the per-partition descriptors for `set_drive_layout`. Joins
/// each plan entry to the backup's `PartitionIndexEntry` (restored
/// partitions; carries `type_guid` from the source) and to the live
/// target disk's `PartitionInfo` (preserved partitions in partial
/// mode; carries `type_guid` / `gpt_attributes` / `name` from the
/// existing on-disk entry).
///
/// This is the load-bearing fix for the EFI/MSR/Recovery-as-Basic-Data
/// bug: previously every partition was stamped with the Basic Data
/// GUID, so EFI got a drive letter, MSR became visible, and Recovery
/// lost its no-auto-mount flag. Now we preserve the source's GUID.
///
/// Attributes are intentionally hardcoded to 0 for *restored* entries
/// because the current backup format does not capture GPT attributes
/// (per the design decision to keep older backups restorable as-is
/// rather than inferring attributes from type GUIDs). Preserved
/// partitions in partial mode still carry their live attributes.
fn build_gpt_layout_entries(
    plan: &RestorePlan,
    reader: &PhnxReader,
    target_disk: &DiskInfo,
    mode: RestoreMode,
) -> Vec<GptLayoutEntry> {
    let pick: Vec<&RestorePlanEntry> = match mode {
        RestoreMode::FullDisk => plan.entries.iter().filter(|e| e.restore).collect(),
        RestoreMode::Partial => plan.entries.iter().collect(),
    };
    pick.into_iter()
        .map(|entry| {
            if entry.restore {
                let src_index = entry.source_partition_index.unwrap_or(u32::MAX);
                let idx_entry = reader.index.iter().find(|e| e.index == src_index);
                let manifest_part = reader
                    .manifest
                    .partitions
                    .iter()
                    .find(|p| p.index == src_index);
                let (type_guid, name) = match (idx_entry, manifest_part) {
                    (Some(idx), Some(m)) => (idx.type_guid, m.name.clone()),
                    (Some(idx), None) => (idx.type_guid, idx.name.clone()),
                    _ => ([0u8; 16], String::new()),
                };
                // Boot-clone fidelity: restore the source's GPT attribute
                // bits (Recovery stays hidden/no-auto-mount) and partition
                // unique GUID (BCD device references survive) when the
                // manifest recorded them. Absent (older backups / MBR
                // sources) → attributes 0 + fresh Windows-generated ID,
                // the pre-fidelity behavior.
                let attributes = manifest_part.and_then(|m| m.gpt_attributes).unwrap_or(0);
                let unique_guid = manifest_part
                    .and_then(|m| m.unique_guid.as_deref())
                    .and_then(phoenix_core::disk::guid_from_string)
                    .unwrap_or([0u8; 16]);
                GptLayoutEntry {
                    offset_bytes: entry.target_offset_bytes,
                    size_bytes: entry.target_size_bytes,
                    type_guid,
                    unique_guid,
                    attributes,
                    name,
                }
            } else {
                // Preserved partition: keep its existing identity so a
                // partial restore doesn't accidentally re-type an
                // untouched Recovery/EFI as Basic Data.
                let live = target_disk
                    .partitions
                    .iter()
                    .find(|p| p.index == entry.target_partition_index);
                match live {
                    Some(p) => GptLayoutEntry {
                        offset_bytes: entry.target_offset_bytes,
                        size_bytes: entry.target_size_bytes,
                        type_guid: p.type_guid,
                        unique_guid: p.unique_guid,
                        attributes: p.gpt_attributes,
                        name: p.name.clone(),
                    },
                    None => {
                        warn!(
                            target_part = entry.target_partition_index,
                            "preserved partition has no matching live entry on target disk; \
                             writing as anonymous Basic Data"
                        );
                        GptLayoutEntry {
                            offset_bytes: entry.target_offset_bytes,
                            size_bytes: entry.target_size_bytes,
                            type_guid: [0u8; 16],
                            unique_guid: [0u8; 16],
                            attributes: 0,
                            name: String::new(),
                        }
                    }
                }
            }
        })
        .collect()
}

/// Build the per-partition descriptors for the MBR writer.
///
/// Full-disk mode writes the restored entries only; partial mode writes
/// EVERY plan entry (the IOCTL is total-rewrite, so preserved partitions
/// must be re-described too — same contract as the GPT arm). A preserved
/// entry's type byte is derived from the live partition's probed
/// filesystem; that loses exotic type bytes like 0x27 (hidden recovery),
/// which come back as plain 0x07 — logged, not fatal.
///
/// Maps source [`FilesystemKind`] to the MBR partition-type byte. This
/// fixes the analogue of the GPT type-GUID bug: previously every MBR
/// partition got stamped 0x07 (NTFS/IFS) regardless of source, so a
/// restored FAT or EFI partition would mount wrong (or not at all).
fn build_mbr_layout_entries(
    plan: &RestorePlan,
    reader: &PhnxReader,
    target_disk: &DiskInfo,
    mode: RestoreMode,
) -> Vec<MbrLayoutEntry> {
    let restoring: Vec<&RestorePlanEntry> = match mode {
        RestoreMode::FullDisk => plan.entries.iter().filter(|e| e.restore).collect(),
        RestoreMode::Partial => plan.entries.iter().collect(),
    };
    // Pick which entry should carry the bootable flag. The backup
    // format doesn't currently record the active-flag, so this is
    // best-effort: prefer the first NTFS partition; if none, prefer
    // the first FAT/exFAT partition; if neither, mark nothing
    // bootable. Capturing the real active-flag at backup time is a
    // separate format change.
    let bootable_idx = restoring
        .iter()
        .enumerate()
        .find(|(_, e)| {
            e.source_partition_index
                .and_then(|src| reader.index.iter().find(|i| i.index == src))
                .map(|i| matches!(i.fs_kind, FilesystemKind::Ntfs))
                .unwrap_or(false)
        })
        .or_else(|| {
            restoring.iter().enumerate().find(|(_, e)| {
                e.source_partition_index
                    .and_then(|src| reader.index.iter().find(|i| i.index == src))
                    .map(|i| matches!(i.fs_kind, FilesystemKind::Fat | FilesystemKind::Exfat))
                    .unwrap_or(false)
            })
        })
        .map(|(idx, _)| idx);

    restoring
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let partition_type = if entry.restore {
                let idx_entry = entry
                    .source_partition_index
                    .and_then(|src| reader.index.iter().find(|e| e.index == src));
                mbr_partition_type_for(idx_entry, entry.target_size_bytes)
            } else {
                // Preserved partition: re-derive its type byte from the live
                // partition's probed filesystem (the enumerator doesn't
                // surface the raw MBR type byte).
                let live_fs = target_disk
                    .partitions
                    .iter()
                    .find(|p| p.index == entry.target_partition_index)
                    .map(|p| p.fs_kind)
                    .unwrap_or(FilesystemKind::Unknown);
                mbr_type_from_fs(live_fs, entry.target_size_bytes)
            };
            MbrLayoutEntry {
                offset_bytes: entry.target_offset_bytes,
                size_bytes: entry.target_size_bytes,
                partition_type,
                bootable: bootable_idx == Some(i),
            }
        })
        .collect()
}

/// [`mbr_partition_type_for`] for a live target partition's probed
/// filesystem — used to re-describe preserved partitions when a partial
/// restore rewrites an MBR table.
fn mbr_type_from_fs(fs: FilesystemKind, size_bytes: u64) -> u8 {
    match fs {
        FilesystemKind::Ntfs
        | FilesystemKind::Exfat
        | FilesystemKind::Bitlocker
        | FilesystemKind::Refs => 0x07,
        FilesystemKind::Fat => {
            if size_bytes < 32 * 1024 * 1024 {
                0x06
            } else {
                0x0C
            }
        }
        FilesystemKind::Efi => 0xEF,
        FilesystemKind::Msr | FilesystemKind::Unknown => {
            warn!(
                fs = ?fs,
                "preserved MBR partition has an ambiguous filesystem; writing type 0x07 (IFS)"
            );
            0x07
        }
    }
}

/// Non-zero MBR disk signature for a reinit whose source image carries none
/// (GPT-imaged sources). Derived from the wall clock — uniqueness matters
/// (Windows dislikes colliding signatures), reproducibility doesn't.
fn fallback_mbr_signature() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let sig = (now.as_secs() as u32) ^ now.subsec_nanos();
    if sig != 0 {
        sig
    } else {
        0x5048_4E58 // "PHNX"
    }
}

/// Lock + dismount the volume of every live target partition that has NO
/// entry in the plan — those partitions are being deleted, and the table
/// rewrite must not race a mounted volume on them. Guards are pushed into
/// the caller's list and released after the table write.
fn lock_removed_target_volumes(
    plan: &RestorePlan,
    disk: &DiskInfo,
    guards: &mut Vec<phoenix_core::disk::LockedVolume>,
) -> Result<()> {
    for p in &disk.partitions {
        if plan
            .entries
            .iter()
            .any(|e| e.target_partition_index == p.index)
        {
            continue;
        }
        let Some(vol) = p.volume_path.clone() else {
            continue;
        };
        let guard = phoenix_core::disk::LockedVolume::acquire(&vol).map_err(|e| {
            phoenix_core::error::PhoenixError::Disk(format!(
                "could not clear the volume {vol} on partition {} before removing it from \
                 the partition table (close any open files/handles on it and retry): {e}",
                p.index
            ))
        })?;
        info!(
            volume = vol.as_str(),
            partition = p.index,
            "dismounted volume of a partition being deleted by the restore plan"
        );
        guards.push(guard);
    }
    Ok(())
}

/// Map source filesystem kind to MBR partition-type byte. The byte
/// values are from the standard MBR partition-type table (see
/// e.g. Microsoft KB or the Wikipedia article):
///
///   * 0x06  FAT16B (≥32 MiB CHS)
///   * 0x07  IFS / NTFS / exFAT
///   * 0x0B  FAT32 CHS
///   * 0x0C  FAT32 LBA — what modern Windows formats default to
///   * 0xEF  EFI System Partition (rare on MBR; legal but unusual)
///
/// BitLocker volumes map to 0x07 (they live in ordinary IFS partitions).
/// Ambiguous source (MSR, Unknown) falls back to 0x07 with a warning —
/// MSR on MBR isn't a real thing, and Unknown means we couldn't
/// classify but the user asked us to restore it anyway.
fn mbr_partition_type_for(idx_entry: Option<&PartitionIndexEntry>, size_bytes: u64) -> u8 {
    let Some(idx) = idx_entry else {
        warn!("MBR restore entry has no source partition; defaulting type to 0x07 (IFS)");
        return 0x07;
    };
    match idx.fs_kind {
        FilesystemKind::Ntfs => 0x07,
        FilesystemKind::Exfat => 0x07,
        // ReFS on MBR is exotic but legal; it lives in an IFS partition.
        FilesystemKind::Refs => 0x07,
        // A BitLocker data volume lives in an ordinary 0x07 (IFS) MBR
        // partition — the FVE header replaces the filesystem boot sector,
        // not the partition type — so a ciphertext round-trip preserves it.
        FilesystemKind::Bitlocker => 0x07,
        FilesystemKind::Fat => {
            // Modern Windows always formats FAT32 with type 0x0C (LBA).
            // The 32 MiB threshold is the classic FAT16-vs-FAT32
            // dividing line — below it FAT16 (0x06), above it FAT32
            // LBA (0x0C). Real-world FAT12 on hard disks is rare and
            // we don't try to distinguish it here.
            if size_bytes < 32 * 1024 * 1024 {
                0x06
            } else {
                0x0C
            }
        }
        FilesystemKind::Efi => 0xEF,
        FilesystemKind::Msr | FilesystemKind::Unknown => {
            warn!(
                fs = ?idx.fs_kind,
                index = idx.index,
                "no clean MBR partition-type mapping for source FS; defaulting to 0x07 (IFS)"
            );
            0x07
        }
    }
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
