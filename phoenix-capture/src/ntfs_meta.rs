//! NTFS metadata rewriting for shrink relocation.
//!
//! After Phase B2 has streamed file data to its relocated locations,
//! the on-disk filesystem still THINKS those bytes live at their old
//! cluster numbers - every MFT data run, the boot sector's mft_lcn,
//! the MFT mirror's location, $Bitmap's allocation bits, and
//! $LogFile's pending-transaction list all reference the source
//! layout. Mounting at this point produces a corrupt volume.
//!
//! This module fixes that. After the data writes finish in
//! `restore_raw`, we walk the MFT under the relocation map and
//! rewrite every reference. Subset of `ntfsresize`'s shrink logic;
//! enough to make the volume mountable and consistent for `chkdsk
//! /F` to pick up.
//!
//! Scope (v1):
//!   * Boot sector: re-read source's mft_lcn / mft_mirror_lcn,
//!     translate through the map, write back.
//!   * MFT walk: every record's non-resident attribute run lists
//!     are parsed, LCN-translated, and re-encoded.
//!   * $Bitmap: clear bits past the new boundary; set bits at the
//!     relocation destinations; truncate to new_total_clusters / 8.
//!   * $LogFile: stamp 0xFF over its $DATA so NTFS treats it as
//!     "no pending transactions" at next mount.
//!   * MFT mirror: copy the (rewritten) first 4 MFT records to the
//!     mirror's relocated location.
//!
//! Relocating a run list can make it *longer* — a run split across two
//! destination ranges costs a few more bytes, and a bigger LCN delta
//! needs wider fields. Windows packs each attribute to its run list
//! rounded up to 8 bytes, so an attribute typically has 0-7 spare bytes
//! inside it: nowhere near enough. We therefore grow the attribute into
//! the *record's* free space (a 1024-byte record is usually only part
//! full), shifting the attributes after it along. See
//! [`plan_record_fit`].
//!
//! Out of scope (refused with an error rather than silently
//! corrupting): a rewrite that outgrows the whole MFT record. NTFS would
//! migrate attributes to $ATTRIBUTE_LIST; we don't implement that yet.
//! The shrink pre-flight in [`crate::ntfs_preflight`] is expected to
//! prevent this from ever reaching the write path — it simulates this
//! module's rewrite (through the very same [`plan_record_fit`]) at plan
//! time and re-plans the relocation to keep every record inside its
//! budget. The error below is a backstop, not the intended failure mode.

use crate::raw::PartitionWriter;
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::relocation::RelocationMap;

const MFT_RECORD_LOGFILE: u64 = 2;
const MFT_RECORD_BITMAP: u64 = 6;

const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Record-header field offsets, named so the byte surgery below reads as
/// something other than magic numbers.
const REC_ATTRS_OFFSET: usize = 20; // u16: first attribute
const REC_USED_SIZE: usize = 24; // u32: bytes in use, incl. the END marker

/// Attribute headers are 8-byte aligned, and so is every length NTFS
/// writes into one.
fn align8(v: usize) -> usize {
    v.div_ceil(8) * 8
}

// Bounds-checked little-endian reads over untrusted on-disk bytes. Every
// MFT record and run list we parse here comes from the source volume (or a
// possibly-corrupt backup of it), so a malformed structure must surface as
// a recoverable `Err`, never a panic that aborts the whole restore.
#[inline]
fn le_u16(buf: &[u8], off: usize) -> Result<u16> {
    buf.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| oob(2, off, buf.len()))
}

#[inline]
fn le_u32(buf: &[u8], off: usize) -> Result<u32> {
    buf.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| oob(4, off, buf.len()))
}

fn oob(width: usize, off: usize, len: usize) -> PhoenixError {
    PhoenixError::InvalidFormat(format!(
        "NTFS metadata: {width}-byte read at offset {off} exceeds buffer length {len}"
    ))
}

/// Parsed view of the NTFS boot sector. Field semantics match the
/// canonical layout (offset 11 BytesPerSector, offset 13
/// SectorsPerCluster, offset 48 MftLcn, offset 56 MftMirrLcn, offset
/// 64 ClustersPerFileRecordSegment).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NtfsBoot {
    // No `bytes_per_sector` field on purpose. It is read from the boot sector to
    // derive `cluster_size`, but it is NOT the MFT fixup stride — that comes from
    // each record's own USA count (see `fixup_stride`). Keeping it here is what
    // let the fixup code reach for the wrong number and break on 4Kn.
    pub(crate) cluster_size: u64,
    pub(crate) mft_lcn: u64,
    pub(crate) mft_mirror_lcn: u64,
    pub(crate) bytes_per_record: u64,
}

fn read_ntfs_boot(writer: &mut PartitionWriter) -> Result<NtfsBoot> {
    let mut sector = vec![0u8; 512];
    writer.read_at(0, &mut sector)?;
    parse_ntfs_boot(&sector)
}

/// Parse an NTFS boot sector from raw bytes. Split out from
/// [`read_ntfs_boot`] so the shrink pre-flight — which reads the *source*
/// volume through its own abstraction rather than a `PartitionWriter` —
/// derives its geometry from exactly the same code the rewriter uses.
pub(crate) fn parse_ntfs_boot(sector: &[u8]) -> Result<NtfsBoot> {
    if sector.len() < 72 {
        return Err(PhoenixError::Other(format!(
            "NTFS boot sector buffer is {} bytes; need at least 72",
            sector.len()
        )));
    }
    if &sector[3..7] != b"NTFS" {
        return Err(PhoenixError::Other(
            "boot sector is not NTFS (expected 'NTFS' at offset 3); cannot rewrite metadata".into(),
        ));
    }
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]) as u64; // cluster math only
    let sectors_per_cluster = sector[13] as u64;
    if bytes_per_sector == 0 || sectors_per_cluster == 0 {
        return Err(PhoenixError::Other(
            "NTFS boot sector reports zero bytes_per_sector or sectors_per_cluster".into(),
        ));
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let mft_lcn = u64::from_le_bytes(sector[48..56].try_into().unwrap());
    let mft_mirror_lcn = u64::from_le_bytes(sector[56..64].try_into().unwrap());
    let raw_cprs = sector[64] as i8;
    let bytes_per_record = if raw_cprs > 0 {
        (raw_cprs as u64) * cluster_size
    } else {
        let shift = (-(raw_cprs as i32)) as u32;
        if shift > 31 {
            return Err(PhoenixError::Other(format!(
                "NTFS boot sector reports implausible ClustersPerFileRecordSegment {raw_cprs}"
            )));
        }
        1u64 << shift
    };
    if !(bytes_per_record == 1024 || bytes_per_record == 2048 || bytes_per_record == 4096) {
        tracing::warn!(
            bytes_per_record,
            "unusual NTFS bytes_per_record; proceeding but this is rare"
        );
    }
    Ok(NtfsBoot {
        cluster_size,
        mft_lcn,
        mft_mirror_lcn,
        bytes_per_record,
    })
}

/// The byte stride between NTFS Update Sequence Array fixups, derived from the
/// record itself.
///
/// This used to be the volume's `bytes_per_sector`, which is right up until it
/// isn't: Windows formats a **4Kn** volume with `bytes_per_sector = 4096` but
/// still uses **1024-byte MFT records**, so a "sector" is four times bigger than
/// the whole record and the first fixup boundary lands past its end. That is
/// exactly the error a 4Kn restore hit: "MFT record sector boundary 4096 past
/// record length 1024".
///
/// The record already carries the answer. `usa_size_words` is one USN plus one
/// entry per protected stride, so `record_len / (usa_size_words - 1)` is the
/// stride NTFS actually used — 512 on both 512e and 4Kn volumes, which is what
/// the doc comment above always claimed. Deriving it means we cannot disagree
/// with the record we are about to rewrite.
pub(crate) fn fixup_stride(record_len: usize, usa_size_words: usize) -> Result<usize> {
    let strides = usa_size_words - 1; // callers guarantee usa_size_words >= 2
    if strides == 0 || !record_len.is_multiple_of(strides) {
        return Err(PhoenixError::Other(format!(
            "MFT record length {record_len} is not divisible into {strides} fixup strides \
             (USA size {usa_size_words}); record is malformed"
        )));
    }
    let stride = record_len / strides;
    if stride < 2 {
        return Err(PhoenixError::Other(format!(
            "MFT fixup stride {stride} is too small to hold a USN"
        )));
    }
    Ok(stride)
}

/// Apply NTFS Update Sequence Array (USA) fixups to a record buffer
/// read from disk. NTFS overwrites the last 2 bytes of each
/// 512-byte sector with the record's USN as a corruption-detection
/// hack; the original 2-byte values live in the USA. This puts them
/// back so the record is the in-memory view that the spec describes.
pub(crate) fn apply_fixups_for_read(record: &mut [u8]) -> Result<()> {
    if record.len() < 8 || &record[0..4] != b"FILE" {
        return Err(PhoenixError::Other(
            "MFT record missing 'FILE' magic; record may be unallocated or fixups already broken"
                .into(),
        ));
    }
    let usa_offset = u16::from_le_bytes([record[4], record[5]]) as usize;
    let usa_size_words = u16::from_le_bytes([record[6], record[7]]) as usize;
    if usa_size_words < 2 || usa_offset + usa_size_words * 2 > record.len() {
        return Err(PhoenixError::Other(
            "MFT record has invalid USA offset/size".into(),
        ));
    }
    let usn = u16::from_le_bytes([record[usa_offset], record[usa_offset + 1]]);
    let bps = fixup_stride(record.len(), usa_size_words)?;
    for i in 1..usa_size_words {
        let sector_end = i * bps;
        if sector_end > record.len() {
            return Err(PhoenixError::Other(format!(
                "MFT record sector boundary {sector_end} past record length {}",
                record.len()
            )));
        }
        let stored_usn_lo = record[sector_end - 2];
        let stored_usn_hi = record[sector_end - 1];
        if stored_usn_lo != (usn & 0xFF) as u8 || stored_usn_hi != (usn >> 8) as u8 {
            return Err(PhoenixError::Other(format!(
                "MFT record fixup mismatch at sector {i}: expected USN {:04X}, got {:02X}{:02X} \
                 (record may be torn or corrupt)",
                usn, stored_usn_hi, stored_usn_lo,
            )));
        }
        let fixup_offset = usa_offset + i * 2;
        record[sector_end - 2] = record[fixup_offset];
        record[sector_end - 1] = record[fixup_offset + 1];
    }
    Ok(())
}

fn apply_fixups_for_write(record: &mut [u8]) -> Result<()> {
    if record.len() < 8 {
        return Err(PhoenixError::InvalidFormat(
            "MFT record too short for a USA header".into(),
        ));
    }
    let usa_offset = le_u16(record, 4)? as usize;
    let usa_size_words = le_u16(record, 6)? as usize;
    if usa_size_words < 2 || usa_offset + usa_size_words * 2 > record.len() {
        return Err(PhoenixError::InvalidFormat(
            "MFT record has invalid USA offset/size".into(),
        ));
    }
    let usn = le_u16(record, usa_offset)?.wrapping_add(1);
    record[usa_offset] = (usn & 0xFF) as u8;
    record[usa_offset + 1] = (usn >> 8) as u8;
    // Same derivation as the read path: the record's own USA count, not the
    // volume's sector size (which is 4096 on 4Kn while records stay 1024).
    let bps = fixup_stride(record.len(), usa_size_words)
        .map_err(|e| PhoenixError::InvalidFormat(e.to_string()))?;
    for i in 1..usa_size_words {
        let sector_end = i * bps;
        if sector_end < 2 || sector_end > record.len() {
            return Err(PhoenixError::InvalidFormat(format!(
                "MFT record sector boundary {sector_end} outside record length {}",
                record.len()
            )));
        }
        let fixup_offset = usa_offset + i * 2;
        record[fixup_offset] = record[sector_end - 2];
        record[fixup_offset + 1] = record[sector_end - 1];
        record[sector_end - 2] = (usn & 0xFF) as u8;
        record[sector_end - 1] = (usn >> 8) as u8;
    }
    Ok(())
}

/// One run of contiguous clusters in a non-resident attribute's
/// run list. A `lcn` of `None` indicates a sparse run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    pub length: u64,
    pub lcn: Option<u64>,
}

/// Parse a run list starting at `bytes[0]` until the 0x00 terminator.
pub(crate) fn parse_run_list(bytes: &[u8]) -> Result<(Vec<DataRun>, usize)> {
    let mut runs = Vec::new();
    let mut prev_lcn: i64 = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        let header = bytes[pos];
        pos += 1;
        if header == 0 {
            return Ok((runs, pos));
        }
        let length_bytes = (header & 0x0F) as usize;
        let lcn_bytes = ((header >> 4) & 0x0F) as usize;
        // The header nibbles can encode up to 15 bytes, but a run's length
        // and LCN delta each fit in 8 bytes; a value above 8 is corruption
        // and, left unchecked, would shift a u64/i64 past its width and
        // panic. Reject it as invalid input instead.
        if length_bytes == 0
            || length_bytes > 8
            || lcn_bytes > 8
            || pos + length_bytes + lcn_bytes > bytes.len()
        {
            return Err(PhoenixError::InvalidFormat(format!(
                "invalid run-list entry: header={header:02X}, pos={pos}, available={}",
                bytes.len()
            )));
        }
        let mut length: u64 = 0;
        for i in 0..length_bytes {
            length |= (bytes[pos + i] as u64) << (i * 8);
        }
        pos += length_bytes;
        let lcn = if lcn_bytes == 0 {
            None
        } else {
            let mut delta: i64 = 0;
            for i in 0..lcn_bytes {
                delta |= (bytes[pos + i] as i64) << (i * 8);
            }
            // Sign-extend the little-endian delta. At the full 8-byte width
            // the i64 is already complete, and `-1i64 << 64` would overflow
            // the shift, so only extend for widths under 8.
            if lcn_bytes < 8 {
                let sign_bit = 1i64 << (lcn_bytes * 8 - 1);
                if delta & sign_bit != 0 {
                    delta |= -1i64 << (lcn_bytes * 8);
                }
            }
            pos += lcn_bytes;
            prev_lcn = prev_lcn.wrapping_add(delta);
            Some(prev_lcn as u64)
        };
        runs.push(DataRun { length, lcn });
    }
    Err(PhoenixError::Other(
        "run list missing 0x00 terminator".into(),
    ))
}

/// Re-encode a list of (length, lcn) pairs into NTFS's compact run
/// list format. LCN deltas are computed against the previous run's
/// LCN; sparse runs (lcn=None) emit no LCN bytes and don't shift
/// `prev_lcn`.
pub(crate) fn encode_run_list(runs: &[DataRun]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev_lcn: i64 = 0;
    for run in runs {
        let length_bytes = bytes_for_unsigned(run.length);
        let (lcn_bytes, lcn_delta) = match run.lcn {
            None => (0u8, 0i64),
            Some(lcn) => {
                let delta = (lcn as i64).wrapping_sub(prev_lcn);
                let n = bytes_for_signed(delta);
                (n, delta)
            }
        };
        let header = (lcn_bytes << 4) | length_bytes;
        out.push(header);
        for i in 0..length_bytes {
            out.push(((run.length >> (i * 8)) & 0xFF) as u8);
        }
        for i in 0..lcn_bytes {
            out.push(((lcn_delta >> (i * 8)) & 0xFF) as u8);
        }
        if let Some(lcn) = run.lcn {
            prev_lcn = lcn as i64;
        }
    }
    out.push(0);
    out
}

fn bytes_for_unsigned(v: u64) -> u8 {
    let mut n = 1u8;
    for i in 1..=8u8 {
        if v >> (i * 8) == 0 {
            n = i;
            break;
        }
    }
    n
}

fn bytes_for_signed(v: i64) -> u8 {
    if v == 0 {
        return 1;
    }
    for n in 1..=8u8 {
        let bits = (n as u32) * 8;
        let max_pos: i64 = (1i64 << (bits - 1)) - 1;
        let min_neg: i64 = -(1i64 << (bits - 1));
        if v >= min_neg && v <= max_pos {
            return n;
        }
    }
    8
}

/// Apply the relocation map to every LCN in `runs`. Splits a run if
/// the relocation breaks it into non-contiguous destination ranges.
pub(crate) fn relocate_runs(runs: &[DataRun], map: &RelocationMap) -> Vec<DataRun> {
    let mut out: Vec<DataRun> = Vec::new();
    for run in runs {
        let Some(lcn) = run.lcn else {
            out.push(*run);
            continue;
        };
        let mut cur = lcn;
        let end = lcn + run.length;
        // Spans coming out of one source run may be merged back together when
        // they happen to land adjacently, but never across source runs — that
        // would shorten a run list the relocation did not actually change,
        // and rewrite records that should have been left alone.
        let mut first = true;
        while cur < end {
            let (dst, span) = map.translate_span(cur, end - cur);
            debug_assert!(span > 0, "translate_span must make progress");
            let merged = if first {
                false
            } else {
                match out.last_mut() {
                    Some(prev) => match prev.lcn {
                        Some(prev_lcn) if prev_lcn + prev.length == dst => {
                            prev.length += span;
                            true
                        }
                        _ => false,
                    },
                    None => false,
                }
            };
            if !merged {
                out.push(DataRun {
                    length: span,
                    lcn: Some(dst),
                });
            }
            first = false;
            cur += span;
        }
    }
    out
}

/// One non-resident attribute, as far as run-list rewriting cares.
/// Resident attributes hold no run list, so a relocation can never change
/// their size; they are not modelled, only stepped over.
#[derive(Debug, Clone)]
pub struct AttrModel {
    /// Offset of the attribute header within the record, as parsed.
    pub attr_offset: usize,
    /// Current on-disk length of the attribute, including its 8-byte
    /// alignment padding.
    pub attr_len: usize,
    /// Attribute-relative offset of the run list. Any attribute *name*
    /// sits before this, so growing the run list never disturbs it.
    pub run_list_offset: usize,
    pub runs: Vec<DataRun>,
}

/// The parts of an MFT record a relocation can disturb, parsed once so
/// the rewriter and the pre-flight simulator work from identical inputs.
#[derive(Debug, Clone)]
pub struct RecordModel {
    /// Bytes in use, reconciled against where the attribute chain
    /// actually ends (some records under-report).
    pub used_size: usize,
    /// Total bytes the record may occupy — its allocated size.
    pub alloc_size: usize,
    pub attrs: Vec<AttrModel>,
}

/// Parse the non-resident attributes of one (fixed-up) MFT record.
pub(crate) fn parse_record_model(record: &[u8]) -> Result<RecordModel> {
    let attrs_offset = le_u16(record, REC_ATTRS_OFFSET)? as usize;
    let used_size_field = le_u32(record, REC_USED_SIZE)? as usize;
    if attrs_offset < 24 || attrs_offset >= record.len() {
        return Err(PhoenixError::InvalidFormat(format!(
            "MFT record: first-attribute offset {attrs_offset} outside record length {}",
            record.len()
        )));
    }
    if used_size_field > record.len() {
        return Err(PhoenixError::InvalidFormat(format!(
            "MFT record: used size {used_size_field} exceeds record length {}",
            record.len()
        )));
    }

    let mut attrs = Vec::new();
    let mut pos = attrs_offset;
    let chain_end = loop {
        if pos + 4 > record.len() {
            return Err(PhoenixError::InvalidFormat(
                "MFT record: attribute chain ran off the end without an END marker".into(),
            ));
        }
        let attr_type = le_u32(record, pos)?;
        if attr_type == ATTR_END {
            break pos + 4;
        }
        if pos + 16 > record.len() {
            return Err(PhoenixError::InvalidFormat(
                "MFT record: attribute header truncated by the end of the record".into(),
            ));
        }
        let attr_len = le_u32(record, pos + 4)? as usize;
        if attr_len == 0 || pos + attr_len > record.len() {
            return Err(PhoenixError::InvalidFormat(format!(
                "MFT record: attribute at offset {pos} has bad length {attr_len}"
            )));
        }
        if record[pos + 8] != 0 {
            // Non-resident: the run-list offset lives at attribute-relative
            // offset 32 and must point inside this attribute.
            if attr_len < 34 {
                return Err(PhoenixError::InvalidFormat(
                    "MFT record: non-resident attribute too short to hold a run-list offset".into(),
                ));
            }
            let run_list_offset = le_u16(record, pos + 32)? as usize;
            if run_list_offset < 34 || run_list_offset >= attr_len {
                return Err(PhoenixError::InvalidFormat(format!(
                    "MFT record: non-resident attribute has run-list offset {run_list_offset} \
                     outside its {attr_len}-byte body"
                )));
            }
            let (runs, _consumed) = parse_run_list(&record[pos + run_list_offset..pos + attr_len])?;
            attrs.push(AttrModel {
                attr_offset: pos,
                attr_len,
                run_list_offset,
                runs,
            });
        }
        pos += attr_len;
    };

    Ok(RecordModel {
        // Trust whichever is larger: a record that under-reports `used_size`
        // would otherwise have its END marker clipped by a tail shift.
        used_size: used_size_field.max(chain_end),
        alloc_size: record.len(),
        attrs,
    })
}

/// What one attribute's run list becomes under a relocation map.
#[derive(Debug, Clone)]
pub(crate) struct AttrFit {
    /// Index into [`RecordModel::attrs`].
    pub(crate) attr_index: usize,
    pub(crate) encoded: Vec<u8>,
    /// The attribute's length after the rewrite. Equal to its current
    /// length whenever the re-encoded list still fits.
    pub(crate) new_attr_len: usize,
}

/// The complete set of edits one record needs. Empty `attrs` means the
/// relocation does not touch this record at all.
#[derive(Debug, Clone)]
pub(crate) struct RecordFitPlan {
    pub(crate) attrs: Vec<AttrFit>,
    pub(crate) new_used_size: usize,
}

/// A record whose rewrite does not fit even after using all the record's
/// slack. Carries the numbers both the backstop error and the pre-flight's
/// re-planner need.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordOverflow {
    /// Record bytes the rewrite would need.
    pub(crate) needed_bytes: usize,
    /// Record bytes available (the allocated record size).
    pub(crate) available: usize,
}

/// Decide, without touching a byte, what a relocation does to one record.
///
/// This is the single source of truth for "does this shrink fit?" — the
/// rewriter applies its output and the pre-flight simulator inspects it,
/// so the two can never disagree about a record's fate.
///
/// An attribute is grown only when its re-encoded run list genuinely
/// outgrows it, and then only to an 8-byte boundary; the growth is paid
/// for out of the record's unused tail. Attributes are never shrunk —
/// leaving neighbours in place keeps the common case a pure in-place
/// overwrite.
pub(crate) fn plan_record_fit(
    model: &RecordModel,
    map: &RelocationMap,
) -> std::result::Result<RecordFitPlan, RecordOverflow> {
    let mut attrs = Vec::new();
    let mut total_growth = 0usize;
    for (attr_index, attr) in model.attrs.iter().enumerate() {
        let new_runs = relocate_runs(&attr.runs, map);
        if new_runs == attr.runs {
            continue;
        }
        let encoded = encode_run_list(&new_runs);
        let required = attr.run_list_offset + encoded.len();
        let new_attr_len = if required <= attr.attr_len {
            attr.attr_len
        } else {
            align8(required)
        };
        total_growth += new_attr_len - attr.attr_len;
        attrs.push(AttrFit {
            attr_index,
            encoded,
            new_attr_len,
        });
    }
    let new_used_size = model.used_size + total_growth;
    if new_used_size > model.alloc_size {
        return Err(RecordOverflow {
            needed_bytes: new_used_size,
            available: model.alloc_size,
        });
    }
    Ok(RecordFitPlan {
        attrs,
        new_used_size,
    })
}

/// Apply a fit plan to the record buffer.
///
/// Growth works front to back, carrying a running `shift`: growing an
/// attribute moves everything after it — later attributes *and* the
/// 0xFFFFFFFF END marker, which lives inside `used_size` and so rides
/// along for free — toward the end of the record. Later attributes are
/// then found at their modelled offset plus the accumulated shift, so
/// several growing attributes in one record compose without special
/// cases.
fn apply_record_fit(record: &mut [u8], model: &RecordModel, plan: &RecordFitPlan) -> Result<()> {
    let mut shift = 0usize;
    let mut tail_end = model.used_size;
    for fit in &plan.attrs {
        let attr = &model.attrs[fit.attr_index];
        let pos = attr.attr_offset + shift;
        let attr_end = pos + attr.attr_len;
        let growth = fit.new_attr_len - attr.attr_len;
        if growth > 0 {
            if attr_end > tail_end || tail_end + growth > record.len() {
                return Err(PhoenixError::InvalidFormat(format!(
                    "MFT record: growing the attribute at offset {pos} by {growth} bytes would \
                     run past the record (tail ends at {tail_end}, record is {} bytes)",
                    record.len()
                )));
            }
            record.copy_within(attr_end..tail_end, attr_end + growth);
            record[pos + 4..pos + 8].copy_from_slice(&(fit.new_attr_len as u32).to_le_bytes());
            tail_end += growth;
            shift += growth;
        }
        let run_list_abs = pos + attr.run_list_offset;
        let new_attr_end = pos + fit.new_attr_len;
        if run_list_abs + fit.encoded.len() > new_attr_end || new_attr_end > record.len() {
            return Err(PhoenixError::InvalidFormat(
                "MFT record: re-encoded run list does not fit the attribute it was planned for"
                    .into(),
            ));
        }
        record[run_list_abs..run_list_abs + fit.encoded.len()].copy_from_slice(&fit.encoded);
        // Zero the slack between the terminator and the attribute's end so
        // no stale run bytes survive a list that got shorter.
        record[run_list_abs + fit.encoded.len()..new_attr_end].fill(0);
    }
    record[REC_USED_SIZE..REC_USED_SIZE + 4]
        .copy_from_slice(&(plan.new_used_size as u32).to_le_bytes());
    Ok(())
}

/// Best-effort file name from a record's resident $FILE_NAME, for error
/// messages only. Prefers the Win32 name over the 8.3 alias.
pub(crate) fn resident_file_name(record: &[u8]) -> Option<String> {
    let attrs_offset = le_u16(record, REC_ATTRS_OFFSET).ok()? as usize;
    let mut pos = attrs_offset;
    let mut best: Option<(u8, String)> = None;
    while pos + 24 <= record.len() {
        let attr_type = le_u32(record, pos).ok()?;
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = le_u32(record, pos + 4).ok()? as usize;
        if attr_len == 0 || pos + attr_len > record.len() {
            break;
        }
        if attr_type == ATTR_FILE_NAME && record[pos + 8] == 0 {
            let value_offset = le_u16(record, pos + 20).ok()? as usize;
            let value = pos + value_offset;
            // $FILE_NAME: name length in UTF-16 units at value+64, the
            // namespace byte at value+65, the name itself from value+66.
            if value + 66 <= pos + attr_len {
                let chars = record[value + 64] as usize;
                let namespace = record[value + 65];
                if value + 66 + chars * 2 <= pos + attr_len {
                    let units: Vec<u16> = (0..chars)
                        .map(|i| {
                            u16::from_le_bytes([
                                record[value + 66 + i * 2],
                                record[value + 67 + i * 2],
                            ])
                        })
                        .collect();
                    if let Ok(name) = String::from_utf16(&units) {
                        // 1 = Win32, 3 = Win32+DOS, 0 = POSIX, 2 = DOS alias.
                        let rank = match namespace {
                            1 | 3 => 0u8,
                            0 => 1,
                            _ => 2,
                        };
                        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                            best = Some((rank, name));
                        }
                    }
                }
            }
        }
        pos += attr_len;
    }
    best.map(|(_, name)| name)
}

fn overflow_error(record: &[u8], record_idx: u64, overflow: &RecordOverflow) -> PhoenixError {
    let name = resident_file_name(record)
        .map(|n| format!(" ({n})"))
        .unwrap_or_default();
    PhoenixError::Other(format!(
        "MFT record {record_idx}{name}: the relocated run lists need {} bytes of record space but \
         only {} are available, even after growing the attribute into the record's free space. \
         The shrink pre-flight is supposed to prevent this before anything is written — please \
         report this source/target combination.",
        overflow.needed_bytes, overflow.available,
    ))
}

/// Walk every non-resident attribute in `record`, translate its run
/// list, and re-encode in place, growing attributes into the record's
/// free space where a relocated list got longer. Returns `true` if
/// anything changed, `Err` if the rewrite outgrows the whole record.
pub(crate) fn rewrite_record_runs(
    record: &mut [u8],
    map: &RelocationMap,
    record_idx: u64,
) -> Result<bool> {
    let model = parse_record_model(record)?;
    let plan = plan_record_fit(&model, map).map_err(|o| overflow_error(record, record_idx, &o))?;
    if plan.attrs.is_empty() {
        return Ok(false);
    }
    apply_record_fit(record, &model, &plan)?;
    Ok(true)
}

fn find_unnamed_data_runs(record: &[u8]) -> Result<Vec<DataRun>> {
    let attrs_offset = le_u16(record, 20)? as usize;
    let mut pos = attrs_offset;
    while pos + 16 <= record.len() {
        let attr_type = le_u32(record, pos)?;
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = le_u32(record, pos + 4)? as usize;
        // A zero (or overshooting) attribute length would never advance
        // `pos`, spinning forever; treat it as corruption.
        if attr_len == 0 || pos + attr_len > record.len() {
            return Err(PhoenixError::InvalidFormat(format!(
                "MFT record: attribute at offset {pos} has bad length {attr_len}"
            )));
        }
        if attr_type == ATTR_DATA {
            let name_len = record[pos + 9] as usize;
            if name_len == 0 {
                let non_resident = record[pos + 8] != 0;
                if !non_resident {
                    return Err(PhoenixError::Other(
                        "$DATA attribute is resident; cannot extract run list".into(),
                    ));
                }
                if pos + 34 > pos + attr_len {
                    return Err(PhoenixError::InvalidFormat(
                        "non-resident $DATA attribute too short to hold a run-list offset".into(),
                    ));
                }
                let run_list_offset = le_u16(record, pos + 32)? as usize;
                let run_list_abs = pos + run_list_offset;
                if run_list_abs > pos + attr_len {
                    return Err(PhoenixError::InvalidFormat(
                        "$DATA run-list offset points past the attribute end".into(),
                    ));
                }
                let run_bytes = &record[run_list_abs..pos + attr_len];
                let (runs, _) = parse_run_list(run_bytes)?;
                return Ok(runs);
            }
        }
        pos += attr_len;
    }
    Err(PhoenixError::Other(
        "no unnamed $DATA attribute found in MFT record".into(),
    ))
}

/// Read MFT record 0 from its post-relocation location, parse its
/// $DATA run list, translate through the map, and convert to a list
/// of (byte_offset, byte_count) extents on the target disk so the
/// walker can read every record by linear MFT index.
fn build_mft_extents_in_bytes(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    map: &RelocationMap,
) -> Result<Vec<(u64, u64)>> {
    let mft_lcn_dst = map.translate_cluster(boot.mft_lcn).unwrap_or(boot.mft_lcn);
    let mft_offset_bytes = mft_lcn_dst * boot.cluster_size;
    let mut record0 = vec![0u8; boot.bytes_per_record as usize];
    writer.read_at(mft_offset_bytes, &mut record0)?;
    apply_fixups_for_read(&mut record0)?;
    let runs = find_unnamed_data_runs(&record0)?;
    let translated = relocate_runs(&runs, map);
    let mut out = Vec::new();
    for run in translated {
        let lcn = run.lcn.ok_or_else(|| {
            PhoenixError::Other(
                "$MFT $DATA attribute contains a sparse run; $MFT is never sparse on real \
                 volumes - refusing to proceed"
                    .into(),
            )
        })?;
        out.push((lcn * boot.cluster_size, run.length * boot.cluster_size));
    }
    Ok(out)
}

fn record_byte_offset(
    mft_extents: &[(u64, u64)],
    record_idx: u64,
    bytes_per_record: u64,
) -> Option<u64> {
    let mut covered_records: u64 = 0;
    for (start_byte, len_bytes) in mft_extents {
        let records_in_extent = len_bytes / bytes_per_record;
        if record_idx < covered_records + records_in_extent {
            let offset_in_extent = (record_idx - covered_records) * bytes_per_record;
            return Some(start_byte + offset_in_extent);
        }
        covered_records += records_in_extent;
    }
    None
}

/// Rewrite every MFT record's run lists under `map`, in two passes.
///
/// The first pass rewrites into a scratch buffer and throws the result
/// away; only if *every* record survives does the second pass write
/// anything. Without this, a record that overflows halfway through the
/// MFT leaves the volume with some records relocated and some not —
/// unrecoverable, because the two halves disagree about where the data
/// lives. The cost is one extra sequential read of the MFT.
fn walk_and_rewrite_mft(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    map: &RelocationMap,
    mft_extents: &[(u64, u64)],
) -> Result<u64> {
    let simulated = walk_mft_records(writer, boot, map, mft_extents, false)?;
    tracing::info!(
        records = simulated,
        "MFT rewrite dry run clean; committing to disk"
    );
    walk_mft_records(writer, boot, map, mft_extents, true)
}

/// One pass of the MFT walk. With `commit` false nothing is written, so
/// the pass is a pure feasibility check over the real on-disk records.
fn walk_mft_records(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    map: &RelocationMap,
    mft_extents: &[(u64, u64)],
    commit: bool,
) -> Result<u64> {
    let total_bytes: u64 = mft_extents.iter().map(|(_, l)| *l).sum();
    let total_records = total_bytes / boot.bytes_per_record;
    let mut buf = vec![0u8; boot.bytes_per_record as usize];
    let mut rewritten = 0u64;
    for idx in 0..total_records {
        let off = match record_byte_offset(mft_extents, idx, boot.bytes_per_record) {
            Some(o) => o,
            None => break,
        };
        writer.read_at(off, &mut buf)?;
        if &buf[0..4] != b"FILE" {
            continue;
        }
        apply_fixups_for_read(&mut buf)?;
        let changed = rewrite_record_runs(&mut buf, map, idx)?;
        if changed {
            rewritten += 1;
            if commit {
                apply_fixups_for_write(&mut buf)?;
                writer.write_at(off, &buf)?;
            }
        }
    }
    Ok(rewritten)
}

/// Regenerate $Bitmap so it describes the new volume:
///   * Bits past `safe_max_cluster` are cleared.
///   * Bits at the relocation destinations are set.
fn rewrite_bitmap(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    mft_extents: &[(u64, u64)],
    map: &RelocationMap,
) -> Result<()> {
    let off = match record_byte_offset(mft_extents, MFT_RECORD_BITMAP, boot.bytes_per_record) {
        Some(o) => o,
        None => return Ok(()),
    };
    let mut buf = vec![0u8; boot.bytes_per_record as usize];
    writer.read_at(off, &mut buf)?;
    if &buf[0..4] != b"FILE" {
        return Ok(());
    }
    apply_fixups_for_read(&mut buf)?;
    let runs = find_unnamed_data_runs(&buf)?;
    let translated = relocate_runs(&runs, map);

    let total_bitmap_bytes: u64 =
        translated.iter().map(|r| r.length).sum::<u64>() * boot.cluster_size;
    let mut bitmap = vec![0u8; total_bitmap_bytes as usize];
    let mut cursor = 0usize;
    for run in &translated {
        let len = (run.length * boot.cluster_size) as usize;
        match run.lcn {
            None => cursor += len,
            Some(lcn) => {
                let phys = lcn * boot.cluster_size;
                writer.read_at(phys, &mut bitmap[cursor..cursor + len])?;
                cursor += len;
            }
        }
    }

    let new_total = map.new_total_clusters;
    let usable_bytes = new_total.div_ceil(8) as usize;
    if usable_bytes < bitmap.len() {
        bitmap[usable_bytes..].fill(0);
    }
    if !new_total.is_multiple_of(8) && (new_total as usize) / 8 < bitmap.len() {
        let boundary_byte = (new_total as usize) / 8;
        let bits_to_keep = (new_total % 8) as u8;
        let mask = (1u8 << bits_to_keep) - 1;
        bitmap[boundary_byte] &= mask;
    }

    for entry in &map.entries {
        for c in 0..entry.cluster_count {
            let dst = entry.dst_cluster_start + c;
            let byte = (dst / 8) as usize;
            let bit = (dst % 8) as u8;
            if byte < bitmap.len() {
                bitmap[byte] |= 1u8 << bit;
            }
        }
    }

    let mut cursor = 0usize;
    for run in &translated {
        let len = (run.length * boot.cluster_size) as usize;
        match run.lcn {
            None => cursor += len,
            Some(lcn) => {
                let phys = lcn * boot.cluster_size;
                writer.write_at(phys, &bitmap[cursor..cursor + len])?;
                cursor += len;
            }
        }
    }
    Ok(())
}

/// Stamp $LogFile's $DATA with 0xFF so NTFS treats it as already
/// replayed at next mount.
fn stamp_logfile(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    mft_extents: &[(u64, u64)],
    map: &RelocationMap,
) -> Result<()> {
    let off = match record_byte_offset(mft_extents, MFT_RECORD_LOGFILE, boot.bytes_per_record) {
        Some(o) => o,
        None => return Ok(()),
    };
    let mut buf = vec![0u8; boot.bytes_per_record as usize];
    writer.read_at(off, &mut buf)?;
    if &buf[0..4] != b"FILE" {
        return Ok(());
    }
    apply_fixups_for_read(&mut buf)?;
    let runs = find_unnamed_data_runs(&buf)?;
    let translated = relocate_runs(&runs, map);
    let blank_cluster = vec![0xFFu8; boot.cluster_size as usize];
    for run in &translated {
        if let Some(lcn) = run.lcn {
            for c in 0..run.length {
                let phys = (lcn + c) * boot.cluster_size;
                writer.write_at(phys, &blank_cluster)?;
            }
        }
    }
    Ok(())
}

/// Copy the (now-rewritten) first 4 MFT records to the MFT mirror.
fn rewrite_mft_mirror(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    mft_extents: &[(u64, u64)],
    map: &RelocationMap,
) -> Result<()> {
    let mirror_lcn = map
        .translate_cluster(boot.mft_mirror_lcn)
        .unwrap_or(boot.mft_mirror_lcn);
    let mirror_byte_offset = mirror_lcn * boot.cluster_size;
    let mut buf = vec![0u8; boot.bytes_per_record as usize];
    for idx in 0..4u64 {
        let primary_off = match record_byte_offset(mft_extents, idx, boot.bytes_per_record) {
            Some(o) => o,
            None => break,
        };
        writer.read_at(primary_off, &mut buf)?;
        if &buf[0..4] != b"FILE" {
            continue;
        }
        let mirror_off = mirror_byte_offset + idx * boot.bytes_per_record;
        writer.write_at(mirror_off, &buf)?;
    }
    Ok(())
}

/// Patch the boot sector's MftLcn / MftMirrLcn fields to point at
/// the (possibly relocated) post-relocation positions.
fn update_boot_mft_pointers(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    map: &RelocationMap,
) -> Result<()> {
    let new_mft = map.translate_cluster(boot.mft_lcn).unwrap_or(boot.mft_lcn);
    let new_mirror = map
        .translate_cluster(boot.mft_mirror_lcn)
        .unwrap_or(boot.mft_mirror_lcn);
    if new_mft == boot.mft_lcn && new_mirror == boot.mft_mirror_lcn {
        return Ok(());
    }
    let mut sector = vec![0u8; 512];
    writer.read_at(0, &mut sector)?;
    sector[48..56].copy_from_slice(&new_mft.to_le_bytes());
    sector[56..64].copy_from_slice(&new_mirror.to_le_bytes());
    writer.write_at(0, &sector)?;
    tracing::info!(
        old_mft_lcn = boot.mft_lcn,
        new_mft_lcn = new_mft,
        old_mft_mirror_lcn = boot.mft_mirror_lcn,
        new_mft_mirror_lcn = new_mirror,
        "patched NTFS boot sector mft_lcn / mft_mirror_lcn"
    );
    Ok(())
}

/// Top-level entry point. Called from `restore_ntfs` after
/// `restore_raw` has streamed all data (with relocation translation)
/// to disk.
pub fn rewrite_metadata_after_relocation(
    writer: &mut PartitionWriter,
    map: &RelocationMap,
) -> Result<()> {
    writer.flush()?;
    let boot = read_ntfs_boot(writer)?;
    if boot.cluster_size != map.cluster_size {
        return Err(PhoenixError::Other(format!(
            "NTFS boot sector reports cluster_size={} but relocation map was built with {}; \
             refusing to proceed with mismatched cluster sizes",
            boot.cluster_size, map.cluster_size
        )));
    }
    tracing::info!(
        mft_lcn = boot.mft_lcn,
        mft_mirror_lcn = boot.mft_mirror_lcn,
        cluster_size = boot.cluster_size,
        relocation_entries = map.entries.len(),
        "starting NTFS metadata rewrite under relocation map"
    );

    let mft_extents = build_mft_extents_in_bytes(writer, &boot, map)?;
    let rewritten = walk_and_rewrite_mft(writer, &boot, map, &mft_extents)?;
    tracing::info!(rewritten, "MFT walk complete");

    rewrite_bitmap(writer, &boot, &mft_extents, map)?;
    stamp_logfile(writer, &boot, &mft_extents, map)?;
    rewrite_mft_mirror(writer, &boot, &mft_extents, map)?;
    update_boot_mft_pointers(writer, &boot, map)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::relocation::RelocationEntry;

    // ---- synthetic MFT record construction -------------------------------
    //
    // Real records come from a formatted volume, which unit tests can't have.
    // These builders emit the same on-disk shapes: a non-resident attribute
    // with its run list at attribute offset 64, a resident attribute with its
    // value at 24, and a record header whose `used_size` covers the chain
    // including the END marker.

    const REC_LEN: usize = 1024;
    const ATTRS_OFFSET: usize = 56;

    /// A non-resident attribute holding `runs`. `extra_pad` inflates it past
    /// the tight packing Windows actually writes (0 = tightly packed, which
    /// leaves only the 8-byte alignment slack a real volume has).
    fn nonres_attr(attr_type: u32, runs: &[DataRun], extra_pad: usize) -> Vec<u8> {
        let encoded = encode_run_list(runs);
        let len = align8(64 + encoded.len() + extra_pad);
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&attr_type.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1; // non-resident
        a[10..12].copy_from_slice(&64u16.to_le_bytes()); // name offset
        a[32..34].copy_from_slice(&64u16.to_le_bytes()); // run-list offset
        a[64..64 + encoded.len()].copy_from_slice(&encoded);
        a
    }

    fn res_attr(attr_type: u32, payload: &[u8]) -> Vec<u8> {
        let len = align8(24 + payload.len());
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&attr_type.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 0; // resident
        a[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes()); // value offset
        a[24..24 + payload.len()].copy_from_slice(payload);
        a
    }

    fn build_record(attrs: &[Vec<u8>]) -> Vec<u8> {
        try_build_record(attrs, REC_LEN).expect("test attributes do not fit a 1024-byte record")
    }

    /// `None` when the attributes plus the END marker don't fit `record_len`.
    /// Note NTFS counts the 4-byte END marker in `used_size`, so a record's
    /// used size is congruent to 4 mod 8 while attribute lengths are 8-aligned
    /// — which is why slack is never itself a multiple of 8, and why the
    /// boundary tests below size the record rather than the filler.
    fn try_build_record(attrs: &[Vec<u8>], record_len: usize) -> Option<Vec<u8>> {
        let total: usize = attrs.iter().map(|a| a.len()).sum();
        if ATTRS_OFFSET + total + 4 > record_len {
            return None;
        }
        let mut rec = vec![0u8; record_len];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&48u16.to_le_bytes()); // USA offset
        rec[6..8].copy_from_slice(&((record_len / 512 + 1) as u16).to_le_bytes());
        rec[REC_ATTRS_OFFSET..REC_ATTRS_OFFSET + 2]
            .copy_from_slice(&(ATTRS_OFFSET as u16).to_le_bytes());
        rec[28..32].copy_from_slice(&(record_len as u32).to_le_bytes());
        let mut pos = ATTRS_OFFSET;
        for a in attrs {
            rec[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        rec[pos..pos + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        rec[REC_USED_SIZE..REC_USED_SIZE + 4].copy_from_slice(&((pos + 4) as u32).to_le_bytes());
        Some(rec)
    }

    /// A map that breaks one 20-cluster run at LCN 200 into two pieces
    /// landing far apart, forcing the run list to gain an entry.
    fn splitting_map() -> RelocationMap {
        RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![
                RelocationEntry {
                    src_cluster_start: 200,
                    cluster_count: 10,
                    dst_cluster_start: 10,
                },
                RelocationEntry {
                    src_cluster_start: 210,
                    cluster_count: 10,
                    dst_cluster_start: 60,
                },
            ],
        }
    }

    fn split_runs() -> Vec<DataRun> {
        vec![
            DataRun {
                length: 20,
                lcn: Some(200),
            },
            DataRun {
                length: 8,
                lcn: Some(300),
            },
        ]
    }

    fn used_size_of(record: &[u8]) -> usize {
        le_u32(record, REC_USED_SIZE).unwrap() as usize
    }

    /// How many bytes this record's rewrite needs beyond what it uses now.
    fn growth_under(record: &[u8], map: &RelocationMap) -> usize {
        let model = parse_record_model(record).unwrap();
        plan_record_fit(&model, map).unwrap().new_used_size - model.used_size
    }

    #[test]
    fn tightly_packed_attribute_grows_into_record_slack() {
        // The regression this whole change exists for: Windows packs an
        // attribute to its run list rounded to 8 bytes, so a relocation that
        // splits even one run overflows the attribute by a byte or two while
        // the surrounding record still has hundreds of bytes free.
        let map = splitting_map();
        let runs = split_runs();
        let mut rec = build_record(&[nonres_attr(ATTR_DATA, &runs, 0)]);
        let before = used_size_of(&rec);

        let attr_budget = {
            let m = parse_record_model(&rec).unwrap();
            m.attrs[0].attr_len - m.attrs[0].run_list_offset
        };
        let needed = encode_run_list(&relocate_runs(&runs, &map)).len();
        assert!(
            needed > attr_budget,
            "test fixture must overflow the attribute ({needed} vs {attr_budget})"
        );

        assert!(rewrite_record_runs(&mut rec, &map, 42).unwrap());

        let after = parse_record_model(&rec).unwrap();
        assert_eq!(after.attrs.len(), 1);
        assert_eq!(after.attrs[0].runs, relocate_runs(&runs, &map));
        assert!(after.used_size > before, "record should have grown");
        // Attributes are 8-aligned, so growth comes in whole 8-byte steps.
        assert_eq!((after.used_size - before) % 8, 0);
        assert!(after.used_size <= REC_LEN);
    }

    #[test]
    fn growth_shifts_following_attributes_without_damaging_them() {
        let map = splitting_map();
        let payload: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        let mut rec = build_record(&[
            nonres_attr(ATTR_DATA, &split_runs(), 0),
            res_attr(ATTR_FILE_NAME, &payload),
        ]);
        assert!(rewrite_record_runs(&mut rec, &map, 7).unwrap());

        // The resident attribute must survive the shift byte for byte, and
        // the END marker must still terminate the chain at used_size.
        let mut pos = ATTRS_OFFSET;
        let mut found = None;
        loop {
            let t = le_u32(&rec, pos).unwrap();
            if t == ATTR_END {
                break;
            }
            let len = le_u32(&rec, pos + 4).unwrap() as usize;
            if t == ATTR_FILE_NAME {
                let vo = le_u16(&rec, pos + 20).unwrap() as usize;
                found = Some(rec[pos + vo..pos + vo + payload.len()].to_vec());
            }
            pos += len;
        }
        assert_eq!(found.as_deref(), Some(payload.as_slice()));
        assert_eq!(pos + 4, used_size_of(&rec));
    }

    #[test]
    fn several_growing_attributes_in_one_record_compose() {
        let map = splitting_map();
        let runs = split_runs();
        let mut rec = build_record(&[
            nonres_attr(ATTR_DATA, &runs, 0),
            nonres_attr(ATTR_DATA, &runs, 0),
            nonres_attr(ATTR_DATA, &runs, 0),
        ]);
        assert!(rewrite_record_runs(&mut rec, &map, 1).unwrap());

        let after = parse_record_model(&rec).unwrap();
        assert_eq!(after.attrs.len(), 3);
        let expected = relocate_runs(&runs, &map);
        for attr in &after.attrs {
            assert_eq!(attr.runs, expected);
        }
    }

    #[test]
    fn growth_that_exactly_fills_the_record_is_allowed() {
        let map = splitting_map();
        // Learn the growth from a roomy record, then build one whose slack is
        // exactly that — the boundary case between fitting and overflowing.
        let attr = nonres_attr(ATTR_DATA, &split_runs(), 0);
        let probe = build_record(std::slice::from_ref(&attr));
        let growth = growth_under(&probe, &map);
        assert!(growth > 0);
        // Size the record so its slack is exactly the growth needed.
        let exact = used_size_of(&probe) + growth;
        let mut rec = try_build_record(std::slice::from_ref(&attr), exact).unwrap();

        assert!(rewrite_record_runs(&mut rec, &map, 3).unwrap());
        assert_eq!(used_size_of(&rec), exact);
    }

    #[test]
    fn overflow_past_the_record_end_is_refused() {
        let map = splitting_map();
        let attr = nonres_attr(ATTR_DATA, &split_runs(), 0);
        let probe = build_record(std::slice::from_ref(&attr));
        let growth = growth_under(&probe, &map);
        // One 8-byte step short of what the rewrite needs.
        let tight = used_size_of(&probe) + growth - 8;
        let mut rec = try_build_record(std::slice::from_ref(&attr), tight).unwrap();
        let err = rewrite_record_runs(&mut rec, &map, 194).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("194"), "{msg}");
        assert!(msg.contains("pre-flight"), "{msg}");
    }

    #[test]
    fn a_shorter_run_list_is_padded_in_place_without_shifting() {
        // Relocation that merges nothing but moves a run to a nearby LCN
        // encodes shorter (smaller delta); the attribute must stay put.
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![RelocationEntry {
                src_cluster_start: 100_000,
                cluster_count: 4,
                dst_cluster_start: 1,
            }],
        };
        let runs = vec![DataRun {
            length: 4,
            lcn: Some(100_000),
        }];
        let mut rec = build_record(&[nonres_attr(ATTR_DATA, &runs, 0)]);
        let before = used_size_of(&rec);
        assert!(rewrite_record_runs(&mut rec, &map, 5).unwrap());
        assert_eq!(used_size_of(&rec), before, "no shift expected");
        let after = parse_record_model(&rec).unwrap();
        assert_eq!(after.attrs[0].runs, relocate_runs(&runs, &map));
    }

    #[test]
    fn an_untouched_record_reports_no_change() {
        // Runs entirely below the boundary: identity translation, no write.
        let map = splitting_map();
        let runs = vec![DataRun {
            length: 4,
            lcn: Some(10),
        }];
        let mut rec = build_record(&[nonres_attr(ATTR_DATA, &runs, 0)]);
        let copy = rec.clone();
        assert!(!rewrite_record_runs(&mut rec, &map, 0).unwrap());
        assert_eq!(rec, copy, "a no-op rewrite must not touch a single byte");
    }

    #[test]
    fn plan_record_fit_predicts_the_rewrite_exactly() {
        // The anti-divergence guarantee the pre-flight rests on: whatever
        // `plan_record_fit` says will happen is what `rewrite_record_runs`
        // actually does, across a spread of slack and fragmentation.
        let map = splitting_map();
        for pad in [0usize, 8, 24] {
            for filler_len in [0usize, 200, 700, 860, 900] {
                let mut attrs = vec![nonres_attr(ATTR_DATA, &split_runs(), pad)];
                if filler_len > 0 {
                    attrs.push(res_attr(ATTR_FILE_NAME, &vec![7u8; filler_len]));
                }
                let Some(mut rec) = try_build_record(&attrs, REC_LEN) else {
                    continue; // combination doesn't fit a real record; not a case
                };
                let model = parse_record_model(&rec).unwrap();
                let predicted = plan_record_fit(&model, &map);
                let actual = rewrite_record_runs(&mut rec, &map, 0);
                match (predicted, actual) {
                    (Ok(plan), Ok(changed)) => {
                        assert_eq!(
                            plan.attrs.is_empty(),
                            !changed,
                            "pad={pad} fill={filler_len}"
                        );
                        if changed {
                            assert_eq!(
                                used_size_of(&rec),
                                plan.new_used_size,
                                "pad={pad} fill={filler_len}"
                            );
                        }
                    }
                    (Err(_), Err(_)) => {}
                    (p, a) => panic!(
                        "prediction and reality disagree at pad={pad} fill={filler_len}: \
                         predicted ok={} actual ok={}",
                        p.is_ok(),
                        a.is_ok()
                    ),
                }
            }
        }
    }

    #[test]
    fn resident_file_name_prefers_the_win32_name() {
        let mut dos = vec![0u8; 66 + 8];
        dos[64] = 4;
        dos[65] = 2; // DOS namespace
        for (i, c) in "DOSN".encode_utf16().enumerate() {
            dos[66 + i * 2..68 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        let mut win32 = vec![0u8; 66 + 16];
        win32[64] = 7;
        win32[65] = 1; // Win32 namespace
        for (i, c) in "real.txt".encode_utf16().take(7).enumerate() {
            win32[66 + i * 2..68 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        let rec = build_record(&[
            res_attr(ATTR_FILE_NAME, &dos),
            res_attr(ATTR_FILE_NAME, &win32),
        ]);
        assert_eq!(resident_file_name(&rec).as_deref(), Some("real.tx"));
    }

    /// The parsers all consume untrusted on-disk bytes; none of them may
    /// panic on malformed input. Feed truncations and random garbage and
    /// require an `Err` (or `Ok`), never an unwind.
    #[test]
    fn parsers_never_panic_on_garbage() {
        let empty_map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 0,
            new_total_clusters: 0,
            entries: Vec::new(),
        };
        // Deterministic pseudo-random bytes (no rng dependency).
        let mut seed: u32 = 0x1234_5678;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        for len in 0..300usize {
            let mut buf: Vec<u8> = (0..len).map(|_| next()).collect();
            // A record that starts with "FILE" exercises deeper paths.
            if len >= 4 {
                buf[0..4].copy_from_slice(b"FILE");
            }
            let _ = parse_run_list(&buf);
            let _ = find_unnamed_data_runs(&buf);
            let mut rec = buf.clone();
            let _ = rewrite_record_runs(&mut rec, &empty_map, 0);
            let mut rec2 = buf.clone();
            let _ = apply_fixups_for_read(&mut rec2);
            let mut rec3 = buf.clone();
            let _ = apply_fixups_for_write(&mut rec3);
        }
    }

    #[test]
    fn parse_then_encode_roundtrips_simple_run_list() {
        let runs = vec![
            DataRun {
                length: 8,
                lcn: Some(100),
            },
            DataRun {
                length: 4,
                lcn: Some(200),
            },
            DataRun {
                length: 16,
                lcn: Some(50),
            },
        ];
        let encoded = encode_run_list(&runs);
        let (decoded, _) = parse_run_list(&encoded).unwrap();
        assert_eq!(decoded.len(), runs.len());
        for (a, b) in runs.iter().zip(decoded.iter()) {
            assert_eq!(a.length, b.length);
            assert_eq!(a.lcn, b.lcn);
        }
    }

    #[test]
    fn parse_handles_sparse_run() {
        let runs = vec![
            DataRun {
                length: 8,
                lcn: Some(100),
            },
            DataRun {
                length: 4,
                lcn: None,
            },
            DataRun {
                length: 8,
                lcn: Some(108),
            },
        ];
        let encoded = encode_run_list(&runs);
        let (decoded, _) = parse_run_list(&encoded).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[1].lcn, None);
        assert_eq!(decoded[2].lcn, Some(108));
    }

    #[test]
    fn relocate_runs_translates_above_boundary() {
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![RelocationEntry {
                src_cluster_start: 200,
                cluster_count: 10,
                dst_cluster_start: 50,
            }],
        };
        let runs = vec![DataRun {
            length: 10,
            lcn: Some(200),
        }];
        let out = relocate_runs(&runs, &map);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lcn, Some(50));
        assert_eq!(out[0].length, 10);
    }

    #[test]
    fn relocate_runs_passes_through_below_boundary_unchanged() {
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![],
        };
        let runs = vec![DataRun {
            length: 10,
            lcn: Some(50),
        }];
        let out = relocate_runs(&runs, &map);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lcn, Some(50));
    }

    #[test]
    fn relocate_runs_splits_when_destination_not_contiguous() {
        let map = RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: 99,
            new_total_clusters: 100,
            entries: vec![
                RelocationEntry {
                    src_cluster_start: 200,
                    cluster_count: 5,
                    dst_cluster_start: 50,
                },
                RelocationEntry {
                    src_cluster_start: 205,
                    cluster_count: 5,
                    dst_cluster_start: 70,
                },
            ],
        };
        let runs = vec![DataRun {
            length: 10,
            lcn: Some(200),
        }];
        let out = relocate_runs(&runs, &map);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].lcn, Some(50));
        assert_eq!(out[0].length, 5);
        assert_eq!(out[1].lcn, Some(70));
        assert_eq!(out[1].length, 5);
    }
}
