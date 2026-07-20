//! Deciding whether an NTFS shrink will work — before writing anything.
//!
//! [`crate::ntfs_meta`] rewrites the target's MFT *after* the data has
//! been streamed. If a record's run lists don't fit there, the failure
//! arrives at the worst possible moment: hours of copying done, the
//! target holding relocated data its metadata no longer describes.
//!
//! This module moves that verdict to plan time. It reads the **source**
//! volume's MFT — out of the backup for a restore, straight off the disk
//! for a clone — and asks, for every record, what the candidate
//! relocation would do to it. The question is answered by the very same
//! [`plan_record_fit`] the rewriter uses, so "the pre-flight says it
//! fits" and "the rewriter succeeds" cannot come apart.
//!
//! The answer is not just yes or no. Because the relocation planner
//! chooses where the moved clusters land, a record that would overflow
//! can usually be *fixed* by giving its clusters a contiguous
//! destination — see `phoenix_restore::relocation::plan_shrink`, which
//! drives the loop this module supplies the verdicts for.

use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::progress::ProgressHandle;
use phoenix_core::relocation::RelocationMap;

use crate::ntfs_meta::{
    apply_fixups_for_read, parse_ntfs_boot, parse_record_model, plan_record_fit,
    resident_file_name, DataRun, NtfsBoot, RecordModel,
};

/// How often to publish progress and check for cancellation while
/// scanning. A record is ~1 KiB, so this is roughly every 4 MiB.
const PROGRESS_STRIDE: u64 = 4096;

/// Read access to a volume's bytes, partition-relative.
///
/// Deliberately narrower than [`crate::reader::BlockSource`]: the
/// pre-flight only ever reads, and it needs exact reads rather than the
/// short-read-tolerant contract. The blanket impl below means any
/// existing block source — a live volume, a VSS shadow, an in-memory
/// test image — satisfies it for free, while the restore side can wrap a
/// `.phnx` reader without pretending to be a disk.
pub trait VolumeRead {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

impl<T: crate::reader::BlockSource> VolumeRead for T {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self.read_at(offset + done as u64, &mut buf[done..])?;
            if n == 0 {
                return Err(PhoenixError::Other(format!(
                    "unexpected end of volume reading {} bytes at offset {offset} (got {done})",
                    buf.len()
                )));
            }
            done += n;
        }
        Ok(())
    }
}

/// One MFT record that a relocation could disturb.
#[derive(Debug, Clone)]
pub struct SnapshotRecord {
    /// Index of the record in the MFT, for error messages and for
    /// re-reading the record to name its file.
    pub index: u64,
    pub model: RecordModel,
}

/// The source MFT, parsed once and reused across every re-planning pass
/// and every step of the minimum-size search.
///
/// Only records holding non-resident attributes are kept — a resident
/// attribute has no run list, so relocation cannot change its size, and
/// on a real volume the great majority of records are resident-only.
#[derive(Debug, Clone)]
pub struct MftSnapshot {
    pub cluster_size: u64,
    pub bytes_per_record: u64,
    pub mft_lcn: u64,
    /// Records examined, including the resident-only ones not retained.
    pub records_scanned: u64,
    pub records: Vec<SnapshotRecord>,
}

impl MftSnapshot {
    /// Source cluster ranges the snapshot knows about, deduplicated and
    /// sorted. Used by the re-planner to reason about a record's data.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// A record whose rewrite would not fit under a candidate map.
#[derive(Debug, Clone)]
pub struct RecordOverflowReport {
    pub record_index: u64,
    pub needed_bytes: usize,
    pub available: usize,
    /// The record's above-boundary source cluster ranges — exactly the
    /// clusters the re-planner has to place contiguously to fix it.
    /// Sorted and merged.
    pub above_ranges: Vec<(u64, u64)>,
}

impl RecordOverflowReport {
    pub fn above_cluster_count(&self) -> u64 {
        self.above_ranges.iter().map(|(_, c)| *c).sum()
    }
}

/// Read and parse the source volume's MFT.
///
/// LCNs here are **source** LCNs: this reads the volume as it is now,
/// before any relocation, which is what makes it usable at plan time
/// against a backup that has not been restored yet.
pub fn scan_mft(
    src: &mut dyn VolumeRead,
    progress: Option<&ProgressHandle>,
) -> Result<MftSnapshot> {
    let mut sector = vec![0u8; 512];
    src.read_exact_at(0, &mut sector)?;
    let boot: NtfsBoot = parse_ntfs_boot(&sector)?;

    // Record 0 describes the MFT itself; its $DATA run list is the map of
    // where every other record lives.
    let mut record0 = vec![0u8; boot.bytes_per_record as usize];
    src.read_exact_at(boot.mft_lcn * boot.cluster_size, &mut record0)?;
    apply_fixups_for_read(&mut record0)?;
    let mft_runs = unnamed_data_runs(&record0)?;

    let mut extents: Vec<(u64, u64)> = Vec::new();
    for run in &mft_runs {
        let lcn = run.lcn.ok_or_else(|| {
            PhoenixError::Other(
                "$MFT has a sparse run; $MFT is never sparse on a real volume - refusing to \
                 pre-flight this shrink"
                    .into(),
            )
        })?;
        extents.push((lcn * boot.cluster_size, run.length * boot.cluster_size));
    }
    let total_bytes: u64 = extents.iter().map(|(_, l)| *l).sum();
    let total_records = total_bytes / boot.bytes_per_record;

    let mut records = Vec::new();
    let mut buf = vec![0u8; boot.bytes_per_record as usize];
    let mut scanned = 0u64;
    for index in 0..total_records {
        if index.is_multiple_of(PROGRESS_STRIDE) {
            if let Some(p) = progress {
                if p.is_cancelled() {
                    return Err(PhoenixError::Cancelled);
                }
                p.set_detail(format!(
                    "Shrink pre-flight: scanning MFT record {index} of {total_records}"
                ));
            }
        }
        let Some(offset) = record_offset(&extents, index, boot.bytes_per_record) else {
            break;
        };
        src.read_exact_at(offset, &mut buf)?;
        if &buf[0..4] != b"FILE" {
            continue; // unused record slot
        }
        scanned += 1;
        apply_fixups_for_read(&mut buf)?;
        let model = parse_record_model(&buf)?;
        // Resident-only records can't be disturbed by a relocation; not
        // keeping them is what makes the snapshot fit in memory on a
        // volume with millions of files.
        if model.attrs.is_empty() {
            continue;
        }
        records.push(SnapshotRecord { index, model });
    }

    tracing::info!(
        records_scanned = scanned,
        records_retained = records.len(),
        total_slots = total_records,
        "shrink pre-flight scanned the source MFT"
    );

    Ok(MftSnapshot {
        cluster_size: boot.cluster_size,
        bytes_per_record: boot.bytes_per_record,
        mft_lcn: boot.mft_lcn,
        records_scanned: scanned,
        records,
    })
}

/// Apply `map` to every record in the snapshot and report the ones whose
/// rewrite would not fit.
///
/// Pure, and cheap enough to run repeatedly: the re-planning loop and the
/// minimum-viable-size search both call it many times over one snapshot
/// without touching the volume again.
pub fn simulate_map(snapshot: &MftSnapshot, map: &RelocationMap) -> Vec<RecordOverflowReport> {
    let mut out = Vec::new();
    for rec in &snapshot.records {
        let Err(overflow) = plan_record_fit(&rec.model, map) else {
            continue;
        };
        out.push(RecordOverflowReport {
            record_index: rec.index,
            needed_bytes: overflow.needed_bytes,
            available: overflow.available,
            above_ranges: above_boundary_ranges(&rec.model, map),
        });
    }
    out
}

/// Every source cluster of `model` that lives past the shrink boundary,
/// as merged, sorted ranges.
fn above_boundary_ranges(model: &RecordModel, map: &RelocationMap) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for attr in &model.attrs {
        for run in &attr.runs {
            let Some(lcn) = run.lcn else { continue };
            let end = lcn.saturating_add(run.length);
            let start = lcn.max(map.safe_max_cluster + 1);
            if end > start {
                ranges.push((start, end - start));
            }
        }
    }
    ranges.sort_by_key(|&(s, _)| s);
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, count) in ranges {
        if let Some(last) = merged.last_mut() {
            let last_end = last.0 + last.1;
            if start <= last_end {
                last.1 = last_end.max(start + count) - last.0;
                continue;
            }
        }
        merged.push((start, count));
    }
    merged
}

/// Best-effort file name for a record, for error messages. Re-reads the
/// record because the snapshot deliberately keeps no resident data.
/// Never fails the caller — an unnamed record just gets no name.
pub fn record_name(
    src: &mut dyn VolumeRead,
    snapshot: &MftSnapshot,
    mft_extents: &[(u64, u64)],
    index: u64,
) -> Option<String> {
    let offset = record_offset(mft_extents, index, snapshot.bytes_per_record)?;
    let mut buf = vec![0u8; snapshot.bytes_per_record as usize];
    src.read_exact_at(offset, &mut buf).ok()?;
    if &buf[0..4] != b"FILE" {
        return None;
    }
    apply_fixups_for_read(&mut buf).ok()?;
    resident_file_name(&buf)
}

/// Byte offset of MFT record `index`, walking the MFT's own extents.
fn record_offset(extents: &[(u64, u64)], index: u64, bytes_per_record: u64) -> Option<u64> {
    let mut covered = 0u64;
    for (start, len) in extents {
        let here = len / bytes_per_record;
        if index < covered + here {
            return Some(start + (index - covered) * bytes_per_record);
        }
        covered += here;
    }
    None
}

/// The unnamed `$DATA` run list of a (fixed-up) record.
fn unnamed_data_runs(record: &[u8]) -> Result<Vec<DataRun>> {
    let model = parse_record_model(record)?;
    // Record 0's first non-resident attribute is its unnamed $DATA on
    // every volume Windows produces; `parse_record_model` keeps them in
    // record order, and $DATA (0x80) sorts after $STANDARD_INFORMATION
    // and $FILE_NAME, which are resident.
    model.attrs.first().map(|a| a.runs.clone()).ok_or_else(|| {
        PhoenixError::Other(
            "$MFT record 0 has no non-resident attribute; cannot locate the MFT".into(),
        )
    })
}

/// Translate the snapshot's MFT location into source-space byte extents,
/// so callers can re-read individual records (for names).
pub fn source_mft_extents(
    src: &mut dyn VolumeRead,
    snapshot: &MftSnapshot,
) -> Result<Vec<(u64, u64)>> {
    let mut record0 = vec![0u8; snapshot.bytes_per_record as usize];
    src.read_exact_at(snapshot.mft_lcn * snapshot.cluster_size, &mut record0)?;
    apply_fixups_for_read(&mut record0)?;
    let runs = unnamed_data_runs(&record0)?;
    Ok(runs
        .iter()
        .filter_map(|r| {
            r.lcn.map(|lcn| {
                (
                    lcn * snapshot.cluster_size,
                    r.length * snapshot.cluster_size,
                )
            })
        })
        .collect())
}

/// Does this map relocate anything a record could notice?
///
/// An empty map is an identity translation, so no run list can change
/// and the whole scan can be skipped — which keeps mild shrinks (nothing
/// past the boundary) as fast as they were before pre-flight existed.
pub fn map_can_disturb_records(map: &RelocationMap) -> bool {
    !map.entries.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::relocation::RelocationEntry;

    /// A minimal but structurally real NTFS volume image: boot sector,
    /// an MFT whose record 0 describes itself, and `extra` file records
    /// whose $DATA runs point wherever the caller says.
    struct VolumeBuilder {
        cluster_size: u64,
        mft_lcn: u64,
        mft_clusters: u64,
        records: Vec<Vec<DataRun>>,
    }

    const REC: usize = 1024;

    impl VolumeBuilder {
        fn new() -> Self {
            Self {
                cluster_size: 4096,
                mft_lcn: 4,
                mft_clusters: 2,
                records: Vec::new(),
            }
        }

        fn with_file(mut self, runs: Vec<DataRun>) -> Self {
            self.records.push(runs);
            self
        }

        fn build(&self) -> Vec<u8> {
            let total_clusters = 512u64;
            let mut image = vec![0u8; (total_clusters * self.cluster_size) as usize];

            // Boot sector.
            image[3..7].copy_from_slice(b"NTFS");
            image[11..13].copy_from_slice(&512u16.to_le_bytes());
            image[13] = (self.cluster_size / 512) as u8;
            image[48..56].copy_from_slice(&self.mft_lcn.to_le_bytes());
            image[56..64].copy_from_slice(&(self.mft_lcn + 64).to_le_bytes());
            image[64] = 0xF6; // -10 => 2^10 = 1024-byte records

            let mft_byte = (self.mft_lcn * self.cluster_size) as usize;
            // Record 0: $DATA describing the MFT's own clusters.
            let rec0 = file_record(&[DataRun {
                length: self.mft_clusters,
                lcn: Some(self.mft_lcn),
            }]);
            image[mft_byte..mft_byte + REC].copy_from_slice(&rec0);
            for (i, runs) in self.records.iter().enumerate() {
                let at = mft_byte + (i + 1) * REC;
                image[at..at + REC].copy_from_slice(&file_record(runs));
            }
            image
        }
    }

    /// A FILE record carrying one non-resident $DATA with `runs`, with
    /// valid USA fixups applied so the scanner accepts it.
    fn file_record(runs: &[DataRun]) -> Vec<u8> {
        use crate::ntfs_meta::encode_run_list;
        let encoded = encode_run_list(runs);
        let attr_len = (64 + encoded.len()).div_ceil(8) * 8;
        let mut rec = vec![0u8; REC];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&48u16.to_le_bytes()); // USA offset
        rec[6..8].copy_from_slice(&3u16.to_le_bytes()); // USN + 2 strides
        rec[20..22].copy_from_slice(&56u16.to_le_bytes()); // first attribute
        rec[28..32].copy_from_slice(&(REC as u32).to_le_bytes());

        let a = 56usize;
        rec[a..a + 4].copy_from_slice(&0x80u32.to_le_bytes()); // $DATA
        rec[a + 4..a + 8].copy_from_slice(&(attr_len as u32).to_le_bytes());
        rec[a + 8] = 1; // non-resident
        rec[a + 10..a + 12].copy_from_slice(&64u16.to_le_bytes());
        rec[a + 32..a + 34].copy_from_slice(&64u16.to_le_bytes()); // run list at +64
        rec[a + 64..a + 64 + encoded.len()].copy_from_slice(&encoded);

        let end = a + attr_len;
        rec[end..end + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        rec[24..28].copy_from_slice(&((end + 4) as u32).to_le_bytes());

        // Stamp the USA: USN 1, saving the two stride-end words.
        let usn: u16 = 1;
        rec[48..50].copy_from_slice(&usn.to_le_bytes());
        for i in 1..3usize {
            let stride_end = i * 512;
            let fixup = 48 + i * 2;
            rec[fixup] = rec[stride_end - 2];
            rec[fixup + 1] = rec[stride_end - 1];
            rec[stride_end - 2..stride_end].copy_from_slice(&usn.to_le_bytes());
        }
        rec
    }

    /// In-memory volume, satisfying `VolumeRead` directly.
    struct Image(Vec<u8>);
    impl VolumeRead for Image {
        fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let start = offset as usize;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(PhoenixError::Other("read past image".into()));
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    fn run(length: u64, lcn: u64) -> DataRun {
        DataRun {
            length,
            lcn: Some(lcn),
        }
    }

    #[test]
    fn scan_finds_every_record_with_a_run_list() {
        let vol = VolumeBuilder::new()
            .with_file(vec![run(4, 100)])
            .with_file(vec![run(8, 200), run(4, 300)])
            .build();
        let mut img = Image(vol);
        let snap = scan_mft(&mut img, None).unwrap();
        assert_eq!(snap.cluster_size, 4096);
        assert_eq!(snap.bytes_per_record, 1024);
        // Record 0 ($MFT) plus the two files: all three carry run lists.
        assert_eq!(snap.records.len(), 3);
        assert_eq!(snap.records[0].index, 0);
        assert_eq!(snap.records[2].model.attrs[0].runs.len(), 2);
    }

    #[test]
    fn record_zero_is_simulated_like_any_other() {
        // $MFT's own $DATA gets relocated too; a shrink that overflows it
        // must be caught, not skipped because the record is special.
        let vol = VolumeBuilder::new().build();
        let mut img = Image(vol);
        let snap = scan_mft(&mut img, None).unwrap();
        assert_eq!(snap.records[0].index, 0);
        assert_eq!(snap.records[0].model.attrs[0].runs, vec![run(2, 4)]);
    }

    #[test]
    fn simulate_flags_the_record_that_would_overflow() {
        // The real-world shape: a badly fragmented file whose run list
        // already fills most of its record. Relocating it cluster by
        // cluster shatters each run into four, and the re-encoded list no
        // longer fits even with every spare byte in the record.
        let fragmented: Vec<DataRun> = (0..80).map(|i| run(4, 2000 + i * 8)).collect();
        let vol = VolumeBuilder::new()
            .with_file(vec![run(4, 10)]) // below the boundary, untouched
            .with_file(fragmented)
            .build();
        let mut img = Image(vol);
        let snap = scan_mft(&mut img, None).unwrap();

        // Scatter every above-boundary cluster to its own destination, two
        // apart, so no two land adjacent and each needs its own run.
        let entries: Vec<RelocationEntry> = (0..80u64)
            .flat_map(|i| {
                (0..4u64).map(move |j| RelocationEntry {
                    src_cluster_start: 2000 + i * 8 + j,
                    cluster_count: 1,
                    dst_cluster_start: (i * 4 + j) * 2,
                })
            })
            .collect();
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 1999,
            new_total_clusters: 2000,
            entries,
        };
        let overflows = simulate_map(&snap, &map);
        assert_eq!(overflows.len(), 1, "{overflows:?}");
        assert_eq!(overflows[0].record_index, 2);
        assert!(overflows[0].needed_bytes > overflows[0].available);
        assert_eq!(overflows[0].above_cluster_count(), 320);
    }

    #[test]
    fn an_empty_map_disturbs_nothing() {
        let vol = VolumeBuilder::new().with_file(vec![run(8, 200)]).build();
        let mut img = Image(vol);
        let snap = scan_mft(&mut img, None).unwrap();
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 300,
            new_total_clusters: 301,
            entries: Vec::new(),
        };
        assert!(!map_can_disturb_records(&map));
        assert!(simulate_map(&snap, &map).is_empty());
    }

    #[test]
    fn above_boundary_ranges_are_merged_and_clipped_to_the_boundary() {
        let vol = VolumeBuilder::new()
            // Straddles the boundary at 149/150, plus an adjacent run.
            .with_file(vec![run(10, 145), run(5, 155)])
            .build();
        let mut img = Image(vol);
        let snap = scan_mft(&mut img, None).unwrap();
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 149,
            new_total_clusters: 150,
            entries: vec![RelocationEntry {
                src_cluster_start: 150,
                cluster_count: 10,
                dst_cluster_start: 0,
            }],
        };
        let file = snap.records.iter().find(|r| r.index == 1).unwrap();
        let ranges = above_boundary_ranges(&file.model, &map);
        // 145..155 clipped to 150..155, then merged with 155..160.
        assert_eq!(ranges, vec![(150, 10)]);
    }
}
