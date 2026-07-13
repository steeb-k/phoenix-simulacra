//! Synthesize a GPT (protective MBR + primary/backup headers + entry array)
//! describing the partition layout of a mounted backup. The backup stores
//! partitions but not the original disk's partition table, so we build one so
//! the attached virtual disk presents its volumes to Windows.

use phoenix_core::hash::crc32;

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_HEADER_SIZE: u32 = 92;
const NUM_ENTRIES: u32 = 128;
const ENTRY_SIZE: u32 = 128;

/// The entry array is 128 x 128 bytes = 16 KiB, *regardless of sector size* —
/// GPT fixes it in bytes, not LBAs.
const ENTRY_ARRAY_BYTES: u64 = (NUM_ENTRIES * ENTRY_SIZE) as u64;

/// Sectors the entry array occupies: **32** on a 512-byte disk, **4** on a 4Kn
/// one. This is the crux of GPT-on-4Kn, and the reason the old constants below
/// could not simply be reused.
pub fn entry_array_sectors(sector_size: u64) -> u64 {
    ENTRY_ARRAY_BYTES.div_ceil(sector_size)
}

/// LBA 0 (protective MBR) + LBA 1 (primary header) + the entry array.
///
/// 34 on a 512-byte disk (the number everyone memorizes), but **6** on a 4Kn one
/// — the entry array is 16 KiB either way, which is 32 sectors of 512 and only 4
/// of 4096. Hardcoding 34 on a 4Kn disk reserves 8x more space than the GPT needs
/// and, worse, puts `first_usable_lba` somewhere Windows does not expect it.
pub fn leading_sectors(sector_size: u64) -> u64 {
    2 + entry_array_sectors(sector_size)
}

/// The backup entry array + the backup header at the very last LBA.
pub fn trailing_sectors(sector_size: u64) -> u64 {
    1 + entry_array_sectors(sector_size)
}

/// Basic Data partition type GUID (EBD0A0A2-B9E5-4433-87C0-68B6B72699C7) in
/// on-disk byte order. Backups of MBR disks record an all-zero type GUID
/// (MBR has type bytes, not GUIDs), but a zero type GUID marks a GPT entry
/// as UNUSED — Windows would see an empty table and surface no volumes. Fall
/// back to Basic Data instead, mirroring the restore path's
/// `BASIC_DATA_PARTITION_TYPE` fallback.
pub const BASIC_DATA_TYPE_GUID: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

/// One partition to describe in the GPT.
#[derive(Debug, Clone)]
pub struct GptPart {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

/// Byte regions to place on the synthesized disk.
pub struct GptImage {
    /// The first 34 sectors (protective MBR + primary header + entry array).
    pub leading: Vec<u8>,
    /// The last 33 sectors (backup entry array + backup header).
    pub trailing: Vec<u8>,
}

/// The protective MBR. Its *structure* is 512 bytes by definition (that is what
/// an MBR is), but it occupies a whole sector, so on a 4Kn disk it is padded out
/// to 4096. The 0x55AA signature stays at byte 510 of the structure either way.
fn protective_mbr(total_sectors: u64, sector_size: usize) -> Vec<u8> {
    let mut mbr = vec![0u8; sector_size];
    // Single partition entry at offset 446, type 0xEE spanning the disk.
    let e = 446;
    mbr[e] = 0x00; // boot indicator
    mbr[e + 1] = 0x00; // starting CHS (head)
    mbr[e + 2] = 0x02; // starting CHS
    mbr[e + 3] = 0x00;
    mbr[e + 4] = 0xEE; // type: GPT protective
    mbr[e + 5] = 0xFF; // ending CHS
    mbr[e + 6] = 0xFF;
    mbr[e + 7] = 0xFF;
    mbr[e + 8..e + 12].copy_from_slice(&1u32.to_le_bytes()); // starting LBA
    let size = (total_sectors - 1).min(0xFFFF_FFFF) as u32;
    mbr[e + 12..e + 16].copy_from_slice(&size.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

fn entry_array(parts: &[GptPart]) -> Vec<u8> {
    let mut arr = vec![0u8; (NUM_ENTRIES * ENTRY_SIZE) as usize];
    for (i, p) in parts.iter().enumerate() {
        if i >= NUM_ENTRIES as usize {
            break;
        }
        let base = i * ENTRY_SIZE as usize;
        arr[base..base + 16].copy_from_slice(&p.type_guid);
        arr[base + 16..base + 32].copy_from_slice(&p.unique_guid);
        arr[base + 32..base + 40].copy_from_slice(&p.first_lba.to_le_bytes());
        arr[base + 40..base + 48].copy_from_slice(&p.last_lba.to_le_bytes());
        arr[base + 48..base + 56].copy_from_slice(&p.attributes.to_le_bytes());
        // Name: UTF-16LE, up to 36 code units.
        let name: Vec<u16> = p.name.encode_utf16().take(35).collect();
        for (j, c) in name.iter().enumerate() {
            let o = base + 56 + j * 2;
            arr[o..o + 2].copy_from_slice(&c.to_le_bytes());
        }
    }
    arr
}

/// The GPT header. The structure is 92 bytes; it occupies one whole sector, so
/// the padding — and only the padding — depends on the sector size. The header
/// CRC covers just the 92 bytes, never the padding.
#[allow(clippy::too_many_arguments)]
fn header(
    my_lba: u64,
    alt_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: [u8; 16],
    entry_array_lba: u64,
    entries_crc: u32,
    sector_size: usize,
) -> Vec<u8> {
    let mut h = vec![0u8; sector_size];
    h[0..8].copy_from_slice(GPT_SIGNATURE);
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    h[12..16].copy_from_slice(&GPT_HEADER_SIZE.to_le_bytes());
    // header CRC (16..20) computed last
    // reserved (20..24) = 0
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
    h[40..48].copy_from_slice(&first_usable.to_le_bytes());
    h[48..56].copy_from_slice(&last_usable.to_le_bytes());
    h[56..72].copy_from_slice(&disk_guid);
    h[72..80].copy_from_slice(&entry_array_lba.to_le_bytes());
    h[80..84].copy_from_slice(&NUM_ENTRIES.to_le_bytes());
    h[84..88].copy_from_slice(&ENTRY_SIZE.to_le_bytes());
    h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    // Header CRC is over the first GPT_HEADER_SIZE bytes with the CRC field
    // (16..20) zeroed.
    let crc = crc32(&h[0..GPT_HEADER_SIZE as usize]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Build the GPT leading + trailing regions for a disk of `disk_size` bytes with
/// `sector_size`-byte logical sectors, carrying `parts`.
///
/// Every LBA here is a *sector* index, so all of it scales with `sector_size` —
/// which is exactly why a 512-only version of this function could never describe
/// a 4Kn disk, no matter how correct its bytes were.
pub fn synthesize(
    disk_size: u64,
    disk_guid: [u8; 16],
    parts: &[GptPart],
    sector_size: u64,
) -> GptImage {
    let ss = sector_size as usize;
    let total_sectors = disk_size / sector_size;
    let last_lba = total_sectors - 1;
    let lead = leading_sectors(sector_size);
    let trail = trailing_sectors(sector_size);
    let first_usable = lead;
    let last_usable = last_lba - trail;

    let arr = entry_array(parts);
    let entries_crc = crc32(&arr);

    // Primary header at LBA 1, entry array at LBA 2, backup header at the last LBA.
    let primary = header(
        1,
        last_lba,
        first_usable,
        last_usable,
        disk_guid,
        2,
        entries_crc,
        ss,
    );
    let backup_entries_lba = last_lba - trail + 1;
    let backup = header(
        last_lba,
        1,
        first_usable,
        last_usable,
        disk_guid,
        backup_entries_lba,
        entries_crc,
        ss,
    );

    // Leading: MBR + primary header + entry array, padded to whole sectors.
    let mut leading = Vec::with_capacity(lead as usize * ss);
    leading.extend_from_slice(&protective_mbr(total_sectors, ss));
    leading.extend_from_slice(&primary);
    leading.extend_from_slice(&arr);
    // The entry array is 16 KiB, which is a whole number of sectors at 512 and at
    // 4096 alike — but pad defensively rather than assume it of some future size.
    leading.resize(lead as usize * ss, 0);

    // Trailing: backup entry array + backup header at the very last LBA.
    let mut trailing = Vec::with_capacity(trail as usize * ss);
    trailing.extend_from_slice(&arr);
    trailing.resize((trail as usize - 1) * ss, 0);
    trailing.extend_from_slice(&backup);
    debug_assert_eq!(trailing.len(), trail as usize * ss);

    GptImage { leading, trailing }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(first: u64, last: u64) -> GptPart {
        GptPart {
            type_guid: [0xAA; 16],
            unique_guid: [0xBB; 16],
            first_lba: first,
            last_lba: last,
            attributes: 0,
            name: "Data".into(),
        }
    }

    /// The structural checks, run at BOTH sector sizes. Every LBA in a GPT is a
    /// sector index, so this is the whole 4Kn story in one test: the same
    /// structures, in different places.
    #[test]
    fn synthesized_gpt_has_valid_structure_at_512_and_4096() {
        for ss in [512u64, 4096u64] {
            let s = ss as usize;
            let disk = 128 * 1024 * 1024u64;
            let img = synthesize(disk, [0xCC; 16], &[part(2048, 4096)], ss);

            // Protective MBR: a 512-byte structure, wherever the sector ends.
            assert_eq!(img.leading[510], 0x55, "ss={ss}");
            assert_eq!(img.leading[511], 0xAA, "ss={ss}");
            assert_eq!(img.leading[446 + 4], 0xEE, "ss={ss}");

            // Primary header at LBA 1 — which is byte 512 or byte 4096.
            assert_eq!(&img.leading[s..s + 8], GPT_SIGNATURE, "ss={ss}");

            // Header CRC verifies (recompute over the 92-byte struct, field zeroed).
            let mut hdr = img.leading[s..s + GPT_HEADER_SIZE as usize].to_vec();
            let stored = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
            hdr[16..20].fill(0);
            assert_eq!(stored, crc32(&hdr), "header CRC, ss={ss}");

            // The backup header is the LAST sector of the trailing region.
            let bh = &img.trailing[img.trailing.len() - s..];
            assert_eq!(&bh[0..8], GPT_SIGNATURE, "backup header, ss={ss}");

            // And the regions are exactly as many sectors as we reserved.
            assert_eq!(img.leading.len() as u64, leading_sectors(ss) * ss);
            assert_eq!(img.trailing.len() as u64, trailing_sectors(ss) * ss);
        }
    }

    /// The 16 KiB entry array is 32 sectors of 512 but only **4** of 4096, so the
    /// GPT reserve is 34/33 sectors on a 512 disk and 6/5 on a 4Kn one. Reusing
    /// the 512 numbers on 4Kn would put `first_usable_lba` 8x too far in.
    #[test]
    fn gpt_reserve_shrinks_in_sectors_on_a_4kn_disk() {
        assert_eq!(entry_array_sectors(512), 32);
        assert_eq!(leading_sectors(512), 34);
        assert_eq!(trailing_sectors(512), 33);

        assert_eq!(entry_array_sectors(4096), 4);
        assert_eq!(leading_sectors(4096), 6);
        assert_eq!(trailing_sectors(4096), 5);
    }

    /// `first_usable_lba` / `last_usable_lba` must be stated in the disk's own
    /// sectors — the field Windows uses to decide whether a partition is legal.
    #[test]
    fn usable_lbas_are_in_the_disks_own_sectors() {
        let disk = 128 * 1024 * 1024u64;
        for ss in [512u64, 4096u64] {
            let s = ss as usize;
            let img = synthesize(disk, [0xCC; 16], &[part(2048, 4096)], ss);
            let hdr = &img.leading[s..];
            let first_usable = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
            let last_usable = u64::from_le_bytes(hdr[48..56].try_into().unwrap());
            let last_lba = disk / ss - 1;

            assert_eq!(first_usable, leading_sectors(ss), "ss={ss}");
            assert_eq!(last_usable, last_lba - trailing_sectors(ss), "ss={ss}");
            assert!(last_usable < last_lba);
        }
    }
}
