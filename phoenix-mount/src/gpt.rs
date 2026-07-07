//! Synthesize a GPT (protective MBR + primary/backup headers + entry array)
//! describing the partition layout of a mounted backup. The backup stores
//! partitions but not the original disk's partition table, so we build one so
//! the attached virtual disk presents its volumes to Windows.

use phoenix_core::hash::crc32;

const SECTOR: usize = 512;
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_HEADER_SIZE: u32 = 92;
const NUM_ENTRIES: u32 = 128;
const ENTRY_SIZE: u32 = 128;
/// LBA 0 (MBR) + LBA 1 (header) + 32 LBAs of entries = 34 leading sectors.
pub const LEADING_SECTORS: u64 = 34;
/// 32 LBAs of backup entries + 1 backup header = 33 trailing sectors.
pub const TRAILING_SECTORS: u64 = 33;

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

fn protective_mbr(total_sectors: u64) -> [u8; SECTOR] {
    let mut mbr = [0u8; SECTOR];
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

#[allow(clippy::too_many_arguments)]
fn header(
    my_lba: u64,
    alt_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: [u8; 16],
    entry_array_lba: u64,
    entries_crc: u32,
) -> [u8; SECTOR] {
    let mut h = [0u8; SECTOR];
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

/// Build the GPT leading + trailing regions for a disk of `disk_size` bytes
/// carrying `parts`. `disk_guid` is the synthesized disk's identity.
pub fn synthesize(disk_size: u64, disk_guid: [u8; 16], parts: &[GptPart]) -> GptImage {
    let total_sectors = disk_size / SECTOR as u64;
    let last_lba = total_sectors - 1;
    let first_usable = LEADING_SECTORS;
    let last_usable = last_lba - TRAILING_SECTORS;

    let arr = entry_array(parts);
    let entries_crc = crc32(&arr);

    // Primary header at LBA 1, entries at LBA 2, backup at last LBA.
    let primary = header(
        1,
        last_lba,
        first_usable,
        last_usable,
        disk_guid,
        2,
        entries_crc,
    );
    let backup_entries_lba = last_lba - TRAILING_SECTORS + 1;
    let backup = header(
        last_lba,
        1,
        first_usable,
        last_usable,
        disk_guid,
        backup_entries_lba,
        entries_crc,
    );

    // Leading region: MBR + primary header + entry array (34 sectors).
    let mut leading = Vec::with_capacity(LEADING_SECTORS as usize * SECTOR);
    leading.extend_from_slice(&protective_mbr(total_sectors));
    leading.extend_from_slice(&primary);
    leading.extend_from_slice(&arr);
    debug_assert_eq!(leading.len(), LEADING_SECTORS as usize * SECTOR);

    // Trailing region: backup entry array + backup header (33 sectors).
    let mut trailing = Vec::with_capacity(TRAILING_SECTORS as usize * SECTOR);
    trailing.extend_from_slice(&arr);
    trailing.extend_from_slice(&backup);
    debug_assert_eq!(trailing.len(), TRAILING_SECTORS as usize * SECTOR);

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

    #[test]
    fn synthesized_gpt_has_valid_structure() {
        let disk = 128 * 1024 * 1024u64;
        let img = synthesize(disk, [0xCC; 16], &[part(34, 1000)]);
        // Protective MBR signature.
        assert_eq!(img.leading[510], 0x55);
        assert_eq!(img.leading[511], 0xAA);
        // MBR protective partition type.
        assert_eq!(img.leading[446 + 4], 0xEE);
        // Primary header signature at LBA 1.
        assert_eq!(&img.leading[SECTOR..SECTOR + 8], GPT_SIGNATURE);
        // Header CRC verifies (recompute with field zeroed).
        let mut hdr = [0u8; SECTOR];
        hdr.copy_from_slice(&img.leading[SECTOR..2 * SECTOR]);
        let stored = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
        hdr[16..20].fill(0);
        assert_eq!(stored, crc32(&hdr[0..GPT_HEADER_SIZE as usize]));
        // Backup header signature is the last sector of the trailing region.
        let bh = &img.trailing[img.trailing.len() - SECTOR..];
        assert_eq!(&bh[0..8], GPT_SIGNATURE);
    }
}
