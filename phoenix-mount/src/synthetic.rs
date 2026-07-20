//! A synthesized read-only virtual disk over a `.phnx` backup: `[protective
//! MBR + primary GPT] [partition data, served on demand from the chunk store]
//! [backup GPT] [fixed-VHD footer]`. Exposes `read_at` over the whole image
//! WITHOUT materializing anything, so it can back both the (stopgap) file
//! materialization and the space-efficient WinFsp on-demand mount — the WinFsp
//! filesystem's read handler just calls [`SyntheticVhd::read_at`].
//!
//! Every partition-data read goes through [`ChunkStore`], which decompresses and
//! BLAKE3-verifies each chunk on a cache miss, so corruption surfaces as a read
//! error rather than silent garbage.

use phoenix_core::container::PhnxReader;
use phoenix_core::error::{PhoenixError, Result};

use crate::chunkstore::{plan_layout, ChunkStore, PartitionSpan};
use crate::gpt::{self, GptPart};
use crate::mbr;
use crate::vhd::{self, VHD_MAX_BYTES};
use crate::vhdx::Vhdx;

/// 1 MiB partition alignment, matching what the restore path lays down.
pub(crate) const ALIGN: u64 = 1024 * 1024;

/// How the raw disk image is wrapped for `AttachVirtualDisk`.
///
/// The two formats differ in exactly one way that matters here: a fixed VHD
/// **cannot express a sector size** (512 is baked into the format), and VHDX can.
/// So a 512e backup keeps the VHD path — it is simpler, it is what every existing
/// mount test exercises, and there is nothing to gain by moving it — while a 4Kn
/// backup gets a VHDX, without which its volumes could be attached but never
/// mounted.
enum Container {
    /// `[disk image][512-byte footer]`.
    Vhd(Box<[u8; 512]>),
    /// `[prologue][disk image]`, with the logical sector size stated in metadata.
    Vhdx(Box<Vhdx>),
}

pub struct SyntheticVhd {
    store: ChunkStore,
    /// Protective MBR + primary GPT header + entry array.
    gpt_leading: Vec<u8>,
    /// Backup GPT entry array + header, at the end of the disk.
    gpt_trailing: Vec<u8>,
    container: Container,
    /// Logical sector size the synthesized disk advertises — 512, or 4096 when the
    /// backup came from a 4Kn disk. Every GPT LBA below is in these units.
    sector_size: u64,
    /// Byte offset at which the trailing (backup) GPT begins.
    trailing_start: u64,
    /// Byte length of the leading GPT region.
    leading_end: u64,
    /// Virtual disk size (excludes the trailing VHD footer).
    disk_size: u64,
    spans: Vec<PartitionSpan>,
}

impl SyntheticVhd {
    /// Build the synthesized disk from an opened backup. Consumes the reader
    /// (the chunk store owns it for on-demand reads).
    ///
    /// A 512e backup is wrapped in a fixed VHD; a 4Kn backup requires VHDX. Use
    /// [`SyntheticVhd::build_vhdx`] when the caller specifically needs a VHDX
    /// container regardless of sector size (e.g. a writable-overlay parent — a
    /// fixed VHD cannot be a differencing parent).
    pub fn build(reader: PhnxReader) -> Result<Self> {
        Self::build_inner(reader, false, false)
    }

    /// Like [`SyntheticVhd::build`], but always wraps the image in VHDX even for
    /// a 512e backup. Required for the writable overlay: only a VHDX can be named
    /// as the parent of a Windows differencing disk.
    pub fn build_vhdx(reader: PhnxReader) -> Result<Self> {
        Self::build_inner(reader, true, false)
    }

    /// As [`SyntheticVhd::build_vhdx`], but the synthesized GPT carries the
    /// SOURCE disk's identity — original disk GUID, original partition unique
    /// GUIDs, original GPT attribute bits — instead of the backup-id-derived
    /// ones. Required to *boot* the image: the Windows BCD references the OS
    /// partition by disk GUID + PartitionId, and a regenerated identity fails
    /// `winload.efi` with 0xc000000e (validated live on a real capture).
    ///
    /// Never attach a disk served with this identity to the host while the
    /// source disk (or another mount of the same backup) is present — GPT
    /// collisions are exactly why the mount paths derive fresh GUIDs.
    pub fn build_vhdx_original_identity(reader: PhnxReader) -> Result<Self> {
        Self::build_inner(reader, true, true)
    }

    fn build_inner(reader: PhnxReader, force_vhdx: bool, original_identity: bool) -> Result<Self> {
        // The synthesized disk must advertise the SOURCE disk's sector size. A
        // volume captured from a 4Kn disk records `BytesPerSector = 4096` in its
        // own boot sector, and NTFS refuses to mount when the filesystem's sector
        // size disagrees with the device it is sitting on. Present such a volume on
        // a 512-byte device and Windows attaches the disk, then calls it RAW.
        let sector_size = match reader.manifest.disk.sector_size {
            0 | 512 => 512u64,
            4096 => 4096u64,
            other => {
                return Err(PhoenixError::Other(format!(
                    "backup came from a disk with a {other}-byte sector, which neither VHD (512 \
                     only) nor VHDX (512 or 4096) can express"
                )))
            }
        };

        // An MBR disk is one sector at the front and nothing at the back, so it
        // reserves no trailing region at all — unlike GPT's 33-sector backup
        // copy. Decided here because it changes the disk size below.
        let is_mbr = reader.manifest.disk.style.eq_ignore_ascii_case("mbr");

        let (spans, raw_disk_size) = plan_layout(&reader, ALIGN);
        let trail_bytes = if is_mbr {
            0
        } else {
            gpt::trailing_sectors(sector_size) * sector_size
        };
        // Round up to a sector multiple and leave room for the trailing GPT past
        // the last partition.
        let disk_size = align_up(raw_disk_size + trail_bytes + ALIGN, sector_size);

        // A fixed VHD cannot say what its sector size is — 512 is the format, not
        // a field — so 4Kn must go through VHDX. VHDX also has no 2040 GiB ceiling,
        // which is the other thing that used to make a mount simply refuse. And a
        // fixed VHD cannot be a differencing parent, so `force_vhdx` routes a 512e
        // backup through VHDX too when the caller needs a writable-overlay parent.
        let container = if sector_size == 512 && !force_vhdx {
            if disk_size > VHD_MAX_BYTES {
                return Err(PhoenixError::Other(format!(
                    "backup describes a {disk_size}-byte disk, larger than the \
                     {VHD_MAX_BYTES}-byte fixed-VHD limit"
                )));
            }
            Container::Vhd(Box::new(vhd::build_footer(
                disk_size,
                *reader.header.backup_id.as_bytes(),
            )))
        } else {
            Container::Vhdx(Box::new(Vhdx::new(
                disk_size,
                sector_size as u32,
                *reader.header.backup_id.as_bytes(),
            )?))
        };

        // Synthesize the GPT + footer from the layout while we still hold the
        // reader's index.
        //
        // A writable overlay gets a DISTINCT GPT identity (disk GUID + the
        // partition unique GUIDs derived from it). Otherwise it would carry the
        // same backup_id-derived GPT GUID as a read-only mount of the same
        // backup — and the GUI enables write by ejecting the read-only mount and
        // immediately attaching this one, so the two same-GUID disks briefly
        // coexist while the old one detaches. Windows keeps the newcomer offline
        // in that window, and its volume never surfaces (no drive letter). A
        // different identity sidesteps the collision entirely; the mounted data
        // is unaffected (the NTFS volume's own id lives in its boot sector).
        let disk_guid = if original_identity {
            // Boot-faithful: the source disk's GUID from the manifest, falling
            // back to the backup id for MBR sources / pre-fidelity backups.
            reader
                .manifest
                .disk
                .disk_guid
                .as_deref()
                .and_then(phoenix_core::disk::guid_from_string)
                .unwrap_or_else(|| *reader.header.backup_id.as_bytes())
        } else if force_vhdx {
            writable_overlay_guid(reader.header.backup_id.as_bytes())
        } else {
            *reader.header.backup_id.as_bytes()
        };
        if is_mbr {
            return Self::finish_mbr(reader, spans, disk_size, sector_size, container);
        }

        let parts: Vec<GptPart> = spans
            .iter()
            .map(|s| {
                let entry = reader.index.iter().find(|e| e.index == s.partition_index);
                // MBR-source backups store an all-zero type GUID; in GPT that
                // means "unused entry", so substitute Basic Data (as restore
                // does) or Windows presents the disk as blank.
                let type_guid = match entry.map(|e| e.type_guid) {
                    Some(g) if g != [0u8; 16] => g,
                    _ => gpt::BASIC_DATA_TYPE_GUID,
                };
                // The binary index has no GPT identity; the JSON manifest does.
                let mpart = reader
                    .manifest
                    .partitions
                    .iter()
                    .find(|p| p.index == s.partition_index);
                let unique_guid = if original_identity {
                    mpart
                        .and_then(|p| p.unique_guid.as_deref())
                        .and_then(phoenix_core::disk::guid_from_string)
                        .unwrap_or_else(|| derive_guid(&disk_guid, s.partition_index))
                } else {
                    derive_guid(&disk_guid, s.partition_index)
                };
                let attributes = if original_identity {
                    mpart.and_then(|p| p.gpt_attributes).unwrap_or(0)
                } else {
                    0
                };
                GptPart {
                    type_guid,
                    unique_guid,
                    // LBAs are in the DISK's sectors, so partition offsets divide
                    // by 4096 on a 4Kn disk, not 512. Partition spans are 1 MiB
                    // aligned, so both divide cleanly.
                    first_lba: s.disk_offset / sector_size,
                    last_lba: (s.disk_offset + s.size) / sector_size - 1,
                    attributes,
                    name: entry.map(|e| e.name.clone()).unwrap_or_default(),
                }
            })
            .collect();
        let gpt_img = gpt::synthesize(disk_size, disk_guid, &parts, sector_size);

        let leading_end = gpt::leading_sectors(sector_size) * sector_size;
        let trailing_start = disk_size - gpt::trailing_sectors(sector_size) * sector_size;
        debug_assert_eq!(gpt_img.leading.len() as u64, leading_end);
        debug_assert_eq!(gpt_img.trailing.len() as u64, disk_size - trailing_start);

        let store = ChunkStore::new(reader, spans.clone(), disk_size)?;

        Ok(Self {
            store,
            sector_size,
            leading_end,
            trailing_start,
            container,
            gpt_leading: gpt_img.leading,
            gpt_trailing: gpt_img.trailing,
            disk_size,
            spans,
        })
    }

    /// Finish building a disk whose source was MBR-partitioned.
    ///
    /// Split out because the shape genuinely differs rather than just the
    /// bytes: one leading sector, no trailing region, and identity that comes
    /// from a 4-byte signature instead of GUIDs. A BIOS guest boots this, so
    /// the signature and the active flag are load-bearing — the BCD names the
    /// boot disk by the former and the MBR boot code chains on the latter.
    fn finish_mbr(
        reader: PhnxReader,
        spans: Vec<PartitionSpan>,
        disk_size: u64,
        sector_size: u64,
        container: Container,
    ) -> Result<Self> {
        // Prefer the captured signature. The fallback keeps pre-fidelity
        // backups (taken before this was recorded) mountable with a stable,
        // non-zero signature — they just may need boot repair to boot, which
        // is the same answer a restored disk gets.
        let signature = reader.manifest.disk.disk_signature.unwrap_or_else(|| {
            u32::from_le_bytes(reader.header.backup_id.as_bytes()[..4].try_into().unwrap())
        });

        let mut parts: Vec<mbr::MbrPart> = spans
            .iter()
            .map(|s| {
                let mpart = reader
                    .manifest
                    .partitions
                    .iter()
                    .find(|p| p.index == s.partition_index);
                mbr::MbrPart {
                    // LBAs are in the DISK's sectors, so a 4Kn disk divides by
                    // 4096. Spans are 1 MiB aligned, so both divide cleanly.
                    first_lba: s.disk_offset / sector_size,
                    sectors: s.size / sector_size,
                    partition_type: mpart.and_then(|p| p.mbr_type).unwrap_or(0),
                    bootable: mpart.and_then(|p| p.mbr_bootable).unwrap_or(false),
                }
            })
            .collect();

        // A table with nothing active does not boot, and a backup predating
        // the captured flag has none.
        if !parts.iter().any(|p| p.bootable) {
            if let Some(i) = guess_active_partition(&reader, &spans) {
                parts[i].bootable = true;
            }
        }

        let leading = mbr::synthesize(signature, &parts, sector_size as usize);
        let leading_end = sector_size;
        // No trailing region: `trailing_start == disk_size` makes the
        // dispatcher's trailing branch unreachable.
        let trailing_start = disk_size;
        debug_assert_eq!(leading.len() as u64, leading_end);

        let store = ChunkStore::new(reader, spans.clone(), disk_size)?;
        Ok(Self {
            store,
            sector_size,
            leading_end,
            trailing_start,
            container,
            gpt_leading: leading,
            gpt_trailing: Vec::new(),
            disk_size,
            spans,
        })
    }

    /// Total length of the **file** the mount serves: the virtual disk plus
    /// whatever the container wraps it in.
    pub fn total_len(&self) -> u64 {
        match &self.container {
            // `[disk image][512-byte footer]`
            Container::Vhd(_) => self.disk_size + 512,
            // `[prologue][disk image][padding to a whole block]`
            Container::Vhdx(v) => v.file_size(),
        }
    }

    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    /// The logical sector size the attached disk will report — 512, or 4096 for a
    /// backup taken from a 4Kn disk.
    pub fn sector_size(&self) -> u64 {
        self.sector_size
    }

    /// The filename to serve. `OpenVirtualDisk` sniffs the content, but the
    /// extension is what a human (and some tooling) will trust.
    pub fn image_name(&self) -> &'static str {
        match &self.container {
            Container::Vhd(_) => "backup.vhd",
            Container::Vhdx(_) => "backup.vhdx",
        }
    }

    pub fn spans(&self) -> &[PartitionSpan] {
        &self.spans
    }

    /// Read `buf.len()` bytes at `offset` **in the container file**, which is what
    /// the mount actually serves.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        // Split the borrow: the VHDX arm needs `&container` and `&mut store` at
        // once, and they are disjoint fields.
        let Self {
            store,
            gpt_leading,
            gpt_trailing,
            container,
            disk_size,
            leading_end,
            trailing_start,
            ..
        } = self;

        match container {
            Container::Vhd(footer) => {
                // `[disk image][footer]`. The footer sits past the disk, so serve
                // it here and let everything below it be the disk image.
                buf.fill(0);
                let total = *disk_size + 512;
                let mut done = 0usize;
                while done < buf.len() {
                    let pos = offset + done as u64;
                    if pos >= total {
                        break;
                    }
                    let remaining = buf.len() - done;
                    if pos >= *disk_size {
                        let off = (pos - *disk_size) as usize;
                        let n = remaining.min((total - pos) as usize);
                        buf[done..done + n].copy_from_slice(&footer[off..off + n]);
                        done += n;
                    } else {
                        let n = remaining.min((*disk_size - pos) as usize);
                        read_disk_image(
                            store,
                            gpt_leading,
                            gpt_trailing,
                            *disk_size,
                            *leading_end,
                            *trailing_start,
                            pos,
                            &mut buf[done..done + n],
                        )?;
                        done += n;
                    }
                }
                Ok(())
            }
            Container::Vhdx(v) => {
                // `[prologue][disk image]`. The VHDX knows where its payload starts
                // and hands us plain image offsets — so the disk image below is
                // written once and both containers use it.
                v.read_at(offset, buf, |image_off, dst| {
                    read_disk_image(
                        store,
                        gpt_leading,
                        gpt_trailing,
                        *disk_size,
                        *leading_end,
                        *trailing_start,
                        image_off,
                        dst,
                    )
                })
            }
        }
    }
}

/// The raw disk image, container-free: `[leading GPT][partition data][backup GPT]`.
///
/// Both containers are only framing around this. Keeping it a free function is
/// what lets the VHDX arm borrow the chunk store mutably while borrowing the
/// container immutably.
#[allow(clippy::too_many_arguments)]
fn read_disk_image(
    store: &mut ChunkStore,
    gpt_leading: &[u8],
    gpt_trailing: &[u8],
    disk_size: u64,
    leading_end: u64,
    trailing_start: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<()> {
    buf.fill(0);
    let mut done = 0usize;
    while done < buf.len() {
        let pos = offset + done as u64;
        if pos >= disk_size {
            break; // past the disk: leave zeros
        }
        let remaining = buf.len() - done;
        let (src, run): (&[u8], usize) = if pos < leading_end {
            let off = pos as usize;
            let n = remaining.min((leading_end - pos) as usize);
            (&gpt_leading[off..off + n], n)
        } else if pos >= trailing_start {
            let off = (pos - trailing_start) as usize;
            let n = remaining.min((disk_size - pos) as usize);
            (&gpt_trailing[off..off + n], n)
        } else {
            // Partition data: served on demand, zeros for gaps and free space.
            let n = remaining.min((trailing_start - pos) as usize);
            store.read_at(pos, &mut buf[done..done + n])?;
            done += n;
            continue;
        };
        buf[done..done + run].copy_from_slice(src);
        done += run;
    }
    Ok(())
}

/// Largest a "System Reserved" partition is expected to be. Windows has used
/// 100 MB (7), 350 MB (8) and 500–579 MB (10/11); 1 GiB covers all of them
/// with room to spare while staying far below any real Windows volume.
const SYSTEM_RESERVED_MAX: u64 = 1024 * 1024 * 1024;

/// Which partition to mark active when the backup didn't record it.
///
/// Only reachable for backups captured before the active flag was recorded —
/// every MBR capture since carries the real one. It exists so such a backup
/// boots rather than presenting a table with nothing active, which cannot.
///
/// Neither position nor size alone is the answer. The active flag says which
/// partition's boot record the MBR code loads, so it has to be the one holding
/// the boot manager:
///
/// - A standard BIOS install puts `bootmgr` in a small NTFS **System
///   Reserved** partition and leaves the large Windows volume inactive — so
///   "the biggest partition" is wrong on the most common layout.
/// - A single-partition install has no System Reserved, and `bootmgr` lives on
///   the Windows volume itself — so "the small one" is wrong there.
/// - A disk with an OEM diagnostic or another OS ahead of Windows breaks
///   "the first partition".
///
/// So look for the System-Reserved shape first, and fall back to the largest
/// NTFS volume. Guessing wrong costs a boot repair from rescue media; not
/// guessing costs a disk that certainly won't boot.
fn guess_active_partition(reader: &PhnxReader, spans: &[PartitionSpan]) -> Option<usize> {
    let is_ntfs = |i: usize| -> bool {
        spans.get(i).is_some_and(|s| {
            reader
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == s.partition_index)
                .is_some_and(|p| p.fs.eq_ignore_ascii_case("ntfs"))
        })
    };

    // System Reserved: the first small NTFS partition on the disk.
    if let Some(i) = (0..spans.len())
        .find(|&i| is_ntfs(i) && spans[i].size > 0 && spans[i].size <= SYSTEM_RESERVED_MAX)
    {
        return Some(i);
    }
    // Otherwise the Windows volume itself, on a single-partition install.
    (0..spans.len())
        .filter(|&i| is_ntfs(i))
        .max_by_key(|&i| spans[i].size)
        .or(if spans.is_empty() { None } else { Some(0) })
}

fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

/// A distinct 16-byte disk GUID for a writable overlay, derived from the
/// backup id so it is stable per backup yet never equal to the read-only mount's
/// GUID (which is the backup id itself). Keeps a just-ejected read-only disk and
/// this one from colliding while the old one finishes detaching.
fn writable_overlay_guid(backup_id: &[u8; 16]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(backup_id);
    hasher.update(b"phoenix-writable-overlay");
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize().as_bytes()[0..16]);
    out
}

/// Deterministic 16-byte GUID from the disk GUID + partition index (for the
/// synthesized GPT's per-partition unique id).
fn derive_guid(disk_guid: &[u8; 16], index: u32) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(disk_guid);
    hasher.update(&index.to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize().as_bytes()[0..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::container::{
        Extent, Header, PartitionStreamSpec, PhnxWriter, EXTENT_LBA_BYTES as LBA, FORMAT_VERSION,
    };
    use phoenix_core::disk::{CaptureMode, FilesystemKind};
    use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
    use uuid::Uuid;

    fn build_backup() -> std::path::PathBuf {
        build_backup_with_type_guid([0x11; 16])
    }

    fn build_backup_with_type_guid(type_guid: [u8; 16]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("synth_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        let ext_bytes = 64 * 1024u64; // one 64 KiB extent
        let extents = vec![Extent {
            start_sector: 0,
            sector_count: ext_bytes / LBA as u64,
        }];
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut s = w
            .begin_partition_stream(PartitionStreamSpec {
                index: 0,
                type_guid,
                name: "Vol".into(),
                original_size: ext_bytes,
                fs_kind: FilesystemKind::Ntfs,
                capture_mode: CaptureMode::UsedBlocks,
                sector_size: LBA,
                used_bytes: 0,
                extents: &extents,
                bytes_per_cluster: 4096,
            })
            .unwrap();
        s.write_chunk(&vec![0x5Au8; ext_bytes as usize]).unwrap();
        let (chunks, _) = s.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "gpt".into(),
                disk_guid: None,
                disk_signature: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "Vol".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: ext_bytes,
                used_bytes: ext_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                mbr_type: None,
                mbr_bootable: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        w.finalize(&manifest).unwrap();
        path
    }

    /// Same shape as `build_backup`, but the manifest describes an MBR disk
    /// carrying real MBR identity — a signature, a type byte and an active
    /// flag — as a capture of a BIOS disk now records.
    fn build_mbr_backup(signature: u32, mbr_type: u8, bootable: bool) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("synth_mbr_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            // flags bit 0 is the GPT flag; clear for an MBR source.
            flags: 0,
            timestamp: 1,
            backup_id,
            disk_signature: signature as u64,
            partition_count: 1,
        };
        let ext_bytes = 64 * 1024u64;
        let extents = vec![Extent {
            start_sector: 0,
            sector_count: ext_bytes / LBA as u64,
        }];
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut s = w
            .begin_partition_stream(PartitionStreamSpec {
                index: 0,
                type_guid: [0u8; 16],
                name: "Vol".into(),
                original_size: ext_bytes,
                fs_kind: FilesystemKind::Ntfs,
                capture_mode: CaptureMode::UsedBlocks,
                sector_size: LBA,
                used_bytes: 0,
                extents: &extents,
                bytes_per_cluster: 4096,
            })
            .unwrap();
        s.write_chunk(&vec![0x5Au8; ext_bytes as usize]).unwrap();
        let (chunks, _) = s.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "mbr".into(),
                disk_guid: None,
                disk_signature: Some(signature),
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "Vol".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: ext_bytes,
                used_bytes: ext_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                mbr_type: Some(mbr_type),
                mbr_bootable: Some(bootable),
                chunks,
                bitmap_hash: None,
            }],
        };
        w.finalize(&manifest).unwrap();
        path
    }

    #[test]
    fn mbr_source_synthesizes_a_real_mbr_not_a_gpt() {
        let path = build_mbr_backup(0xCAFE_F00D, 0x07, true);
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();

        let mut sector = vec![0u8; 512];
        vhd.read_at(0, &mut sector).unwrap();

        assert_eq!(&sector[510..512], &[0x55, 0xAA]);
        // The identity the BCD names the boot disk by, carried from capture.
        assert_eq!(&sector[0x1B8..0x1BC], &0xCAFE_F00Du32.to_le_bytes());
        // A REAL entry, not GPT's 0xEE protective one — that is the whole
        // point: a protective MBR tells a BIOS guest the disk is empty.
        assert_eq!(sector[446 + 4], 0x07);
        assert_ne!(sector[446 + 4], 0xEE);
        assert_eq!(sector[446], 0x80, "active flag must survive capture");

        // The partition starts where the layout put it, in disk sectors.
        let first_lba = u32::from_le_bytes(sector[454..458].try_into().unwrap()) as u64;
        assert_eq!(first_lba * 512, ALIGN, "first partition at the 1 MiB align");

        // And no GPT anywhere: no signature at LBA 1, and nothing reserved at
        // the end of the disk.
        let mut lba1 = vec![0u8; 512];
        vhd.read_at(512, &mut lba1).unwrap();
        assert_ne!(&lba1[0..8], b"EFI PART");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mbr_without_a_captured_active_flag_still_boots_something() {
        // Pre-fidelity backups have no flag recorded, and a table with
        // nothing active cannot boot. One NTFS partition — the single-install
        // case — so it is the one marked.
        let path = build_mbr_backup(1, 0x07, false);
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();
        let mut sector = vec![0u8; 512];
        vhd.read_at(0, &mut sector).unwrap();
        assert_eq!(sector[446], 0x80);
        std::fs::remove_file(&path).ok();
    }

    /// Spans + manifest for a hypothetical layout, to exercise the
    /// active-partition guess without building a whole backup per case.
    fn guess_for(parts: &[(&str, u64)]) -> Option<usize> {
        let path = std::env::temp_dir().join(format!("synth_guess_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 0,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: parts.len() as u32,
        };
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut manifests = Vec::new();
        for (i, (fs, _size)) in parts.iter().enumerate() {
            let ext_bytes = 64 * 1024u64;
            let extents = vec![Extent {
                start_sector: 0,
                sector_count: ext_bytes / LBA as u64,
            }];
            let mut s = w
                .begin_partition_stream(PartitionStreamSpec {
                    index: i as u32,
                    type_guid: [0u8; 16],
                    name: format!("P{i}"),
                    original_size: ext_bytes,
                    fs_kind: FilesystemKind::Ntfs,
                    capture_mode: CaptureMode::UsedBlocks,
                    sector_size: LBA,
                    used_bytes: 0,
                    extents: &extents,
                    bytes_per_cluster: 4096,
                })
                .unwrap();
            s.write_chunk(&vec![0u8; ext_bytes as usize]).unwrap();
            let (chunks, _) = s.finish().unwrap();
            manifests.push(PartitionManifest {
                index: i as u32,
                name: format!("P{i}"),
                type_guid: None,
                fs: (*fs).into(),
                capture_mode: "used-blocks".into(),
                original_size: ext_bytes,
                used_bytes: ext_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                mbr_type: Some(0x07),
                mbr_bootable: Some(false),
                chunks,
                bitmap_hash: None,
            });
        }
        w.finalize(&BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "mbr".into(),
                disk_guid: None,
                disk_signature: Some(1),
                sector_size: 512,
            },
            partitions: manifests,
        })
        .unwrap();

        let reader = PhnxReader::open(&path).unwrap();
        // Spans carry the sizes the guess reasons about; synthesize them at
        // the requested sizes rather than the tiny on-disk payloads.
        let spans: Vec<PartitionSpan> = parts
            .iter()
            .enumerate()
            .map(|(i, (_fs, size))| PartitionSpan {
                partition_index: i as u32,
                disk_offset: ALIGN + i as u64 * ALIGN,
                size: *size,
                ..plan_layout(&reader, ALIGN).0[i].clone()
            })
            .collect();
        let out = guess_active_partition(&reader, &spans);
        drop(reader);
        std::fs::remove_file(&path).ok();
        out
    }

    #[test]
    fn active_guess_prefers_system_reserved_over_the_windows_volume() {
        // The most common BIOS layout. "Largest partition" would pick C: and
        // be wrong — bootmgr lives on System Reserved, and that is what the
        // MBR boot code has to chain into.
        const MB: u64 = 1024 * 1024;
        assert_eq!(guess_for(&[("ntfs", 500 * MB), ("ntfs", 200_000 * MB)]), Some(0));
    }

    #[test]
    fn active_guess_falls_back_to_the_windows_volume_when_alone() {
        // Single-partition install: no System Reserved, bootmgr sits on C:.
        const MB: u64 = 1024 * 1024;
        assert_eq!(guess_for(&[("ntfs", 200_000 * MB)]), Some(0));
    }

    #[test]
    fn active_guess_skips_a_leading_non_ntfs_partition() {
        // An OEM diagnostic or other-OS partition ahead of Windows is what
        // breaks a naive "first partition" rule.
        const MB: u64 = 1024 * 1024;
        assert_eq!(
            guess_for(&[("fat32", 500 * MB), ("ntfs", 500 * MB), ("ntfs", 200_000 * MB)]),
            Some(1)
        );
    }

    #[test]
    fn mbr_partition_data_is_served_after_the_single_leading_sector() {
        // GPT reserves 34 sectors up front; MBR reserves one. The data must
        // still land at the 1 MiB alignment either way.
        let path = build_mbr_backup(1, 0x07, true);
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();
        let mut buf = vec![0u8; 16];
        vhd.read_at(ALIGN, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x5A), "partition payload");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn synthetic_disk_serves_gpt_data_and_footer() {
        let path = build_backup();
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();

        // Protective MBR signature at the end of sector 0.
        let mut sec0 = vec![0u8; 512];
        vhd.read_at(0, &mut sec0).unwrap();
        assert_eq!(sec0[510], 0x55);
        assert_eq!(sec0[511], 0xAA);
        // GPT signature at LBA 1.
        let mut sec1 = vec![0u8; 512];
        vhd.read_at(512, &mut sec1).unwrap();
        assert_eq!(&sec1[0..8], b"EFI PART");

        // Partition data (0x5A) at the partition's disk offset.
        let poff = vhd.spans()[0].disk_offset;
        let mut data = vec![0u8; 4096];
        vhd.read_at(poff, &mut data).unwrap();
        assert!(data.iter().all(|&b| b == 0x5A), "partition data not served");

        // VHD footer cookie at the very end.
        let total = vhd.total_len();
        let mut foot = vec![0u8; 512];
        vhd.read_at(total - 512, &mut foot).unwrap();
        assert_eq!(&foot[0..8], b"conectix");

        // A read straddling the GPT-leading / data-gap boundary is zero past the
        // GPT and returns without error.
        let mut straddle = vec![0xFFu8; 1024];
        vhd.read_at(16 * 512, &mut straddle).unwrap(); // sector 16, inside leading region+gap
                                                       // (No panic / no error is the assertion; content is GPT-or-zero.)

        std::fs::remove_file(&path).ok();
    }

    /// GPT type GUID at entry `i` of the entry array, which starts at LBA 2 — so
    /// its byte offset depends on the disk's sector size.
    fn entry_type_guid(vhd: &mut SyntheticVhd, i: usize) -> [u8; 16] {
        let at = 2 * vhd.sector_size() + i as u64 * 128;
        let mut entry = [0u8; 128];
        vhd.read_at(at, &mut entry).unwrap();
        entry[0..16].try_into().unwrap()
    }

    /// Regression test: backups of MBR disks record an all-zero type GUID,
    /// which in GPT marks the entry as UNUSED — Windows saw a blank disk and
    /// mounted no volumes. The synthesized GPT must substitute Basic Data.
    #[test]
    fn zero_type_guid_falls_back_to_basic_data() {
        let path = build_backup_with_type_guid([0u8; 16]);
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();
        assert_eq!(entry_type_guid(&mut vhd, 0), gpt::BASIC_DATA_TYPE_GUID);
        std::fs::remove_file(&path).ok();
    }

    /// A real (GPT-source) type GUID must pass through untouched.
    #[test]
    fn nonzero_type_guid_is_preserved() {
        let path = build_backup();
        let reader = PhnxReader::open(&path).unwrap();
        let mut vhd = SyntheticVhd::build(reader).unwrap();
        assert_eq!(entry_type_guid(&mut vhd, 0), [0x11; 16]);
        std::fs::remove_file(&path).ok();
    }
}
