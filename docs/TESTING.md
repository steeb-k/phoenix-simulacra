# Carbon Phoenix — Testing Guide

Carbon Phoenix is a disk backup tool where a silent bug can mean **data loss**,
so testing is tiered and deliberately paranoid. Integrity is verified at three
levels: the backup file's internal consistency, the backup's fidelity to the
source, and the full source → backup → restore/clone round-trip byte-for-byte.

## Test tiers at a glance

| Tier | What | Where | Admin? | Runs in CI? |
|------|------|-------|--------|-------------|
| **T1** | Pure unit tests (no disks) | every lib crate + `#[test]` | no | yes |
| **T2** | System tests on virtual disks (VHDX via diskpart) | `phoenix-systests/tests/` | **yes** | elevated job |
| **T2-winfsp** | Zero-space WinFsp mount system test | `phoenix-systests/tests/winfsp_mount.rs` | **yes** | manual (needs WinFsp + libclang) |
| **T3-auto** | Destructive tests on a **real physical USB disk** | `phoenix-systests/tests/real_disk.rs` | **yes** | no (opt-in, real hardware) |
| **T3-manual** | Live system-disk VSS backup/clone + boot | `scripts/live-smoke-checklist.md` | **yes** | no (manual, pre-release) |

`--test-threads=1` is mandatory for every T2/T3 run: diskpart, disk-index
discovery, and drive-letter assignment are process-global and race in parallel.

---

## Tier 1 — unit tests (68+ tests, no admin)

Pure logic, no real disks. Run everywhere:

```bash
cargo test --workspace
```

Coverage highlights:
- **FAT parsing** — FAT12/16/32 cluster-count derivation, EOC-terminal cluster
  capture, 12-bit packed reads (regression for two data-loss bugs).
- **NTFS metadata** — `ntfs_meta` run-list parsing hardened against malformed
  bytes (never panics on untrusted input); truncation fuzz loop.
- **Container format v2** — footer CRC, total-length/truncation, index-table
  hash, per-entry CRC; a committed **corruption matrix** asserts each specific
  error variant (chunk flip, table flip, truncation at a chunk boundary,
  zeroed chunk count, garbage after footer, …).
- **Verify tiers** — `verify_structure` (no decompress), `verify_sampled`,
  full chunk re-hash; chunk-count mismatch is a hard error.
- **Restore/clone planning** — `pack_full_disk` layout (front-pack, anchor,
  clamp), overlap/oversize rejection, NTFS shrink relocation map.
- **Mount** — VHD footer bytes, GPT synthesis + CRCs, `SyntheticVhd` /
  `ChunkStore` on-demand reads (chunk-interior, straddle, gap→zeros, corrupt
  chunk → error).

---

## Tier 2 — virtual-disk system tests (elevated)

These create, attach, format, fill, back up, restore, clone, and mount **real
VHDX disks** via diskpart — they surface as ordinary `\\.\PhysicalDriveN`, so
the production engine runs unmodified against genuine NTFS/FAT/exFAT volumes,
with zero risk to the host's real disks. Every test hashes a deterministic
fixture tree (multi-chunk, fragmented) and re-checks it after the operation,
plus `chkdsk`.

Run all of them from an **elevated** PowerShell:

```powershell
.\scripts\run-system-tests.ps1
# or a single test:
.\scripts\run-system-tests.ps1 -Test ntfs_backup_restore_roundtrip_same_size
```

Or directly:

```powershell
cargo test -p phoenix-systests -- --ignored --test-threads=1 --nocapture
```

| File | Covers |
|------|--------|
| `backup_restore_roundtrip.rs` | NTFS backup → restore, same size, fixture + chkdsk |
| `clone.rs` | disk-to-disk clone, same-size and expand-to-larger |
| `fat_family.rs` | FAT32 and exFAT backup → restore round-trips |
| `resize_roundtrip.rs` | NTFS **grow** (`FSCTL_EXTEND_VOLUME`) and **shrink** (relocation + MFT/`$Bitmap`/`$LogFile` rewrite) |
| `mount.rs` | materialize-to-VHD mount (the dev fallback path) + browse fixture |

All disks in T2 are **GPT** (VHDX can't be MBR via this path) — MBR coverage
comes from T3.

### WinFsp zero-space mount test

The real (space-efficient) mount path is tested separately because it needs the
`winfsp` build feature, which needs **libclang** (LLVM) to build and **WinFsp**
installed to run:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # if LLVM isn't on PATH
cargo test -p phoenix-systests --features winfsp --test winfsp_mount -- --ignored --test-threads=1 --nocapture
```

`winfsp_mount.rs` serves a synthesized `backup.vhd` on demand from the `.phnx`
through WinFsp, attaches it read-only, and verifies the fixture through the
attached disk — i.e. it proves `AttachVirtualDisk` works over a user-mode
filesystem with **zero extra disk space**.

---

## Tier 3 (automated) — real physical USB disk

`phoenix-systests/tests/real_disk.rs` runs the whole backup/restore/clone matrix
against a **real, physical USB disk** — real sectors, real fragmentation, real
removable-media behavior, and **MBR** partition tables (the coverage gap T2's
all-GPT VHDs never touch). Fragmented real NTFS is what caught the
`div_ceil` phantom-cluster **silent data-loss** bug that the aligned VHD
fixtures could not.

### ⚠️ These tests WIPE the target disk. Safety model

They only run when explicitly opted in via `PHOENIX_T3_DISK`, and the harness
(`RealDisk::acquire`) **refuses** — before every destructive step — any disk
that is not USB, or is the boot/system disk, or is outside ~16–64 GB. Optionally
pin the exact device with `PHOENIX_T3_SERIAL`. If any gate fails it panics
without writing.

```powershell
$env:PHOENIX_T3_DISK    = "2"                        # the disk number to wipe
$env:PHOENIX_T3_SERIAL  = "04018bdbdd996130c3c9"     # optional exact-device pin
cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
```

Without `PHOENIX_T3_DISK` set, the tests skip cleanly (nothing is wiped).

| Scenario | Covers |
|----------|--------|
| `real_mbr_multifs_roundtrip` | MBR NTFS + FAT32 image → full-disk restore, NTFS auto-grow, partition-table + data |
| `real_mbr_restore_shrink` | NTFS relocation (shrink to 512 MB), chkdsk-clean |
| `real_mbr_exfat_roundtrip` | exFAT (raw capture) + NTFS round-trip |
| `real_clone_to_vhd` | clone the real disk → a VHD, read-back verify |

Every scenario runs with **verify-after-backup on** (re-reads the USB source and
confirms the backup matches before restoring).

Note: Windows won't make a removable USB flash drive GPT, so T3 uses MBR.
GPT-on-real-hardware needs a spare *fixed* disk (see `docs/ROADMAP.md`).

---

## Tier 3 (manual) — live system disk + boot

Some behavior can't be automated safely on a build machine: backing up and
cloning the **running** Windows system disk via VSS, and **booting** a cloned
disk. Work through `scripts/live-smoke-checklist.md` on real hardware before a
release. It covers live VSS backup/verify, restore-and-boot, live VSS clone,
resize restores, mount (WinFsp), and 4Kn / USB media.

---

## Integrity guarantees (what the tests protect)

- **`verify` command** — re-hashes every chunk against the per-chunk BLAKE3 in
  the manifest, plus format-v2 metadata checksums. Proves the `.phnx` is
  uncorrupted and internally consistent. `verify --quick` = structure + sample.
- **verify-after-backup** (default on) — immediately re-reads the **source**
  (VSS snapshot / volume lock still held) and confirms every chunk matches.
  Proves the image faithfully captured the disk. Opt out with `backup --no-verify`.
- **verify-on-restore** (default on) — re-hashes chunks before writing them.
- **Fixture round-trips** (T2/T3) — a known, independently-hashed fixture,
  compared after a full round-trip; validates the *engine's* correctness, which
  a self-referential `verify` structurally cannot.
- **Loud failure on short reads** — `capture_ntfs`/`capture_fat`/`capture_raw`
  error rather than silently drop a used extent that reads 0 bytes.

## Prerequisites

- **T1**: Rust stable.
- **T2/T3**: Rust + an **elevated** shell (diskpart, raw disk handles, formatting).
- **winfsp tests / feature**: LLVM (`libclang`, for `winfsp-sys` bindgen) + WinFsp
  installed (https://winfsp.dev).
- **T3-auto**: a spare USB disk you can fully erase.

## CI

`.github/workflows/windows.yml` runs fmt + clippy (`-D warnings`) + T1 on x64
(and builds aarch64), plus an elevated job for the T2 suite. The winfsp and T3
tiers are run manually on hardware.
