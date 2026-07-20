//! Synthesize a real (non-protective) MBR for a backup of a BIOS/MBR disk.
//!
//! The GPT counterpart is [`crate::gpt`]. The two differ in more than layout:
//! a GPT is 34 sectors at the front and 33 at the back, while an MBR is
//! **exactly one sector** with nothing at the end of the disk — so a
//! synthesized MBR disk has no trailing region at all.
//!
//! Faithfulness matters here as much as it does for GPT identity. On a BIOS
//! install the BCD names the boot disk by its **NT disk signature** (the four
//! bytes at 0x1B8), so a disk synthesized with a fresh signature fails to boot
//! for the same reason a regenerated GPT disk GUID does. The signature and the
//! per-partition type byte and active flag all come from the manifest; they are
//! captured precisely because they cannot be reconstructed afterwards.
//!
//! Boot code is deliberately NOT synthesized. Bytes 0..440 stay zero: the
//! source disk's boot code lives outside every captured partition, so we never
//! had it. A guest that needs it gets the boot-repair path (`bootsect /nt60
//! /mbr`), the same answer as a restored disk.

/// Offset of the first partition entry in the MBR.
const FIRST_ENTRY: usize = 446;
/// Bytes per partition entry.
const ENTRY_SIZE: usize = 16;
/// An MBR holds four primary entries and no more.
pub const MAX_PARTITIONS: usize = 4;
/// Offset of the NT disk signature.
const SIGNATURE_OFFSET: usize = 0x1B8;

/// One partition to describe in the MBR. LBAs are in the disk's own sector
/// units, matching [`crate::gpt::GptPart`].
#[derive(Debug, Clone)]
pub struct MbrPart {
    pub first_lba: u64,
    pub sectors: u64,
    /// Partition type byte (0x07 NTFS/exFAT, 0x0C FAT32-LBA, 0x27 recovery…).
    pub partition_type: u8,
    /// The active flag. The MBR boot code chains into whichever primary
    /// carries it, so losing it means the disk does not boot.
    pub bootable: bool,
}

/// Fallback type byte when a backup has none recorded — pre-fidelity backups,
/// or a partition captured before MBR identity was preserved.
///
/// 0x07 (NTFS/exFAT/HPFS) rather than zero: a zero type byte marks an entry
/// **unused**, so the guest would see an empty table and no volumes at all —
/// the same trap as an all-zero GPT type GUID, which is why the GPT path falls
/// back to Basic Data.
pub const DEFAULT_TYPE: u8 = 0x07;

/// Build the single MBR sector. Padded to `sector_size` because it occupies a
/// whole sector, though the structure itself is 512 bytes by definition.
///
/// Entries beyond the fourth are dropped: MBR has room for four primaries, and
/// a backup carrying more came from an extended/logical layout this does not
/// attempt to reproduce.
pub fn synthesize(disk_signature: u32, parts: &[MbrPart], sector_size: usize) -> Vec<u8> {
    let mut mbr = vec![0u8; sector_size];
    mbr[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4].copy_from_slice(&disk_signature.to_le_bytes());

    for (i, p) in parts.iter().take(MAX_PARTITIONS).enumerate() {
        let base = FIRST_ENTRY + i * ENTRY_SIZE;
        mbr[base] = if p.bootable { 0x80 } else { 0x00 };
        // CHS fields are the "use LBA" sentinel rather than real geometry.
        // Everything that reads these disks is LBA-aware, and computing
        // honest CHS for a modern disk is impossible anyway — the geometry
        // does not fit in the field.
        mbr[base + 1..base + 4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        mbr[base + 4] = if p.partition_type == 0 {
            DEFAULT_TYPE
        } else {
            p.partition_type
        };
        mbr[base + 5..base + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        // Both fields saturate: an MBR cannot address beyond 2 TiB, and a
        // truncated value is a wrong partition rather than a refused one, so
        // clamp deliberately instead of wrapping silently.
        let start = p.first_lba.min(u32::MAX as u64) as u32;
        let count = p.sectors.min(u32::MAX as u64) as u32;
        mbr[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
        mbr[base + 12..base + 16].copy_from_slice(&count.to_le_bytes());
    }

    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(first_lba: u64, sectors: u64, partition_type: u8, bootable: bool) -> MbrPart {
        MbrPart {
            first_lba,
            sectors,
            partition_type,
            bootable,
        }
    }

    #[test]
    fn synthesized_mbr_has_valid_structure() {
        let parts = vec![part(2048, 1_048_576, 0x07, true), part(1_050_624, 4096, 0x27, false)];
        let mbr = synthesize(0xDEAD_BEEF, &parts, 512);

        assert_eq!(mbr.len(), 512);
        assert_eq!(&mbr[510..512], &[0x55, 0xAA]);
        // The identity the BCD names the boot disk by.
        assert_eq!(&mbr[0x1B8..0x1BC], &0xDEAD_BEEFu32.to_le_bytes());

        // First entry: active, NTFS, at 2048 for 1 MiB of sectors.
        assert_eq!(mbr[446], 0x80);
        assert_eq!(mbr[446 + 4], 0x07);
        assert_eq!(&mbr[446 + 8..446 + 12], &2048u32.to_le_bytes());
        assert_eq!(&mbr[446 + 12..446 + 16], &1_048_576u32.to_le_bytes());

        // Second: not active, recovery type.
        assert_eq!(mbr[462], 0x00);
        assert_eq!(mbr[462 + 4], 0x27);
        assert_eq!(&mbr[462 + 8..462 + 12], &1_050_624u32.to_le_bytes());

        // Unused entries stay zeroed.
        assert!(mbr[478..510].iter().all(|&b| b == 0));
    }

    #[test]
    fn boot_code_region_is_left_empty() {
        // We never captured the source's boot code — it lives outside every
        // partition — so claiming to synthesize it would be a lie. Boot repair
        // is the answer, exactly as it is for a restored disk.
        let mbr = synthesize(1, &[part(2048, 4096, 0x07, true)], 512);
        assert!(mbr[..440].iter().all(|&b| b == 0));
    }

    #[test]
    fn zero_type_byte_falls_back_rather_than_marking_unused() {
        // A zero type byte means UNUSED, so a pre-fidelity backup would
        // otherwise synthesize a table the guest reads as empty.
        let mbr = synthesize(1, &[part(2048, 4096, 0x00, true)], 512);
        assert_eq!(mbr[446 + 4], DEFAULT_TYPE);
    }

    #[test]
    fn occupies_a_whole_sector_on_4kn() {
        let mbr = synthesize(1, &[part(256, 4096, 0x07, true)], 4096);
        assert_eq!(mbr.len(), 4096);
        // The structure stays 512 bytes wherever it sits.
        assert_eq!(&mbr[510..512], &[0x55, 0xAA]);
        assert!(mbr[512..].iter().all(|&b| b == 0));
    }

    #[test]
    fn extra_partitions_are_dropped_not_wrapped() {
        // Five entries would otherwise scribble past the table into the
        // boot signature.
        let parts: Vec<MbrPart> = (0..5)
            .map(|i| part(2048 + i * 4096, 4096, 0x07, false))
            .collect();
        let mbr = synthesize(1, &parts, 512);
        assert_eq!(&mbr[510..512], &[0x55, 0xAA]);
        // The fourth entry is the last one written.
        assert_eq!(&mbr[494 + 8..494 + 12], &(2048u32 + 3 * 4096).to_le_bytes());
    }
}
