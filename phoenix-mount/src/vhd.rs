//! Fixed-format VHD footer synthesis.
//!
//! A fixed VHD is simply the raw disk image followed by a single 512-byte
//! footer (Microsoft "Virtual Hard Disk Image Format Specification", the
//! `conectix` footer). We synthesize that footer so a materialized disk image
//! can be attached read-only via the Windows virtual-disk API. Fixed VHD tops
//! out at ~2040 GiB; larger disks need VHDX (a documented follow-up).

/// Maximum addressable size of a fixed VHD (2040 GiB, the CHS limit).
pub const VHD_MAX_BYTES: u64 = 2040u64 * 1024 * 1024 * 1024;

const FOOTER_SIZE: usize = 512;
const VHD_COOKIE: &[u8; 8] = b"conectix";
const CREATOR_APP: &[u8; 4] = b"phnx";
// "Wi2k" — Windows creator-host-OS constant from the VHD spec.
const CREATOR_HOST_OS: u32 = 0x5769_326B;
const DISK_TYPE_FIXED: u32 = 2;

/// Compute VHD CHS geometry for `total_sectors` (512-byte sectors), following
/// the algorithm in the VHD spec's "CHS Calculation" appendix verbatim.
fn chs(total_sectors: u64) -> (u16, u8, u8) {
    let ts = total_sectors.min(65535 * 16 * 255);
    let mut sectors_per_track: u64;
    let mut heads: u64;
    let mut cylinder_times_heads: u64;

    if ts >= 65535 * 16 * 63 {
        sectors_per_track = 255;
        heads = 16;
        cylinder_times_heads = ts / sectors_per_track;
    } else {
        sectors_per_track = 17;
        cylinder_times_heads = ts / sectors_per_track;
        heads = cylinder_times_heads.div_ceil(1024).max(4);
        if cylinder_times_heads >= heads * 1024 || heads > 16 {
            sectors_per_track = 31;
            heads = 16;
            cylinder_times_heads = ts / sectors_per_track;
        }
        if cylinder_times_heads >= heads * 1024 {
            sectors_per_track = 63;
            heads = 16;
            cylinder_times_heads = ts / sectors_per_track;
        }
    }
    let cylinders = (cylinder_times_heads / heads).min(65535) as u16;
    (cylinders, heads as u8, sectors_per_track as u8)
}

/// One's-complement checksum over the footer with the checksum field zeroed.
fn footer_checksum(buf: &[u8; FOOTER_SIZE]) -> u32 {
    let mut sum: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        // Skip the 4-byte checksum field at offset 64.
        if (64..68).contains(&i) {
            continue;
        }
        sum = sum.wrapping_add(b as u32);
    }
    !sum
}

/// Build a fixed-VHD footer for a disk of `disk_size` bytes. `unique_id` is the
/// VHD's 16-byte identity (use the backup UUID's bytes for stability).
pub fn build_footer(disk_size: u64, unique_id: [u8; 16]) -> [u8; FOOTER_SIZE] {
    let mut f = [0u8; FOOTER_SIZE];
    let total_sectors = disk_size / 512;
    let (cyl, heads, spt) = chs(total_sectors);

    f[0..8].copy_from_slice(VHD_COOKIE);
    f[8..12].copy_from_slice(&0x0000_0002u32.to_be_bytes()); // features: reserved bit set
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // file format version 1.0
    f[16..24].copy_from_slice(&0xFFFF_FFFF_FFFF_FFFFu64.to_be_bytes()); // data offset (fixed)
    f[24..28].copy_from_slice(&0u32.to_be_bytes()); // timestamp (0 = 2000-01-01; avoids clock use)
    f[28..32].copy_from_slice(CREATOR_APP);
    f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // creator version
    f[36..40].copy_from_slice(&CREATOR_HOST_OS.to_be_bytes());
    f[40..48].copy_from_slice(&disk_size.to_be_bytes()); // original size
    f[48..56].copy_from_slice(&disk_size.to_be_bytes()); // current size
    f[56..58].copy_from_slice(&cyl.to_be_bytes());
    f[58] = heads;
    f[59] = spt;
    f[60..64].copy_from_slice(&DISK_TYPE_FIXED.to_be_bytes());
    // checksum at 64..68 left zero for now
    f[68..84].copy_from_slice(&unique_id);
    f[84] = 0; // saved state
    let ck = footer_checksum(&f);
    f[64..68].copy_from_slice(&ck.to_be_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_has_expected_fields() {
        let f = build_footer(64 * 1024 * 1024, [0xABu8; 16]);
        assert_eq!(&f[0..8], VHD_COOKIE);
        // Disk type = fixed (2).
        assert_eq!(
            u32::from_be_bytes(f[60..64].try_into().unwrap()),
            DISK_TYPE_FIXED
        );
        // Current size matches.
        assert_eq!(
            u64::from_be_bytes(f[48..56].try_into().unwrap()),
            64 * 1024 * 1024
        );
    }

    #[test]
    fn checksum_validates() {
        let f = build_footer(128 * 1024 * 1024, [1u8; 16]);
        let stored = u32::from_be_bytes(f[64..68].try_into().unwrap());
        // Recompute with the field excluded and compare.
        assert_eq!(stored, footer_checksum(&f));
        // A one's-complement checksum: sum of all bytes (incl. checksum) is
        // all-ones (0xFFFFFFFF) modulo the excluded field — sanity check that
        // it's non-trivial.
        assert_ne!(stored, 0);
    }
}
