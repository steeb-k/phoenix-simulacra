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

use crate::partition_table::{
    init_target_disk_as_gpt, notify_disk_updated, write_partition_layout, GptInitState,
};
use crate::plan::RestorePlan;

pub struct RestoreOptions {
    pub backup_path: std::path::PathBuf,
    pub plan: RestorePlan,
    pub verify_on_restore: bool,
    pub progress: Option<ProgressHandle>,
}

/// Outcome of a successful restore. Distinguishes:
///   * `partitions_restored` — total partitions written.
///   * `partitions_resized` — partitions whose target size differed
///     from the source. These needed boot-sector patching at minimum.
///   * `partitions_metadata_rewritten` — partitions where the NTFS
///     metadata rewriter ran (post-data-write `$Bitmap` truncation,
///     `$LogFile` clean-shutdown stamp, MFT data-run translation, MFT
///     mirror sync). Currently only NTFS shrinks; FAT/exFAT and NTFS
///     grows still need user-side `chkdsk /F` to reconcile metadata.
///
/// The chkdsk hint is suppressed for partitions counted in
/// `partitions_metadata_rewritten` because the rewriter has already
/// done the reconciliation work that chkdsk would otherwise do at next
/// mount. The hint still fires for resized partitions that didn't get
/// our automatic cleanup (the `partitions_resized -
/// partitions_metadata_rewritten` delta).
#[derive(Debug, Clone, Default)]
pub struct RestoreSummary {
    pub partitions_restored: u32,
    pub partitions_resized: u32,
    pub partitions_metadata_rewritten: u32,
}

impl RestoreSummary {
    /// Number of resized partitions whose post-resize metadata still
    /// needs `chkdsk /F` to reconcile (i.e., we did not run our own
    /// cleanup pass over them).
    pub fn partitions_needing_chkdsk(&self) -> u32 {
        self.partitions_resized
            .saturating_sub(self.partitions_metadata_rewritten)
    }
}

pub fn run_restore(opts: RestoreOptions) -> Result<RestoreSummary> {
    let mut reader = PhnxReader::open(&opts.backup_path)?;
    opts.plan
        .validate_against_backup(&reader.manifest)?;
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

    info!(
        "Restoring to disk {} ({}); source style={}, target was {}",
        disk.index,
        disk.path,
        reader.manifest.disk.style,
        if disk.is_gpt { "GPT" } else { "MBR/uninitialized" }
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
    let source_was_gpt = reader.manifest.disk.style.eq_ignore_ascii_case("gpt");
    let gpt_state: Option<GptInitState> = if source_was_gpt {
        if let Some(ref p) = opts.progress {
            p.set_phase("Initializing target disk as GPT");
        }
        let source_disk_guid = parse_disk_guid(reader.manifest.disk.disk_guid.as_deref());
        Some(init_target_disk_as_gpt(
            &disk.path,
            disk.size_bytes,
            source_disk_guid,
        )?)
    } else {
        None
    };

    let restoring: Vec<_> = opts.plan.entries.iter().filter(|e| e.restore).collect();
    // Sum the per-partition `used_bytes` from the manifest so the GUI's
    // progress bar — which renders `current` and `total` through
    // `format_bytes` — shows real-world scale ("1.6 GB / 21 GB") instead
    // of the chunk count it used to ("1.57 KB / 5.29 KB" for a 21 GB
    // partition, courtesy of `format_bytes(5419)`).
    let total_bytes: u64 = restoring
        .iter()
        .filter_map(|entry| {
            reader
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == entry.source_partition_index)
        })
        .map(|p| p.used_bytes)
        .sum();

    if let Some(ref p) = opts.progress {
        p.begin(total_bytes.max(1), "Restore");
    }

    let mut bytes_done = 0u64;
    let mut summary = RestoreSummary::default();

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

        summary.partitions_restored += 1;
        if entry.target_size_bytes != idx_entry.original_size {
            summary.partitions_resized += 1;
        }

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
                partition = entry.source_partition_index,
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
                // The NTFS metadata rewriter ran iff we passed it a
                // map. With `build_relocation_map` now returning
                // Some(empty_map) for shrinks that don't need
                // physical relocation, this branch fires for every
                // NTFS shrink — including the "data already fits"
                // case, which still benefits from `$Bitmap` /
                // `$LogFile` cleanup.
                if relocation.is_some() {
                    summary.partitions_metadata_rewritten += 1;
                }
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
                )?;
            }
        }
    }

    // PHASE 3 — now that every partition's bytes are on disk, plant
    // the partition entry table on top of them. We held off on this
    // step until the data writes were done so partmgr/volmgr couldn't
    // notice the entries and lazy-mount empty volumes that would
    // otherwise bounce raw writes with `ERROR_ACCESS_DENIED`. Then a
    // best-effort `IOCTL_DISK_UPDATE_PROPERTIES` so Disk Management /
    // Explorer pick up the new layout immediately.
    if let Some(state) = gpt_state {
        if let Some(ref p) = opts.progress {
            p.set_phase("Writing partition layout");
        }
        write_partition_layout(&disk.path, &opts.plan, &state)?;
        if let Some(ref p) = opts.progress {
            p.set_phase("Notifying Windows of the new partition layout");
        }
        notify_disk_updated(&disk.path);
    }

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
