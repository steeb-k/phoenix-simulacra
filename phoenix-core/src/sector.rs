//! Logical-sector-size classification for cross-sector-size restore/clone.
//!
//! A backup records its source disk's logical sector size
//! (`manifest.disk.sector_size`). When that differs from the restore/clone
//! target's sector size, most of the imaging world simply refuses — a 4Kn
//! volume's `BytesPerSector = 4096` boot sector lands verbatim on a 512e device
//! and Windows brings the volume up RAW. This module is the decision layer that
//! says whether the mismatch is *convertible* (only 4Kn → 512e), *unsupported*
//! (everything else), or a non-issue (identical), plus per-partition helpers the
//! pre-flight uses to build its warnings.
//!
//! The actual boot-sector rewrite lives in `phoenix_capture::sector_convert`;
//! the full design is in docs/SECTOR-SIZE-CONVERSION.md.

use crate::disk::FilesystemKind;
use crate::error::{PhoenixError, Result};

/// The 512e logical sector size a 4Kn source converts *to* — the only supported
/// conversion target.
pub const SECTOR_512E: u32 = 512;
/// The 4Kn logical sector size a convertible backup comes *from*.
pub const SECTOR_4KN: u32 = 4096;

/// The relationship between a backup's source sector size and the target's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorPlan {
    /// Sizes match (or one is unknown): restore/clone the boot sectors verbatim,
    /// no conversion.
    Identical,
    /// 4Kn (4096) source onto a 512e (512) target: convertible for NTFS/FAT32,
    /// but only when the user opts in.
    Convert4knTo512e,
}

/// Classify a source-vs-target logical sector-size pair.
///
/// * equal (or either unknown/zero) → [`SectorPlan::Identical`]
/// * 4096 → 512 → [`SectorPlan::Convert4knTo512e`]
/// * anything else (notably 512 → 4096) → [`PhoenixError::SectorSizeUnsupported`]
///
/// A zero on either side means "geometry not recorded" — every backup written
/// before `sector_size` was captured was 512e, so treating unknown as Identical
/// preserves the legacy behavior without ever falsely refusing an old backup.
pub fn classify_sector_sizes(source: u32, target: u32) -> Result<SectorPlan> {
    if source == 0 || target == 0 || source == target {
        return Ok(SectorPlan::Identical);
    }
    match (source, target) {
        (SECTOR_4KN, SECTOR_512E) => Ok(SectorPlan::Convert4knTo512e),
        _ => Err(PhoenixError::SectorSizeUnsupported {
            source_sector: source,
            target_sector: target,
        }),
    }
}

/// What a 4Kn → 512e conversion does to a given partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionConvertRole {
    /// A filesystem whose boot sector we rewrite (NTFS, or FAT/ESP).
    Convert,
    /// No filesystem to convert, and it doesn't block boot (MSR / empty /
    /// unclassified): nothing to do.
    NoFilesystem,
    /// A data filesystem v1 does not convert — exFAT, BitLocker ciphertext,
    /// ReFS (its superblock carries a checksum we don't recompute).
    /// Its bytes still restore, but the partition is unreadable on the 512e
    /// target and is left as-is (Alert E).
    LeftUnconverted,
}

/// Map a manifest filesystem kind to its role under a 4Kn → 512e conversion.
///
/// The ESP is recorded as [`FilesystemKind::Efi`] (its type GUID is
/// authoritative and it is captured raw), but it holds a FAT32 filesystem whose
/// boot sector the converter rewrites — so `Efi` counts as `Convert`. The actual
/// conversion re-sniffs the landed boot sector, so this mapping only drives the
/// pre-flight messaging, not the rewrite itself.
pub fn partition_convert_role(fs: FilesystemKind) -> PartitionConvertRole {
    match fs {
        FilesystemKind::Ntfs | FilesystemKind::Fat | FilesystemKind::Efi => {
            PartitionConvertRole::Convert
        }
        FilesystemKind::Msr | FilesystemKind::Unknown => PartitionConvertRole::NoFilesystem,
        FilesystemKind::Exfat | FilesystemKind::Bitlocker | FilesystemKind::Refs => {
            PartitionConvertRole::LeftUnconverted
        }
    }
}

/// The named result of a planned 4Kn → 512e conversion: which partitions will
/// be converted, which are left as-is (Alert E), and whether the ESP is among
/// the converted set (Alert D bootability). Built by the restore/clone
/// pre-flight and shared with the CLI/GUI so all three describe the same
/// operation. Empty/`false` default is the "no conversion" state.
#[derive(Debug, Clone, Default)]
pub struct ConversionReport {
    /// Human names (`"[idx] label"`) of partitions whose boot sector is rewritten.
    pub converted_names: Vec<String>,
    /// Human names of partitions left as-is because v1 does not convert their
    /// filesystem (exFAT / BitLocker ciphertext).
    pub unconverted_names: Vec<String>,
    /// True when the EFI System Partition is in the converted set.
    pub bootable: bool,
}

/// Whether this partition is the EFI System Partition — the one whose successful
/// conversion determines if the restored/cloned disk can boot (Alert D).
pub fn is_esp(fs: FilesystemKind) -> bool {
    matches!(fs, FilesystemKind::Efi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_and_unknown_never_convert_or_refuse() {
        assert_eq!(
            classify_sector_sizes(512, 512).unwrap(),
            SectorPlan::Identical
        );
        assert_eq!(
            classify_sector_sizes(4096, 4096).unwrap(),
            SectorPlan::Identical
        );
        // Zero (pre-sector-size backups) is treated as legacy 512e — no refusal.
        assert_eq!(
            classify_sector_sizes(0, 512).unwrap(),
            SectorPlan::Identical
        );
        assert_eq!(
            classify_sector_sizes(4096, 0).unwrap(),
            SectorPlan::Identical
        );
    }

    #[test]
    fn four_kn_to_512e_is_the_only_conversion() {
        assert_eq!(
            classify_sector_sizes(4096, 512).unwrap(),
            SectorPlan::Convert4knTo512e
        );
    }

    #[test]
    fn coarser_target_and_odd_pairs_are_refused() {
        // 512e → 4Kn: the load-bearing refusal (fine → coarse can't be faked).
        assert!(matches!(
            classify_sector_sizes(512, 4096),
            Err(PhoenixError::SectorSizeUnsupported {
                source_sector: 512,
                target_sector: 4096
            })
        ));
        // Any other mismatch is equally refused.
        assert!(classify_sector_sizes(4096, 2048).is_err());
        assert!(classify_sector_sizes(512, 1024).is_err());
    }

    #[test]
    fn roles_match_the_matrix() {
        assert_eq!(
            partition_convert_role(FilesystemKind::Ntfs),
            PartitionConvertRole::Convert
        );
        assert_eq!(
            partition_convert_role(FilesystemKind::Efi),
            PartitionConvertRole::Convert
        );
        assert_eq!(
            partition_convert_role(FilesystemKind::Msr),
            PartitionConvertRole::NoFilesystem
        );
        assert_eq!(
            partition_convert_role(FilesystemKind::Exfat),
            PartitionConvertRole::LeftUnconverted
        );
        assert!(is_esp(FilesystemKind::Efi));
        assert!(!is_esp(FilesystemKind::Ntfs));
    }
}
