//! VHDX container synthesis — the prologue that lets a mounted backup advertise
//! a **4096-byte logical sector size**.
//!
//! # Why this exists
//!
//! Mounting works by synthesizing a virtual disk over the `.phnx` and pointing
//! `AttachVirtualDisk` at it. That disk was a **fixed VHD**, whose format pins the
//! sector size to 512 — full stop, it is not a field in the format. A volume
//! captured from a 4Kn disk records `BytesPerSector = 4096` in its own boot
//! sector, and NTFS refuses to mount when the filesystem's sector size disagrees
//! with the device's. So a 4Kn backup could be attached and never mounted, and
//! the honest thing to do was refuse it.
//!
//! VHDX carries the logical sector size as **metadata**, which is the whole point
//! of using it here.
//!
//! # The shape
//!
//! A VHDX is normally a sparse, block-allocated format with a log for crash
//! consistency. We need none of that: the image is read-only and every byte of it
//! already exists (served on demand from the chunk store). So we emit the
//! simplest VHDX that is still a legal one — every payload block marked
//! `FULLY_PRESENT` and laid out **contiguously**, in order, immediately after a
//! fixed-size prologue:
//!
//! ```text
//!   0 MiB  File Type Identifier ("vhdxfile")
//!  64 KiB  Header 1 ─┐ identical but for the sequence number; Windows takes
//! 128 KiB  Header 2 ─┘ whichever has the higher one and a valid CRC-32C
//! 192 KiB  Region Table 1 ─┐ points at the BAT and Metadata regions
//! 256 KiB  Region Table 2 ─┘
//!   1 MiB  Log region (present, empty — LogGuid is zero, so no replay)
//!   2 MiB  Metadata region  <- LogicalSectorSize lives here
//!   3 MiB  BAT region
//!   N MiB  Payload: the raw disk image, verbatim
//! ```
//!
//! That last line is what makes this cheap: because the payload is contiguous and
//! in order, file offset `payload_start + i` **is** image offset `i`. The chunk
//! store is untouched — [`Vhdx::payload_start`] is the only thing the reader needs
//! to know. It is the same trick the fixed-VHD path uses (`[image][footer]`), just
//! with the fixture in front instead of behind.
//!
//! # Reading the spec
//!
//! Field layouts follow the "[MS-VHDX]" open specification. The two places it is
//! easy to get quietly wrong, both of which produce a file Windows rejects with no
//! diagnosis whatsoever:
//!
//!   * checksums are **CRC-32C** (Castagnoli), not the IEEE CRC-32 that GPT uses
//!     (see `phoenix_core::hash::crc32c`);
//!   * every region and payload block must be **1 MiB aligned** in the file.

use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash::crc32c;

const MIB: u64 = 1024 * 1024;
const KIB: usize = 1024;

/// Payload block size. Any power of two in 1 MiB..=256 MiB is legal; 2 MiB keeps
/// the BAT small (8 bytes per block) without making the final partial block
/// wasteful.
const BLOCK_SIZE: u64 = 2 * MIB;

/// Region/block alignment mandated by the spec.
const ALIGN: u64 = MIB;

const HEADER_1_OFFSET: usize = 64 * KIB;
const HEADER_2_OFFSET: usize = 128 * KIB;
const REGION_TABLE_1_OFFSET: usize = 192 * KIB;
const REGION_TABLE_2_OFFSET: usize = 256 * KIB;

/// The headers occupy the first 1 MiB; everything else is 1 MiB-aligned after it.
const LOG_OFFSET: u64 = MIB;
const LOG_LENGTH: u64 = MIB;
const METADATA_OFFSET: u64 = 2 * MIB;
const METADATA_LENGTH: u64 = MIB;
const BAT_OFFSET: u64 = 3 * MIB;

/// BAT entry states ([MS-VHDX] 2.5).
const PAYLOAD_BLOCK_FULLY_PRESENT: u64 = 6;

/// Region GUIDs ([MS-VHDX] 2.3.1), little-endian on the wire.
const REGION_BAT: [u8; 16] = guid(
    0x2DC2_7766,
    0xF623,
    0x4200,
    [0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08],
);
const REGION_METADATA: [u8; 16] = guid(
    0x8B7C_A206,
    0x4790,
    0x4B9A,
    [0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E],
);

/// Metadata item GUIDs ([MS-VHDX] 2.6.2).
const META_FILE_PARAMETERS: [u8; 16] = guid(
    0xCAA1_6737,
    0xFA36,
    0x4D43,
    [0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B],
);
const META_VIRTUAL_DISK_SIZE: [u8; 16] = guid(
    0x2FA5_4224,
    0xCD1B,
    0x4876,
    [0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8],
);
const META_LOGICAL_SECTOR_SIZE: [u8; 16] = guid(
    0x8141_BF1D,
    0xA96F,
    0x4709,
    [0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F],
);
const META_PHYSICAL_SECTOR_SIZE: [u8; 16] = guid(
    0xCDA3_48C7,
    0x445D,
    0x4471,
    [0x9C, 0xC9, 0xE9, 0x88, 0x52, 0x51, 0xC5, 0x56],
);
const META_VIRTUAL_DISK_ID: [u8; 16] = guid(
    0xBECA_12AB,
    0xB2E6,
    0x4523,
    [0x93, 0xEF, 0xC3, 0x09, 0xE0, 0x00, 0xC7, 0x46],
);

/// Build a 16-byte GUID in the mixed-endian on-disk layout (first three fields
/// little-endian, trailing eight bytes as-is) at compile time.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5],
        d4[6], d4[7],
    ]
}

/// A synthesized VHDX container over a raw disk image.
///
/// Holds only the prologue — the payload is the caller's image, read through
/// whatever it already uses. See the module docs for the layout.
pub struct Vhdx {
    prologue: Vec<u8>,
    payload_start: u64,
    /// Total file size: prologue + payload rounded up to a whole block.
    file_size: u64,
    disk_size: u64,
}

impl Vhdx {
    /// Synthesize the container for a `disk_size`-byte image whose device reports
    /// `logical_sector_size`-byte sectors.
    pub fn new(disk_size: u64, logical_sector_size: u32, disk_id: [u8; 16]) -> Result<Self> {
        if !matches!(logical_sector_size, 512 | 4096) {
            return Err(PhoenixError::Other(format!(
                "VHDX logical sector size must be 512 or 4096, got {logical_sector_size}"
            )));
        }
        if disk_size == 0 || !disk_size.is_multiple_of(logical_sector_size as u64) {
            return Err(PhoenixError::Other(format!(
                "VHDX virtual disk size {disk_size} must be a non-zero multiple of the \
                 {logical_sector_size}-byte logical sector size"
            )));
        }

        let block_count = disk_size.div_ceil(BLOCK_SIZE);

        // The BAT interleaves one sector-bitmap entry after every `chunk_ratio`
        // payload entries. Our payload blocks are all FULLY_PRESENT, so the
        // bitmap blocks are never consulted and their entries stay zero
        // (SB_BLOCK_NOT_PRESENT) — but the slots must still be there, or every
        // BAT index past the first chunk is off by one and Windows reads the
        // wrong block for every subsequent megabyte of the disk.
        let chunk_ratio = (1u64 << 23) * logical_sector_size as u64 / BLOCK_SIZE;
        let bitmap_entries = block_count.div_ceil(chunk_ratio);
        let bat_entries = block_count + bitmap_entries;

        let bat_length = align_up(bat_entries * 8, ALIGN).max(ALIGN);
        let payload_start = align_up(BAT_OFFSET + bat_length, ALIGN);
        let file_size = payload_start + block_count * BLOCK_SIZE;

        let mut v = Self {
            prologue: Vec::new(),
            payload_start,
            file_size,
            disk_size,
        };
        v.prologue = v.build_prologue(
            disk_size,
            logical_sector_size,
            disk_id,
            block_count,
            chunk_ratio,
            bat_length,
        );
        debug_assert_eq!(v.prologue.len() as u64, payload_start);
        Ok(v)
    }

    /// File offset at which the raw disk image begins. Image offset `i` lives at
    /// file offset `payload_start + i`.
    pub fn payload_start(&self) -> u64 {
        self.payload_start
    }

    /// Total size of the synthesized `.vhdx` file.
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Serve `buf.len()` bytes of the container at `offset`, delegating any part
    /// that falls in the payload to `read_image`, which is handed **image**
    /// (not file) offsets.
    ///
    /// The tail padding between `disk_size` and the end of the final block reads
    /// as zeros: it is inside the file but outside the virtual disk, so nothing
    /// should ever ask for it, and zeros are the only honest answer if it does.
    pub fn read_at<F>(&self, offset: u64, buf: &mut [u8], mut read_image: F) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            if pos >= self.file_size {
                buf[done..].fill(0);
                break;
            }
            let want = buf.len() - done;

            if pos < self.payload_start {
                let n = want.min((self.payload_start - pos) as usize);
                buf[done..done + n].copy_from_slice(&self.prologue[pos as usize..pos as usize + n]);
                done += n;
                continue;
            }

            let image_off = pos - self.payload_start;
            if image_off >= self.disk_size {
                buf[done..].fill(0);
                break;
            }
            let n = want.min((self.disk_size - image_off) as usize);
            read_image(image_off, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_prologue(
        &self,
        disk_size: u64,
        logical_sector_size: u32,
        disk_id: [u8; 16],
        block_count: u64,
        chunk_ratio: u64,
        bat_length: u64,
    ) -> Vec<u8> {
        let mut out = vec![0u8; self.payload_start as usize];

        // --- File Type Identifier -------------------------------------------
        out[0..8].copy_from_slice(b"vhdxfile");
        // Creator, UTF-16LE, 512 bytes. Purely informational.
        for (i, c) in "carbon-phoenix".encode_utf16().enumerate() {
            let at = 8 + i * 2;
            out[at..at + 2].copy_from_slice(&c.to_le_bytes());
        }

        // --- Headers ---------------------------------------------------------
        // Two copies. Windows picks the one with the greater SequenceNumber whose
        // CRC-32C validates, so both are written valid and header 1 wins.
        let h1 = build_header(2, LOG_OFFSET, LOG_LENGTH);
        let h2 = build_header(1, LOG_OFFSET, LOG_LENGTH);
        out[HEADER_1_OFFSET..HEADER_1_OFFSET + h1.len()].copy_from_slice(&h1);
        out[HEADER_2_OFFSET..HEADER_2_OFFSET + h2.len()].copy_from_slice(&h2);

        // --- Region tables ---------------------------------------------------
        let rt = build_region_table(bat_length);
        out[REGION_TABLE_1_OFFSET..REGION_TABLE_1_OFFSET + rt.len()].copy_from_slice(&rt);
        out[REGION_TABLE_2_OFFSET..REGION_TABLE_2_OFFSET + rt.len()].copy_from_slice(&rt);

        // --- Log region ------------------------------------------------------
        // Left as zeros. The header's LogGuid is all-zero, which is precisely how
        // a VHDX says "the log is empty; there is nothing to replay".

        // --- Metadata region -------------------------------------------------
        let meta = build_metadata(disk_size, logical_sector_size, disk_id);
        let m = METADATA_OFFSET as usize;
        out[m..m + meta.len()].copy_from_slice(&meta);

        // --- Block Allocation Table ------------------------------------------
        let bat = build_bat(block_count, chunk_ratio, self.payload_start);
        let b = BAT_OFFSET as usize;
        out[b..b + bat.len()].copy_from_slice(&bat);

        out
    }
}

/// VHDX header ([MS-VHDX] 2.2). 4 KiB, checksummed with CRC-32C over the whole
/// structure with the checksum field zeroed.
fn build_header(sequence: u64, log_offset: u64, log_length: u64) -> Vec<u8> {
    let mut h = vec![0u8; 4 * KIB];
    h[0..4].copy_from_slice(b"head");
    // 4..8 = Checksum, filled in below.
    h[8..16].copy_from_slice(&sequence.to_le_bytes());
    // FileWriteGuid / DataWriteGuid: identify the writer. Any value is legal for a
    // file we only ever read; a fixed one keeps the synthesis deterministic, which
    // matters because the byte-level tests hash it.
    h[16..32].copy_from_slice(&PHOENIX_WRITE_GUID);
    h[32..48].copy_from_slice(&PHOENIX_WRITE_GUID);
    // 48..64 LogGuid — ZERO. This is the whole reason we can skip implementing the
    // log: a zero LogGuid means the log is empty and needs no replay.
    // 64..66 LogVersion — must be 0.
    h[66..68].copy_from_slice(&1u16.to_le_bytes()); // Version = 1
    h[68..72].copy_from_slice(&(log_length as u32).to_le_bytes());
    h[72..80].copy_from_slice(&log_offset.to_le_bytes());

    let crc = crc32c(&h);
    h[4..8].copy_from_slice(&crc.to_le_bytes());
    h
}

const PHOENIX_WRITE_GUID: [u8; 16] = guid(
    0x9C0F_1E5B,
    0x7A31,
    0x4C2D,
    [0xB6, 0x4A, 0x50, 0x48, 0x4F, 0x45, 0x4E, 0x58],
);

/// Region table ([MS-VHDX] 2.3). 64 KiB: a 16-byte header then 32-byte entries.
fn build_region_table(bat_length: u64) -> Vec<u8> {
    let mut t = vec![0u8; 64 * KIB];
    t[0..4].copy_from_slice(b"regi");
    // 4..8 Checksum, below.
    t[8..12].copy_from_slice(&2u32.to_le_bytes()); // EntryCount: BAT + Metadata
                                                   // 12..16 Reserved.

    let mut put = |i: usize, id: &[u8; 16], off: u64, len: u64| {
        let e = 16 + i * 32;
        t[e..e + 16].copy_from_slice(id);
        t[e + 16..e + 24].copy_from_slice(&off.to_le_bytes());
        t[e + 24..e + 28].copy_from_slice(&(len as u32).to_le_bytes());
        // Required = 1: refuse the file rather than open it without a region we
        // consider essential. Both of ours are.
        t[e + 28..e + 32].copy_from_slice(&1u32.to_le_bytes());
    };
    put(0, &REGION_BAT, BAT_OFFSET, bat_length);
    put(1, &REGION_METADATA, METADATA_OFFSET, METADATA_LENGTH);

    let crc = crc32c(&t);
    t[4..8].copy_from_slice(&crc.to_le_bytes());
    t
}

/// Metadata region ([MS-VHDX] 2.6): a table header, five entries, then the item
/// values. This is where `LogicalSectorSize` lives — the entire reason for VHDX.
fn build_metadata(disk_size: u64, logical_sector_size: u32, disk_id: [u8; 16]) -> Vec<u8> {
    let mut m = vec![0u8; METADATA_LENGTH as usize];
    m[0..8].copy_from_slice(b"metadata");
    // 8..10 Reserved.
    m[10..12].copy_from_slice(&5u16.to_le_bytes()); // EntryCount
                                                    // 12..32 Reserved2.

    // Item values are placed after the 64 KiB table, per the spec's requirement
    // that offsets be >= 64 KiB and relative to the start of the region.
    let mut value_at: u32 = 64 * KIB as u32;
    let mut entry = 0usize;

    // Metadata entry flags: bit0 IsUser, bit1 IsVirtualDisk, bit2 IsRequired.
    //
    // `is_virtual_disk` is NOT decoration. `FileParameters` describes the *file*
    // (its block size, whether blocks are preallocated) — not the virtual disk —
    // so its IsVirtualDisk bit must be **0**, while the other four items are
    // virtual-disk metadata and set it. Windows validates this, and setting it on
    // FileParameters is exactly what made `OpenVirtualDisk` reject an otherwise
    // perfect container with ERROR_FILE_CORRUPT (1392) and no further explanation.
    // Verified against the metadata table of a VHDX Windows built itself.
    let mut put = |m: &mut Vec<u8>, id: &[u8; 16], value: &[u8], is_virtual_disk: bool| {
        let e = 32 + entry * 32;
        m[e..e + 16].copy_from_slice(id);
        m[e + 16..e + 20].copy_from_slice(&value_at.to_le_bytes());
        m[e + 20..e + 24].copy_from_slice(&(value.len() as u32).to_le_bytes());
        let flags: u32 = 0b100 | if is_virtual_disk { 0b010 } else { 0 };
        m[e + 24..e + 28].copy_from_slice(&flags.to_le_bytes());
        let at = value_at as usize;
        m[at..at + value.len()].copy_from_slice(value);
        value_at += value.len() as u32;
        entry += 1;
    };

    // FileParameters: BlockSize, then flags. LeaveBlocksAllocated (bit 0) is set —
    // our payload blocks really are all present and contiguous, which is exactly
    // what that flag asserts. HasParent (bit 1) stays clear: no differencing chain.
    let mut file_params = [0u8; 8];
    file_params[0..4].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    file_params[4..8].copy_from_slice(&1u32.to_le_bytes());
    put(&mut m, &META_FILE_PARAMETERS, &file_params, false);

    put(
        &mut m,
        &META_VIRTUAL_DISK_SIZE,
        &disk_size.to_le_bytes(),
        true,
    );
    put(
        &mut m,
        &META_LOGICAL_SECTOR_SIZE,
        &logical_sector_size.to_le_bytes(),
        true,
    );
    // Physical sector size: report 4096 always. It is advisory (alignment hint),
    // and 4096 is true of essentially all modern media, including the 512e disks
    // this path also serves.
    put(
        &mut m,
        &META_PHYSICAL_SECTOR_SIZE,
        &4096u32.to_le_bytes(),
        true,
    );
    put(&mut m, &META_VIRTUAL_DISK_ID, &disk_id, true);

    m
}

/// Block Allocation Table ([MS-VHDX] 2.5): one 64-bit entry per payload block,
/// with a sector-bitmap entry interleaved after every `chunk_ratio` of them.
///
/// Entry layout: bits 0..2 state, bits 20..63 the block's file offset in MiB.
fn build_bat(block_count: u64, chunk_ratio: u64, payload_start: u64) -> Vec<u8> {
    let bitmap_entries = block_count.div_ceil(chunk_ratio);
    let mut bat = Vec::with_capacity(((block_count + bitmap_entries) * 8) as usize);

    for block in 0..block_count {
        let file_off = payload_start + block * BLOCK_SIZE;
        debug_assert!(
            file_off.is_multiple_of(ALIGN),
            "payload blocks must be 1 MiB aligned"
        );
        let entry = ((file_off / MIB) << 20) | PAYLOAD_BLOCK_FULLY_PRESENT;
        bat.extend_from_slice(&entry.to_le_bytes());

        // After the last payload block of each chunk, the sector-bitmap slot.
        // Left as SB_BLOCK_NOT_PRESENT (all zero): a FULLY_PRESENT payload block
        // is entirely valid data, so its bitmap is never read. The slot must exist
        // regardless — skip it and every later block index is off by one.
        if (block + 1).is_multiple_of(chunk_ratio) || block + 1 == block_count {
            bat.extend_from_slice(&0u64.to_le_bytes());
        }
    }
    bat
}

fn align_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vhdx_4kn(disk_size: u64) -> Vhdx {
        Vhdx::new(disk_size, 4096, [0x11; 16]).expect("build vhdx")
    }

    #[test]
    fn rejects_a_sector_size_the_format_cannot_express() {
        assert!(Vhdx::new(1024 * MIB, 1024, [0; 16]).is_err());
        assert!(Vhdx::new(1024 * MIB, 0, [0; 16]).is_err());
    }

    /// A virtual disk that isn't a whole number of sectors is not a disk.
    #[test]
    fn rejects_a_disk_size_that_is_not_a_sector_multiple() {
        assert!(Vhdx::new(4096 * 10 + 1, 4096, [0; 16]).is_err());
        assert!(Vhdx::new(0, 4096, [0; 16]).is_err());
    }

    #[test]
    fn file_identifier_and_headers_are_where_windows_looks() {
        let v = vhdx_4kn(64 * MIB);
        assert_eq!(&v.prologue[0..8], b"vhdxfile");
        assert_eq!(&v.prologue[HEADER_1_OFFSET..HEADER_1_OFFSET + 4], b"head");
        assert_eq!(&v.prologue[HEADER_2_OFFSET..HEADER_2_OFFSET + 4], b"head");
        assert_eq!(
            &v.prologue[REGION_TABLE_1_OFFSET..REGION_TABLE_1_OFFSET + 4],
            b"regi"
        );
        assert_eq!(
            &v.prologue[REGION_TABLE_2_OFFSET..REGION_TABLE_2_OFFSET + 4],
            b"regi"
        );
        assert_eq!(
            &v.prologue[METADATA_OFFSET as usize..METADATA_OFFSET as usize + 8],
            b"metadata"
        );
    }

    /// Both headers and both region tables must carry a valid CRC-32C computed
    /// over the structure with the checksum field zeroed. Windows silently refuses
    /// the file otherwise, so verify it the way Windows will.
    #[test]
    fn checksums_validate_the_way_windows_checks_them() {
        let v = vhdx_4kn(64 * MIB);

        for &(off, len) in &[
            (HEADER_1_OFFSET, 4 * KIB),
            (HEADER_2_OFFSET, 4 * KIB),
            (REGION_TABLE_1_OFFSET, 64 * KIB),
            (REGION_TABLE_2_OFFSET, 64 * KIB),
        ] {
            let mut copy = v.prologue[off..off + len].to_vec();
            let stored = u32::from_le_bytes(copy[4..8].try_into().unwrap());
            copy[4..8].fill(0);
            assert_eq!(
                crc32c(&copy),
                stored,
                "structure at 0x{off:X} has a bad CRC-32C — Windows would reject the file"
            );
            assert_ne!(stored, 0, "a zero checksum means we forgot to write one");
        }
    }

    /// The entire reason this module exists.
    #[test]
    fn metadata_states_the_logical_sector_size() {
        for &ss in &[512u32, 4096u32] {
            let v = Vhdx::new(64 * MIB, ss, [0x22; 16]).unwrap();
            let meta = &v.prologue[METADATA_OFFSET as usize..];

            // Walk the metadata table looking for the logical-sector-size item,
            // exactly as a reader would, rather than trusting a fixed offset.
            let count = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;
            let mut found = None;
            for i in 0..count {
                let e = 32 + i * 32;
                if meta[e..e + 16] == META_LOGICAL_SECTOR_SIZE {
                    let off = u32::from_le_bytes(meta[e + 16..e + 20].try_into().unwrap()) as usize;
                    let len = u32::from_le_bytes(meta[e + 20..e + 24].try_into().unwrap()) as usize;
                    assert_eq!(len, 4);
                    found = Some(u32::from_le_bytes(meta[off..off + 4].try_into().unwrap()));
                }
            }
            assert_eq!(found, Some(ss), "LogicalSectorSize metadata is wrong");
        }
    }

    /// `FileParameters` must NOT carry the IsVirtualDisk bit; the other four items
    /// must.
    ///
    /// This exact bit is what made Windows reject the first version of this
    /// container with ERROR_FILE_CORRUPT and no further detail. The flags are
    /// pinned against the metadata table of a VHDX Windows generated itself.
    #[test]
    fn file_parameters_is_file_metadata_not_virtual_disk_metadata() {
        let v = vhdx_4kn(64 * MIB);
        let meta = &v.prologue[METADATA_OFFSET as usize..];
        let count = u16::from_le_bytes(meta[10..12].try_into().unwrap()) as usize;

        const IS_VIRTUAL_DISK: u32 = 0b010;
        const IS_REQUIRED: u32 = 0b100;

        let mut seen = 0;
        for i in 0..count {
            let e = 32 + i * 32;
            let id: [u8; 16] = meta[e..e + 16].try_into().unwrap();
            let flags = u32::from_le_bytes(meta[e + 24..e + 28].try_into().unwrap());
            assert_eq!(flags & IS_REQUIRED, IS_REQUIRED, "every item is required");
            if id == META_FILE_PARAMETERS {
                assert_eq!(
                    flags & IS_VIRTUAL_DISK,
                    0,
                    "FileParameters describes the FILE, not the virtual disk — setting \
                     IsVirtualDisk here makes Windows reject the whole container"
                );
                seen += 1;
            } else {
                assert_eq!(
                    flags & IS_VIRTUAL_DISK,
                    IS_VIRTUAL_DISK,
                    "item {i} is virtual-disk metadata and must say so"
                );
                seen += 1;
            }
        }
        assert_eq!(seen, 5, "all five required metadata items must be present");
    }

    /// Every payload block must be FULLY_PRESENT, 1 MiB-aligned, and in order —
    /// that contiguity is the property that lets the chunk store stay unaware of
    /// VHDX entirely.
    #[test]
    fn bat_maps_blocks_contiguously_and_in_order() {
        let disk = 64 * MIB;
        let v = vhdx_4kn(disk);
        let blocks = disk.div_ceil(BLOCK_SIZE);
        let chunk_ratio = (1u64 << 23) * 4096 / BLOCK_SIZE;

        let bat = &v.prologue[BAT_OFFSET as usize..];
        let mut idx = 0usize;
        for block in 0..blocks {
            let e = u64::from_le_bytes(bat[idx * 8..idx * 8 + 8].try_into().unwrap());
            assert_eq!(e & 0x7, PAYLOAD_BLOCK_FULLY_PRESENT, "block {block}");
            let file_off = (e >> 20) * MIB;
            assert_eq!(
                file_off,
                v.payload_start() + block * BLOCK_SIZE,
                "block {block} is not where the payload actually is"
            );
            idx += 1;
            if (block + 1).is_multiple_of(chunk_ratio) || block + 1 == blocks {
                idx += 1; // the sector-bitmap slot
            }
        }
    }

    /// Reads spanning the prologue/payload boundary must stitch correctly — this
    /// is the seam every mounted byte crosses.
    #[test]
    fn reads_stitch_prologue_and_payload() {
        let disk = 8 * MIB;
        let v = vhdx_4kn(disk);
        // Image byte i == (i % 251), a pattern no prologue field will accidentally match.
        let image = |off: u64, buf: &mut [u8]| -> Result<()> {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((off + i as u64) % 251) as u8;
            }
            Ok(())
        };

        // Straddle the boundary: last 16 bytes of prologue + first 16 of payload.
        let start = v.payload_start() - 16;
        let mut buf = [0u8; 32];
        v.read_at(start, &mut buf, image).unwrap();
        assert_eq!(&buf[0..16], &v.prologue[start as usize..][..16]);
        for i in 0..16u64 {
            assert_eq!(buf[16 + i as usize], (i % 251) as u8, "payload byte {i}");
        }

        // The tail past the virtual disk but inside the final block reads as zeros.
        let mut tail = [0xAAu8; 16];
        v.read_at(v.payload_start() + disk, &mut tail, image)
            .unwrap();
        assert!(tail.iter().all(|&b| b == 0), "padding must read as zeros");

        // ...and so does everything past the end of the file.
        let mut past = [0xAAu8; 16];
        v.read_at(v.file_size() + 4096, &mut past, image).unwrap();
        assert!(past.iter().all(|&b| b == 0));
    }
}
