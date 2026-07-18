# Sector-size conversion (4Kn → 512e) on restore and clone

Restore or clone a backup taken from a **4Kn** (4096-byte logical sector) disk onto a **512e** (512-byte logical sector) disk, and end up with a disk Windows can actually mount — and, for a full Windows system disk, boot.

Without conversion that restore comes up **RAW**: used-block capture copies each filesystem's boot sector verbatim, so a 4Kn volume's `BytesPerSector = 4096` lands on a 512-byte-sector device, and Windows rejects the contradiction. (This is why most imaging tools refuse cross-sector clones outright.)

The fix is a **metadata-only boot-sector rewrite** — no file data moves — applied per partition after its bytes land. It is implemented, opt-in, and validated: the T2 suite (`phoenix-systests/tests/sector_convert.rs`) covers NTFS convert+grow, FAT32 convert, the 512e→4Kn refusal, the opt-in gate, and per-partition partial restore, all mounting and passing `chkdsk`; the recipe was first proven by hand on real hardware.

## Why it's safe (the load-bearing fact)

Both NTFS and FAT are **cluster-addressed**. Every file's data, the `$MFT`/`$Bitmap` (NTFS), and the FATs and directories (FAT32) are located by cluster, and a cluster's byte offset is `cluster_number × cluster_size_bytes`. Because the conversion **preserves the cluster size in bytes**, nothing moves: for a 4096→512 flip every sector-denominated field scales by `ratio = 8` and every byte offset is invariant. NTFS's MFT-record fixup stride is always 512 regardless of the volume's sector size, so MFT records need zero change.

## Scope

**In:**

- Direction: **4Kn → 512e only.**
- Filesystems: **NTFS and FAT32** — together these make a bootable Windows disk (ESP + MSR + NTFS + Recovery; MSR is empty and needs nothing).
- Both full-disk and **partial** restore, and clone. Conversion is per-partition.
- Composes with same-size and **grow**. A converted-partition **shrink is refused** (the NTFS relocation rewrite isn't sector-size-aware).

**Out:**

- **512e → 4Kn** (or any other mismatch): hard refusal, no override. Fine→coarse can't be faked — sub-4096-byte clusters can't exist on 4Kn, and 512-but-not-4096-aligned structures break.
- **exFAT**: deferred (its boot region carries a checksum and more sector-denominated fields). An image containing an exFAT data partition is still restorable — that partition is left unconverted, and unreadable on the target, with a specific warning.

## The recipe

Per partition, after its data is written: read the just-landed boot sector at the partition's target offset, and if it is 4Kn and a supported filesystem, patch these fields (all within the first 512 bytes of the VBR), then write the patched VBR to both the primary and the backup boot-sector location. `ratio = old_BytesPerSector / 512`.

**NTFS:** `0x0B BytesPerSector → 512`, `0x0D SectorsPerCluster ×ratio`, `0x1C HiddenSectors → target_partition_offset / 512`, `0x28 TotalSectors ×ratio`. The backup boot sector's byte offset is invariant (`old_TotalSectors × old_BytesPerSector == new_TotalSectors × 512`).

**FAT32:** `0x0B → 512`, `0x0D SectorsPerCluster ×ratio`, `0x0E ReservedSectors ×ratio`, `0x1C HiddenSectors → target_offset / 512`, `0x20 TotSec32 ×ratio`, `0x24 FATSz32 ×ratio`, `0x30 FSInfoSector ×ratio`, `0x32 BkBootSector ×ratio`. The FSInfo body needs no change.

`HiddenSectors` is set from the **target** partition offset (not old×ratio), because a full-disk restore may place the partition at a different offset than the source. Field ranges are guarded — a scale that would overflow (e.g. `SectorsPerCluster` past 255) is refused cleanly.

The conversion runs as a standalone step in the restore/clone write loop that **auto-detects the landed boot sector** (NTFS magic / FAT32 BPB) rather than dispatching on the manifest's filesystem kind — the ESP is classified `Efi` and restored raw, and it is exactly the partition whose conversion makes the disk bootable. Implementation: `apply_sector_conversion` in `phoenix-capture/src/sector_convert.rs`; classification and pre-flight in `phoenix-core/src/sector.rs`.

## Pre-flight decision matrix

Computed from the manifest's source sector size vs the target disk's, per partition:

| Source | Target | Outcome |
|---|---|---|
| 512 | 512 | Normal restore/clone, no conversion. |
| 4096 | 4096 | Normal restore/clone, no conversion. |
| 4096 | 512 | **Conversion available — opt-in required.** CLI: `--convert-sector-size` on `restore`/`clone` (a hard error names the flag otherwise). GUI: a hazard dialog with an acknowledgement checkbox. |
| 512 | 4096 (or other mismatch) | **Hard refusal.** |

Within an opted-in conversion: NTFS/FAT32 partitions convert; MSR/empty need nothing; exFAT and other filesystems are left unconverted with a named warning. If the converted set does not include the ESP, the summary warns that the disk will hold its data but won't boot. The result is always presented as a **converted copy, not a byte-identical restore**.

## Verify-after interaction

No special handling is needed: restore's verify is an in-pipeline chunk-hash check that never re-reads the target, and clone's read-back verify runs per-block *before* the boot-sector rewrite — so a converted operation cannot false-fail on the sectors it rewrote. The "did the conversion take" proof is the mount + `chkdsk` check in the T2 tests.
