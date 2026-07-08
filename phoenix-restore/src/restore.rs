use std::path::Path;

use phoenix_capture::fat::restore_fat;
use phoenix_capture::ntfs::restore_ntfs;
use phoenix_capture::raw::{restore_raw, PartitionWriter};
use phoenix_core::container::{PartitionIndexEntry, PhnxReader};
use phoenix_core::disk::{enumerate_disks, DiskInfo, FilesystemKind};
use phoenix_core::error::Result;
use phoenix_core::manifest::fs_kind_from_string;
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
}

pub fn run_restore(opts: RestoreOptions) -> Result<RestoreSummary> {
    let mut reader = PhnxReader::open(&opts.backup_path)?;
    opts.plan.validate_against_backup(&reader.manifest)?;
    // Pre-flight: refuse a shrink whose source has data physically
    // beyond the new partition's end *before* we touch the target disk.
    // This has to come before `init_target_disk_as_gpt` — a half-
    // initialized GPT plus an aborted restore would leave the user's
    // disk in worse shape than not starting at all.
    opts.plan.validate_extents_fit(&mut reader)?;

    let disks = enumerate_disks()?;
    let disk = disks
        .into_iter()
        .find(|d| d.index == opts.plan.target_disk_index)
        .ok_or_else(|| phoenix_core::error::PhoenixError::Disk("target disk not found".into()))?;

    // Reject an overlapping or oversized layout before touching the disk.
    // This catches a hand-edited plan.toml as well as any auto-layout bug.
    opts.plan.validate_layout(disk.size_bytes)?;

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
    let needs_init = mode == RestoreMode::FullDisk;
    let writes_layout = mode == RestoreMode::FullDisk
        || (mode == RestoreMode::Partial && disk.is_gpt && disk.disk_guid.is_some());

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
    let resize_step = if any_grow {
        steps.push("Resizing partitions".to_string());
        Some(steps.len() - 1)
    } else {
        None
    };
    steps.push("Bringing disk online".to_string());
    let online_step = steps.len() - 1;

    if let Some(ref p) = opts.progress {
        p.set_steps(steps);
        p.set_step(0);
    }

    let gpt_state: Option<GptInitState> = if mode == RestoreMode::FullDisk && source_was_gpt {
        if let Some(ref p) = opts.progress {
            if let Some(step) = init_step {
                p.set_step(step);
            }
        }
        let source_disk_guid = parse_disk_guid(reader.manifest.disk.disk_guid.as_deref());
        Some(init_target_disk_as_gpt(
            &disk.path,
            disk.size_bytes,
            source_disk_guid,
        )?)
    } else {
        if mode == RestoreMode::FullDisk && !source_was_gpt {
            if let Some(ref p) = opts.progress {
                if let Some(step) = init_step {
                    p.set_step(step);
                }
            }
            let sig = (reader.header.disk_signature & 0xFFFF_FFFF) as u32;
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
    // Sequence among restoring entries only, used to index into the
    // per-partition steps declared in the plan above.
    let mut restore_seq = 0usize;

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

        // For NTFS shrink restores we build a relocation map up front
        // and pass it through `restore_ntfs` -> `restore_raw` -> the
        // metadata rewriter. The map's `None` case is identical to the
        // pre-relocation behavior, so non-shrink and non-NTFS paths see
        // no functional change.
        let relocation = match fs {
            FilesystemKind::Ntfs if entry.target_size_bytes < idx_entry.original_size => {
                let stream = reader.read_stream_header(&idx_entry)?;
                let cluster_size = stream.bytes_per_cluster as u64;
                crate::relocation::build_relocation_map(
                    &stream.extents,
                    idx_entry.sector_size as u64,
                    cluster_size,
                    entry.target_size_bytes,
                )?
            }
            _ => None,
        };
        if let Some(ref m) = relocation {
            info!(
                partition = src_index,
                entries = m.entries.len(),
                new_total_clusters = m.new_total_clusters,
                "built NTFS relocation map for shrink"
            );
        }

        match fs {
            FilesystemKind::Ntfs => {
                bytes_done += restore_ntfs(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    entry.target_size_bytes,
                    opts.verify_on_restore,
                    opts.progress.as_ref(),
                    bytes_done,
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
                    opts.verify_on_restore,
                    opts.progress.as_ref(),
                    bytes_done,
                )?;
            }
            _ => {
                bytes_done += restore_raw(
                    &mut reader,
                    &idx_entry,
                    &mut writer,
                    opts.verify_on_restore,
                    opts.progress.as_ref(),
                    bytes_done,
                    None,
                    entry.target_size_bytes,
                )?;
            }
        }
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
    } else if mode == RestoreMode::FullDisk && !source_was_gpt {
        if let Some(ref p) = opts.progress {
            if let Some(step) = layout_step {
                p.set_step(step);
            }
        }
        let mbr_entries = build_mbr_layout_entries(&opts.plan, &reader);
        write_mbr_partition_layout(&disk.path, &mbr_entries)?;
        notify_disk_updated(&disk.path);
    } else if mode == RestoreMode::Partial && disk.is_gpt {
        if let Some(guid_bytes) = disk.disk_guid {
            if let Some(ref p) = opts.progress {
                if let Some(step) = layout_step {
                    p.set_step(step);
                }
            }
            let disk_guid = bytes_to_guid(guid_bytes);
            let gpt_entries = build_gpt_layout_entries(&opts.plan, &reader, &disk, mode);
            update_partition_layout_existing(
                &disk.path,
                &gpt_entries,
                &disk_guid,
                disk.size_bytes,
            )?;
            notify_disk_updated(&disk.path);
        }
    }

    // PHASE 4 — for every NTFS partition we wrote at a larger size
    // than the source, ask NTFS to extend its `$Bitmap` / `$MFT` /
    // `total_sectors` to fill the new slot. We deliberately left the
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
        if !matches!(idx_entry.fs_kind, FilesystemKind::Ntfs) {
            continue;
        }
        if entry.target_size_bytes <= idx_entry.original_size {
            continue;
        }
        if let Some(ref p) = opts.progress {
            if let Some(step) = resize_step {
                p.set_step(step);
            }
            p.set_detail(format!("Extending NTFS volume for partition {}", src_index));
        }
        if let Err(e) = extend_ntfs_volume(
            disk.index,
            entry.target_offset_bytes,
            entry.target_size_bytes,
            idx_entry.sector_size as u64,
        ) {
            warn!(
                partition = src_index,
                error = %e,
                "FSCTL_EXTEND_VOLUME failed; the partition was restored at source size and \
                 can be extended manually via Disk Management"
            );
        }
    }

    // PHASE 5 — clear the "offline" disk attribute so volumes
    // auto-mount with drive letters. Without this the user has to
    // right-click → Online in Disk Management every time. See
    // `bring_disk_online` for the full rationale (SAN policy +
    // duplicate-signature handling). Best-effort: a warn is logged on
    // failure, but the restored data is already valid on disk.
    if let Some(ref p) = opts.progress {
        p.set_step(online_step);
        p.set_detail(String::new());
    }
    bring_disk_online(&disk.path);

    if let Some(ref p) = opts.progress {
        p.end();
    }
    info!(
        partitions_restored = summary.partitions_restored,
        partitions_resized = summary.partitions_resized,
        "Restore complete"
    );
    Ok(summary)
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
                GptLayoutEntry {
                    offset_bytes: entry.target_offset_bytes,
                    size_bytes: entry.target_size_bytes,
                    type_guid,
                    attributes: 0,
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
                            attributes: 0,
                            name: String::new(),
                        }
                    }
                }
            }
        })
        .collect()
}

/// Build the per-partition descriptors for the MBR writer. Only
/// restored entries participate — `update_partition_layout_existing`'s
/// non-restored case is GPT-only, since we don't currently support
/// rewriting an MBR partition table during a partial restore.
///
/// Maps source [`FilesystemKind`] to the MBR partition-type byte. This
/// fixes the analogue of the GPT type-GUID bug: previously every MBR
/// partition got stamped 0x07 (NTFS/IFS) regardless of source, so a
/// restored FAT or EFI partition would mount wrong (or not at all).
fn build_mbr_layout_entries(plan: &RestorePlan, reader: &PhnxReader) -> Vec<MbrLayoutEntry> {
    let restoring: Vec<&RestorePlanEntry> = plan.entries.iter().filter(|e| e.restore).collect();
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
            let idx_entry = entry
                .source_partition_index
                .and_then(|src| reader.index.iter().find(|e| e.index == src));
            let partition_type = mbr_partition_type_for(idx_entry, entry.target_size_bytes);
            MbrLayoutEntry {
                offset_bytes: entry.target_offset_bytes,
                size_bytes: entry.target_size_bytes,
                partition_type,
                bootable: bootable_idx == Some(i),
            }
        })
        .collect()
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
