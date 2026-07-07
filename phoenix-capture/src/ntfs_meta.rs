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
//! Out of scope (refused with an error rather than silently
//! corrupting): a relocation that grows a non-resident attribute's
//! run list past its in-record byte budget. NTFS would migrate the
//! attribute to $ATTRIBUTE_LIST; we don't implement that yet.

use crate::raw::PartitionWriter;
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::relocation::RelocationMap;

const MFT_RECORD_LOGFILE: u64 = 2;
const MFT_RECORD_BITMAP: u64 = 6;

const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

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
struct NtfsBoot {
    bytes_per_sector: u64,
    cluster_size: u64,
    mft_lcn: u64,
    mft_mirror_lcn: u64,
    bytes_per_record: u64,
}

fn read_ntfs_boot(writer: &mut PartitionWriter) -> Result<NtfsBoot> {
    let mut sector = vec![0u8; 512];
    writer.read_at(0, &mut sector)?;
    if &sector[3..7] != b"NTFS" {
        return Err(PhoenixError::Other(
            "boot sector is not NTFS (expected 'NTFS' at offset 3); cannot rewrite metadata".into(),
        ));
    }
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]) as u64;
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
        bytes_per_sector,
        cluster_size,
        mft_lcn,
        mft_mirror_lcn,
        bytes_per_record,
    })
}

/// Apply NTFS Update Sequence Array (USA) fixups to a record buffer
/// read from disk. NTFS overwrites the last 2 bytes of each
/// 512-byte sector with the record's USN as a corruption-detection
/// hack; the original 2-byte values live in the USA. This puts them
/// back so the record is the in-memory view that the spec describes.
fn apply_fixups_for_read(record: &mut [u8], bytes_per_sector: u64) -> Result<()> {
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
    let bps = bytes_per_sector as usize;
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

fn apply_fixups_for_write(record: &mut [u8], bytes_per_sector: u64) -> Result<()> {
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
    let bps = bytes_per_sector as usize;
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
#[derive(Debug, Clone, Copy)]
struct DataRun {
    length: u64,
    lcn: Option<u64>,
}

/// Parse a run list starting at `bytes[0]` until the 0x00 terminator.
fn parse_run_list(bytes: &[u8]) -> Result<(Vec<DataRun>, usize)> {
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
fn encode_run_list(runs: &[DataRun]) -> Vec<u8> {
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
fn relocate_runs(runs: &[DataRun], map: &RelocationMap) -> Vec<DataRun> {
    let mut out = Vec::new();
    for run in runs {
        let lcn = match run.lcn {
            None => {
                out.push(*run);
                continue;
            }
            Some(l) => l,
        };
        let mut cur_src = lcn;
        let end_src = lcn + run.length;
        while cur_src < end_src {
            let dst = map.translate_cluster(cur_src).unwrap_or(cur_src);
            let mut span = 1u64;
            while cur_src + span < end_src {
                let next_dst = map
                    .translate_cluster(cur_src + span)
                    .unwrap_or(cur_src + span);
                if next_dst != dst + span {
                    break;
                }
                span += 1;
            }
            out.push(DataRun {
                length: span,
                lcn: Some(dst),
            });
            cur_src += span;
        }
    }
    out
}

/// Walk every non-resident attribute in `record`, translate its run
/// list, and re-encode in place. Returns `true` if anything changed,
/// `Err` if a re-encoded run list no longer fits in its budget.
fn rewrite_record_runs(record: &mut [u8], map: &RelocationMap, record_idx: u64) -> Result<bool> {
    let attrs_offset = le_u16(record, 20)? as usize;
    let used_size = le_u32(record, 24)? as usize;
    let mut changed = false;
    let mut pos = attrs_offset;
    while pos + 16 <= record.len() {
        let attr_type = le_u32(record, pos)?;
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = le_u32(record, pos + 4)? as usize;
        if attr_len == 0 || pos + attr_len > record.len() || pos + attr_len > used_size {
            return Err(PhoenixError::InvalidFormat(format!(
                "MFT record {record_idx}: attribute at offset {pos} has bad length {attr_len}"
            )));
        }
        let non_resident = record[pos + 8] != 0;
        if non_resident {
            // The run-list offset lives at attribute-relative offset 32; it
            // must sit inside this attribute (which is already known to be
            // within the record).
            if pos + 34 > pos + attr_len {
                return Err(PhoenixError::InvalidFormat(format!(
                    "MFT record {record_idx}: non-resident attribute too short to hold a \
                     run-list offset"
                )));
            }
            let run_list_offset = le_u16(record, pos + 32)? as usize;
            let run_list_abs = pos + run_list_offset;
            if run_list_abs >= pos + attr_len {
                return Err(PhoenixError::Other(format!(
                    "MFT record {record_idx}: non-resident attribute has run-list offset past \
                     attribute end"
                )));
            }
            let run_bytes = &record[run_list_abs..pos + attr_len];
            let (runs, _consumed) = parse_run_list(run_bytes)?;
            let new_runs = relocate_runs(&runs, map);
            let needs_rewrite = new_runs.len() != runs.len()
                || runs
                    .iter()
                    .zip(new_runs.iter())
                    .any(|(a, b)| a.lcn != b.lcn || a.length != b.length);
            if needs_rewrite {
                let encoded = encode_run_list(&new_runs);
                let budget = (pos + attr_len) - run_list_abs;
                if encoded.len() > budget {
                    return Err(PhoenixError::Other(format!(
                        "MFT record {record_idx}: relocated run list grew to {} bytes, past the \
                         {} byte attribute budget. v1 doesn't yet implement $ATTRIBUTE_LIST \
                         migration; pick a less aggressive shrink target.",
                        encoded.len(),
                        budget,
                    )));
                }
                let dst = &mut record[run_list_abs..pos + attr_len];
                dst[..encoded.len()].copy_from_slice(&encoded);
                for b in &mut dst[encoded.len()..] {
                    *b = 0;
                }
                changed = true;
            }
        }
        pos += attr_len;
    }
    Ok(changed)
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
    apply_fixups_for_read(&mut record0, boot.bytes_per_sector)?;
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

fn walk_and_rewrite_mft(
    writer: &mut PartitionWriter,
    boot: &NtfsBoot,
    map: &RelocationMap,
    mft_extents: &[(u64, u64)],
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
        apply_fixups_for_read(&mut buf, boot.bytes_per_sector)?;
        let changed = rewrite_record_runs(&mut buf, map, idx)?;
        if changed {
            apply_fixups_for_write(&mut buf, boot.bytes_per_sector)?;
            writer.write_at(off, &buf)?;
            rewritten += 1;
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
    apply_fixups_for_read(&mut buf, boot.bytes_per_sector)?;
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
    for byte_idx in usable_bytes..bitmap.len() {
        bitmap[byte_idx] = 0;
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
    apply_fixups_for_read(&mut buf, boot.bytes_per_sector)?;
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
            let _ = apply_fixups_for_read(&mut rec2, 512);
            let mut rec3 = buf.clone();
            let _ = apply_fixups_for_write(&mut rec3, 512);
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
