//! 4Kn → 512e filesystem boot-sector conversion.
//!
//! When a volume captured from a 4096-byte-logical-sector (4Kn) disk is restored
//! or cloned onto a 512-byte-logical-sector (512e) disk, used-block capture
//! copies its boot sector verbatim — so it still advertises
//! `BytesPerSector = 4096`. Windows refuses to mount a filesystem whose recorded
//! sector size contradicts the device, and the volume comes up RAW. This module
//! rewrites the boot sector's sector-denominated BPB fields so the volume
//! describes itself in 512-byte sectors instead.
//!
//! **No file data moves.** Both NTFS and FAT are cluster-addressed: a cluster's
//! byte offset is `cluster_number × cluster_size_bytes`, and we preserve the
//! cluster size *in bytes* by scaling `SectorsPerCluster` by the same
//! `ratio = 4096/512 = 8` that we scale every other sector count by. Every
//! byte offset is therefore invariant, and NTFS's MFT-record fixup stride is
//! always 512 regardless of the volume's sector size, so MFT records need no
//! change either. Proven on hardware (2026-07-14) for NTFS and FAT32; the full
//! rationale and the exact field list are in docs/SECTOR-SIZE-CONVERSION.md.
//!
//! The converter **auto-detects** the filesystem from the just-landed boot
//! sector (NTFS magic, or a FAT32 BPB) rather than trusting a manifest kind —
//! the ESP is recorded as `Efi`/raw yet holds a FAT32 boot sector, so a
//! kind-driven dispatch would miss exactly the partition whose conversion makes
//! the disk bootable. exFAT, MSR/empty, BitLocker ciphertext, and any already-
//! 512e boot sector are left untouched.

use phoenix_core::error::{PhoenixError, Result};
use tracing::info;

use crate::raw::PartitionWriter;

/// The logical sector size a 4Kn volume is converted *to*.
const TARGET_SECTOR: u64 = 512;

/// What [`apply_sector_conversion`] did to one partition's boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertOutcome {
    /// An NTFS boot sector was rewritten to 512e (primary + backup).
    ConvertedNtfs,
    /// A FAT32 boot sector was rewritten to 512e (primary + backup).
    ConvertedFat32,
    /// Nothing to do: the landed boot sector isn't a 4Kn NTFS/FAT32 volume
    /// (exFAT, MSR/empty, BitLocker ciphertext, or already 512-byte-sector).
    Skipped,
}

impl ConvertOutcome {
    pub fn converted(self) -> bool {
        matches!(self, Self::ConvertedNtfs | Self::ConvertedFat32)
    }
}

/// Read the just-landed boot sector at partition-relative offset 0, and if it is
/// a 4Kn NTFS or FAT32 volume rewrite it (primary + backup boot sector) to
/// describe itself in 512-byte sectors.
///
/// `target_offset_bytes` is the partition's absolute start on the target disk,
/// used to recompute `HiddenSectors` (the partition's LBA) for the new geometry —
/// a full-disk restore may place the partition at a different offset than the
/// source, so it can't just be scaled from the old value.
///
/// The caller must only invoke this while the target disk is offline / the
/// volume is not mounted (the same window every raw restore write happens in),
/// and after the partition's data has been streamed.
pub fn apply_sector_conversion(
    writer: &mut PartitionWriter,
    target_offset_bytes: u64,
) -> Result<ConvertOutcome> {
    // Commit any buffered data writes before reading the boot sector back, so we
    // patch the bytes that actually landed (see `PartitionWriter::flush`).
    writer.flush()?;

    let mut boot = vec![0u8; 512];
    writer.read_at(0, &mut boot)?;

    if &boot[3..7] == b"NTFS" {
        return convert_ntfs(writer, &mut boot, target_offset_bytes);
    }
    if is_fat32(&boot) {
        return convert_fat32(writer, &mut boot, target_offset_bytes);
    }
    Ok(ConvertOutcome::Skipped)
}

/// Multiply a sector-denominated field by `ratio`, failing cleanly if the result
/// won't fit the field's on-disk width (`max`). v1 does not implement the huge-
/// cluster / oversized-field encodings, so an overflow is refused rather than
/// silently truncated. `field` names the field for the error message.
fn scale(value: u64, ratio: u64, max: u64, field: &str) -> Result<u64> {
    let scaled = value.checked_mul(ratio).ok_or_else(|| {
        PhoenixError::Other(format!(
            "sector-size conversion overflow scaling {field} ({value} × {ratio})"
        ))
    })?;
    if scaled > max {
        return Err(PhoenixError::Other(format!(
            "sector-size conversion cannot represent {field}: {value} × {ratio} = {scaled} \
             exceeds the field's maximum {max}. This volume's geometry (e.g. a very large \
             cluster size) is not convertible in this version."
        )));
    }
    Ok(scaled)
}

/// Derive the `ratio` (old logical sector size / 512) from a boot sector's
/// `BytesPerSector` field, or `None` when there is nothing to convert (already
/// 512e, or a bogus value). Errors on a non-512-multiple sector size.
fn conversion_ratio(old_bps: u16) -> Result<Option<u64>> {
    let old = old_bps as u64;
    if old <= TARGET_SECTOR {
        return Ok(None);
    }
    if !old.is_multiple_of(TARGET_SECTOR) {
        return Err(PhoenixError::Other(format!(
            "boot sector reports {old_bps} bytes/sector, which is not a multiple of 512; \
             cannot convert to 512e"
        )));
    }
    Ok(Some(old / TARGET_SECTOR))
}

fn hidden_sectors(target_offset_bytes: u64) -> Result<u32> {
    (target_offset_bytes / TARGET_SECTOR)
        .try_into()
        .map_err(|_| {
            PhoenixError::Other(format!(
                "partition offset {target_offset_bytes} in 512-byte sectors exceeds the 32-bit \
             HiddenSectors field"
            ))
        })
}

/// NTFS BPB field offsets (all within the first 512 bytes of the VBR).
mod ntfs_off {
    pub const BYTES_PER_SECTOR: usize = 0x0B; // u16
    pub const SECTORS_PER_CLUSTER: usize = 0x0D; // u8
    pub const HIDDEN_SECTORS: usize = 0x1C; // u32
    pub const TOTAL_SECTORS: usize = 0x28; // u64
}

fn convert_ntfs(
    writer: &mut PartitionWriter,
    boot: &mut [u8],
    target_offset_bytes: u64,
) -> Result<ConvertOutcome> {
    let old_bps = u16::from_le_bytes([
        boot[ntfs_off::BYTES_PER_SECTOR],
        boot[ntfs_off::BYTES_PER_SECTOR + 1],
    ]);
    let Some(ratio) = conversion_ratio(old_bps)? else {
        return Ok(ConvertOutcome::Skipped);
    };

    let old_spc = boot[ntfs_off::SECTORS_PER_CLUSTER] as u64;
    // SectorsPerCluster is a u8. Values 0xF0..=0xFF encode a cluster size as a
    // negative power of two (huge clusters); v1 doesn't scale that encoding.
    if old_spc == 0 || old_spc > 128 {
        return Err(PhoenixError::Other(format!(
            "NTFS SectorsPerCluster {old_spc} is out of the convertible range (1..=128); this \
             volume uses a cluster geometry v1 can't rescale"
        )));
    }
    let new_spc = scale(old_spc, ratio, 255, "NTFS SectorsPerCluster")?;

    let old_total = u64::from_le_bytes(
        boot[ntfs_off::TOTAL_SECTORS..ntfs_off::TOTAL_SECTORS + 8]
            .try_into()
            .unwrap(),
    );
    let new_total = old_total.checked_mul(ratio).ok_or_else(|| {
        PhoenixError::Other("sector-size conversion overflow scaling NTFS TotalSectors".into())
    })?;
    let new_hidden = hidden_sectors(target_offset_bytes)?;

    // NTFS keeps a duplicate boot sector at the last sector of the volume, i.e.
    // partition-relative `old_total × old_bps` bytes. The scale leaves that byte
    // offset invariant (old_total × old_bps == new_total × 512), so we overwrite
    // the source's copy in place with the patched VBR.
    let backup_off = old_total.checked_mul(old_bps as u64).ok_or_else(|| {
        PhoenixError::Other("sector-size conversion overflow computing NTFS backup boot".into())
    })?;

    boot[ntfs_off::BYTES_PER_SECTOR..ntfs_off::BYTES_PER_SECTOR + 2]
        .copy_from_slice(&(TARGET_SECTOR as u16).to_le_bytes());
    boot[ntfs_off::SECTORS_PER_CLUSTER] = new_spc as u8;
    boot[ntfs_off::HIDDEN_SECTORS..ntfs_off::HIDDEN_SECTORS + 4]
        .copy_from_slice(&new_hidden.to_le_bytes());
    boot[ntfs_off::TOTAL_SECTORS..ntfs_off::TOTAL_SECTORS + 8]
        .copy_from_slice(&new_total.to_le_bytes());

    writer.write_at(0, boot)?;
    writer.write_at(backup_off, boot)?;

    info!(
        old_bytes_per_sector = old_bps,
        new_bytes_per_sector = TARGET_SECTOR,
        old_sectors_per_cluster = old_spc,
        new_sectors_per_cluster = new_spc,
        old_total_sectors = old_total,
        new_total_sectors = new_total,
        hidden_sectors = new_hidden,
        backup_boot_offset = backup_off,
        "converted NTFS boot sector 4Kn → 512e (primary + backup)"
    );
    Ok(ConvertOutcome::ConvertedNtfs)
}

/// FAT32 BPB field offsets.
mod fat_off {
    pub const BYTES_PER_SECTOR: usize = 0x0B; // u16
    pub const SECTORS_PER_CLUSTER: usize = 0x0D; // u8
    pub const RESERVED_SECTORS: usize = 0x0E; // u16
    pub const ROOT_ENTRY_COUNT: usize = 0x11; // u16 (0 for FAT32)
    pub const FAT_SIZE_16: usize = 0x16; // u16 (0 for FAT32)
    pub const HIDDEN_SECTORS: usize = 0x1C; // u32
    pub const TOTAL_SECTORS_32: usize = 0x20; // u32
    pub const FAT_SIZE_32: usize = 0x24; // u32
    pub const FS_INFO_SECTOR: usize = 0x30; // u16
    pub const BK_BOOT_SECTOR: usize = 0x32; // u16
}

/// A FAT boot sector is FAT32 (as opposed to FAT12/16) when it uses the FAT32
/// BPB shape: no fixed root directory (`RootEntCnt == 0`), the 16-bit FAT size
/// is unused (`FATSz16 == 0`), and the 32-bit FAT size is set. This is the same
/// discriminator the FAT spec uses.
fn is_fat32(boot: &[u8]) -> bool {
    if boot.len() < 512 || boot[510] != 0x55 || boot[511] != 0xAA {
        return false;
    }
    let root_ent = u16::from_le_bytes([
        boot[fat_off::ROOT_ENTRY_COUNT],
        boot[fat_off::ROOT_ENTRY_COUNT + 1],
    ]);
    let fatsz16 = u16::from_le_bytes([boot[fat_off::FAT_SIZE_16], boot[fat_off::FAT_SIZE_16 + 1]]);
    let fatsz32 = u32::from_le_bytes(
        boot[fat_off::FAT_SIZE_32..fat_off::FAT_SIZE_32 + 4]
            .try_into()
            .unwrap(),
    );
    root_ent == 0 && fatsz16 == 0 && fatsz32 != 0
}

fn convert_fat32(
    writer: &mut PartitionWriter,
    boot: &mut [u8],
    target_offset_bytes: u64,
) -> Result<ConvertOutcome> {
    let old_bps = u16::from_le_bytes([
        boot[fat_off::BYTES_PER_SECTOR],
        boot[fat_off::BYTES_PER_SECTOR + 1],
    ]);
    let Some(ratio) = conversion_ratio(old_bps)? else {
        return Ok(ConvertOutcome::Skipped);
    };

    let old_spc = boot[fat_off::SECTORS_PER_CLUSTER] as u64;
    if old_spc == 0 {
        return Err(PhoenixError::Other(
            "FAT32 boot sector reports zero SectorsPerCluster".into(),
        ));
    }
    let new_spc = scale(old_spc, ratio, 255, "FAT32 SectorsPerCluster")?;

    let read_u16 = |off: usize| u16::from_le_bytes([boot[off], boot[off + 1]]) as u64;
    let read_u32 = |off: usize| u32::from_le_bytes(boot[off..off + 4].try_into().unwrap()) as u64;

    let new_reserved = scale(
        read_u16(fat_off::RESERVED_SECTORS),
        ratio,
        u16::MAX as u64,
        "FAT32 ReservedSectors",
    )?;
    let new_totsec32 = scale(
        read_u32(fat_off::TOTAL_SECTORS_32),
        ratio,
        u32::MAX as u64,
        "FAT32 TotSec32",
    )?;
    let new_fatsz32 = scale(
        read_u32(fat_off::FAT_SIZE_32),
        ratio,
        u32::MAX as u64,
        "FAT32 FATSz32",
    )?;
    let new_fsinfo = scale(
        read_u16(fat_off::FS_INFO_SECTOR),
        ratio,
        u16::MAX as u64,
        "FAT32 FSInfo sector",
    )?;
    let old_bkboot = read_u16(fat_off::BK_BOOT_SECTOR);
    let new_bkboot = scale(old_bkboot, ratio, u16::MAX as u64, "FAT32 BkBootSec")?;
    let new_hidden = hidden_sectors(target_offset_bytes)?;

    // The FAT32 backup boot sector lives at `BkBootSec × BytesPerSector` bytes
    // from the partition start; like NTFS's the byte offset is invariant under
    // the scale (old_bkboot × old_bps == new_bkboot × 512), so we overwrite it
    // in place. The backup FSInfo sitting beside it needs no change.
    let backup_off = old_bkboot.checked_mul(old_bps as u64).ok_or_else(|| {
        PhoenixError::Other("sector-size conversion overflow computing FAT32 backup boot".into())
    })?;

    boot[fat_off::BYTES_PER_SECTOR..fat_off::BYTES_PER_SECTOR + 2]
        .copy_from_slice(&(TARGET_SECTOR as u16).to_le_bytes());
    boot[fat_off::SECTORS_PER_CLUSTER] = new_spc as u8;
    boot[fat_off::RESERVED_SECTORS..fat_off::RESERVED_SECTORS + 2]
        .copy_from_slice(&(new_reserved as u16).to_le_bytes());
    boot[fat_off::HIDDEN_SECTORS..fat_off::HIDDEN_SECTORS + 4]
        .copy_from_slice(&new_hidden.to_le_bytes());
    boot[fat_off::TOTAL_SECTORS_32..fat_off::TOTAL_SECTORS_32 + 4]
        .copy_from_slice(&(new_totsec32 as u32).to_le_bytes());
    boot[fat_off::FAT_SIZE_32..fat_off::FAT_SIZE_32 + 4]
        .copy_from_slice(&(new_fatsz32 as u32).to_le_bytes());
    boot[fat_off::FS_INFO_SECTOR..fat_off::FS_INFO_SECTOR + 2]
        .copy_from_slice(&(new_fsinfo as u16).to_le_bytes());
    boot[fat_off::BK_BOOT_SECTOR..fat_off::BK_BOOT_SECTOR + 2]
        .copy_from_slice(&(new_bkboot as u16).to_le_bytes());

    writer.write_at(0, boot)?;
    if backup_off != 0 {
        writer.write_at(backup_off, boot)?;
    }

    info!(
        old_bytes_per_sector = old_bps,
        new_bytes_per_sector = TARGET_SECTOR,
        new_sectors_per_cluster = new_spc,
        new_reserved_sectors = new_reserved,
        new_total_sectors = new_totsec32,
        new_fat_size = new_fatsz32,
        hidden_sectors = new_hidden,
        backup_boot_offset = backup_off,
        "converted FAT32 boot sector 4Kn → 512e (primary + backup)"
    );
    Ok(ConvertOutcome::ConvertedFat32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PartitionWriter` writes to a raw disk device; for unit tests we drive
    /// the pure conversion math directly against an in-memory boot sector using
    /// the same read/patch/write logic, so the field scaling is exercised
    /// without a disk. (The end-to-end write-to-disk path is covered by the T2
    /// systests, which mount + chkdsk the converted volume.)
    ///
    /// This mirror opens a temp file as the "partition" so it can go through a
    /// real `PartitionWriter`, keeping the offset math honest.
    struct TempPart {
        writer: PartitionWriter,
        path: std::path::PathBuf,
    }

    fn build_ntfs_4kn_boot(partition_bytes: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..7].copy_from_slice(b"NTFS");
        b[ntfs_off::BYTES_PER_SECTOR..ntfs_off::BYTES_PER_SECTOR + 2]
            .copy_from_slice(&4096u16.to_le_bytes());
        b[ntfs_off::SECTORS_PER_CLUSTER] = 1; // 4 KiB clusters on 4Kn
        let total = partition_bytes / 4096 - 1;
        b[ntfs_off::TOTAL_SECTORS..ntfs_off::TOTAL_SECTORS + 8]
            .copy_from_slice(&total.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    fn build_fat32_4kn_boot() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"MSDOS5.0");
        b[fat_off::BYTES_PER_SECTOR..fat_off::BYTES_PER_SECTOR + 2]
            .copy_from_slice(&4096u16.to_le_bytes());
        b[fat_off::SECTORS_PER_CLUSTER] = 1;
        b[fat_off::RESERVED_SECTORS..fat_off::RESERVED_SECTORS + 2]
            .copy_from_slice(&4u16.to_le_bytes());
        b[fat_off::ROOT_ENTRY_COUNT..fat_off::ROOT_ENTRY_COUNT + 2]
            .copy_from_slice(&0u16.to_le_bytes());
        b[fat_off::FAT_SIZE_16..fat_off::FAT_SIZE_16 + 2].copy_from_slice(&0u16.to_le_bytes());
        b[fat_off::TOTAL_SECTORS_32..fat_off::TOTAL_SECTORS_32 + 4]
            .copy_from_slice(&65536u32.to_le_bytes());
        b[fat_off::FAT_SIZE_32..fat_off::FAT_SIZE_32 + 4].copy_from_slice(&256u32.to_le_bytes());
        b[fat_off::FS_INFO_SECTOR..fat_off::FS_INFO_SECTOR + 2]
            .copy_from_slice(&1u16.to_le_bytes());
        b[fat_off::BK_BOOT_SECTOR..fat_off::BK_BOOT_SECTOR + 2]
            .copy_from_slice(&6u16.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn is_fat32_discriminates() {
        assert!(is_fat32(&build_fat32_4kn_boot()));
        // A FAT16 boot (RootEntCnt != 0, FATSz16 != 0) is not FAT32.
        let mut b = build_fat32_4kn_boot();
        b[fat_off::ROOT_ENTRY_COUNT..fat_off::ROOT_ENTRY_COUNT + 2]
            .copy_from_slice(&512u16.to_le_bytes());
        b[fat_off::FAT_SIZE_16..fat_off::FAT_SIZE_16 + 2].copy_from_slice(&20u16.to_le_bytes());
        assert!(!is_fat32(&b));
    }

    #[test]
    fn ntfs_field_scaling_preserves_cluster_bytes_and_offsets() {
        // 512 MiB partition on 4Kn.
        let p = 512 * 1024 * 1024u64;
        let boot = build_ntfs_4kn_boot(p);
        let mut patched = boot.clone();
        // Reproduce convert_ntfs's math (offset 1 MiB partition start).
        let ratio = 8u64;
        let old_spc = boot[ntfs_off::SECTORS_PER_CLUSTER] as u64;
        let old_total = u64::from_le_bytes(boot[40..48].try_into().unwrap());
        patched[11..13].copy_from_slice(&512u16.to_le_bytes());
        patched[ntfs_off::SECTORS_PER_CLUSTER] = (old_spc * ratio) as u8;
        patched[40..48].copy_from_slice(&(old_total * ratio).to_le_bytes());

        let new_bps = u16::from_le_bytes([patched[11], patched[12]]) as u64;
        let new_spc = patched[ntfs_off::SECTORS_PER_CLUSTER] as u64;
        // Cluster size in bytes is invariant: 4096×1 == 512×8.
        assert_eq!(4096 * old_spc, new_bps * new_spc);
        // Backup boot byte offset is invariant.
        let new_total = u64::from_le_bytes(patched[40..48].try_into().unwrap());
        assert_eq!(old_total * 4096, new_total * 512);
    }

    impl TempPart {
        fn new(bytes: usize) -> Self {
            let path = std::env::temp_dir().join(format!(
                "phoenix-sconv-{}.bin",
                uuid::Uuid::new_v4().simple()
            ));
            {
                let f = std::fs::File::create(&path).unwrap();
                f.set_len(bytes as u64).unwrap();
            }
            // PartitionWriter opens a device path; a plain file works for the
            // 512-aligned read/write path we exercise here.
            let writer = PartitionWriter::open_disk(path.to_str().unwrap(), 0, 512).unwrap();
            Self { writer, path }
        }
    }

    impl Drop for TempPart {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn end_to_end_ntfs_rewrites_primary_and_backup() {
        // A small 4Kn NTFS partition laid into a temp-file "partition".
        let p = 16 * 1024 * 1024u64; // 16 MiB
        let boot = build_ntfs_4kn_boot(p);
        let old_total = u64::from_le_bytes(boot[40..48].try_into().unwrap());

        let mut part = TempPart::new(p as usize);
        part.writer.write_at(0, &boot).unwrap();
        // Also lay a copy at the source backup location so we can prove it's
        // overwritten with the patched VBR.
        part.writer.write_at(old_total * 4096, &boot).unwrap();

        let outcome = apply_sector_conversion(&mut part.writer, 1024 * 1024).unwrap();
        assert_eq!(outcome, ConvertOutcome::ConvertedNtfs);

        let mut primary = vec![0u8; 512];
        part.writer.read_at(0, &mut primary).unwrap();
        assert_eq!(u16::from_le_bytes([primary[11], primary[12]]), 512);
        assert_eq!(primary[ntfs_off::SECTORS_PER_CLUSTER], 8);
        assert_eq!(
            u32::from_le_bytes(primary[28..32].try_into().unwrap()),
            (1024 * 1024) / 512
        );
        let new_total = u64::from_le_bytes(primary[40..48].try_into().unwrap());
        assert_eq!(new_total, old_total * 8);

        // Backup boot sector (same byte offset) now holds the patched VBR.
        let mut backup = vec![0u8; 512];
        part.writer.read_at(new_total * 512, &mut backup).unwrap();
        assert_eq!(backup, primary);
    }

    #[test]
    fn skips_non_convertible_boot() {
        let mut part = TempPart::new(1024 * 1024);
        // exFAT magic — must be left untouched.
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"EXFAT   ");
        part.writer.write_at(0, &boot).unwrap();
        assert_eq!(
            apply_sector_conversion(&mut part.writer, 0).unwrap(),
            ConvertOutcome::Skipped
        );
    }
}
