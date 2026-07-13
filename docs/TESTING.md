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
| **T2-winfsp** | Zero-space WinFsp mount system tests (incl. partition selection) | `phoenix-systests/tests/winfsp_mount.rs` | **yes** | elevated job (CI installs WinFsp) |
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
- **exFAT allocation bitmap** — `exfat_plan` locates the bitmap via the root
  directory and derives used-cluster extents from it; a synthetic-image test
  proves a `NoFatChain` contiguous file (absent from the FAT) is captured.
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
- **Parallel chunk pipeline** (`pipeline_equivalence.rs`,
  `restore_parallel_roundtrip.rs`, `stream_extents_overlap.rs`) — the
  pipelined capture writer is byte-equivalent to a serially-computed
  reference layout (offsets, compressed bytes, record order, stream handoff
  between partitions); parallel decode preserves order, attributes
  `HashMismatch` correctly, and propagates consumer errors; `restore_raw`
  round-trips into a sentinel-filled temp file proving extent-only writes
  and fails on a corrupted chunk; the double-buffered clone loop matches a
  reference image (plain, ReadBack, relocation split) and cancels without
  deadlock. An `#[ignore]`d `pipeline_throughput` probe prints MB/s
  (`cargo test -p phoenix-core --release pipeline_throughput -- --ignored
  --nocapture`).

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
| `fat_family.rs` | FAT32 and exFAT backup → restore round-trips; asserts both capture **used-blocks** (exFAT via the allocation bitmap) and the backup is materially smaller than the partition |
| `resize_roundtrip.rs` | NTFS **grow** (`FSCTL_EXTEND_VOLUME`) and **shrink** (relocation + MFT/`$Bitmap`/`$LogFile` rewrite) |
| `mount.rs` | materialize-to-VHD mount (the dev fallback path) + browse fixture |
| `bitlocker.rs` | full BitLocker lifecycle (needs Windows Pro+): encrypt with a password protector → **unlocked** volume classifies NTFS/used-blocks/`Unlocked` and round-trips as a normal *plaintext* backup → `Lock-BitLocker` → classifies Bitlocker/raw/`Locked`, backup is *ciphertext* (verify-after passes), restore comes back locked and yields the fixture only after `Unlock-BitLocker` with the original password |
| `gpt_identity.rs` | restore preserves the source's **disk GUID, partition unique GUIDs, and attribute bits** (the identities the BCD uses) — proven collision-free by detaching the source VHD before restoring |
| `partial_mbr.rs` | partial restores that **rewrite the partition table**: MBR shrunk-slot rewrite with a preserved sibling surviving byte-identical, deleting a live partition from the plan (its mounted volume locked + dismounted first), and a `reinit_style = "gpt"` plan re-initializing an MBR disk as GPT — the engine paths behind the GUI layout editor's replace/delete/blank/style-switch actions |
| `vss.rs` | the **lock-then-VSS escalation** (no user switch) pinned on all three arms, with VSS **proven working, not just not-erroring** (`snapshot_volume`'s failure fallback is silent, so naive tests can pass with VSS broken): (1) point-in-time — snapshot, modify a file, read the *original* bytes through the shadow device; (2) backup of an NTFS volume **with a file handle held open** — no lock can survive that, so only a real escalation to a shadow read can succeed, then restore+verify; (3) **no second arm** via FAT32 (VSS is NTFS-only) + a held handle: unfreezable, so the backup must *abort* with a lock error rather than smear a live read; (4) idle FAT32: must lock → capture → unlock → restore byte-for-byte; (5+6) **outbound lock exclusivity** — while the lock is held (primitive-level, and for a whole locked backup's duration under a concurrent writer hammer) external file creates/opens on the volume must be refused, and allowed again after release |

Most T2 disks are **GPT**; `partial_mbr.rs` lays its VHDX fixtures out as MBR
(`diskpart clean` + `convert mbr` works fine on an attached VHDX), so T2 now
covers MBR partial-restore table rewrites too. Full-disk MBR round-trips on
real media remain T3.

### WinFsp zero-space mount tests

The real (space-efficient) mount path needs the `winfsp` build feature, which
needs **libclang** (LLVM) to build and **WinFsp** installed to run.
`run-system-tests.ps1` builds with the feature automatically (it points
`LIBCLANG_PATH` at the standard LLVM install), or run the mount tests alone:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # if LLVM isn't on PATH
cargo test -p phoenix-systests --features winfsp --test winfsp_mount -- --ignored --test-threads=1 --nocapture
```

`winfsp_mount.rs` has two tests:

- `winfsp_mount_and_browse_files` serves a synthesized `backup.vhd` on demand
  from the `.phnx` through WinFsp, attaches it read-only, and verifies the
  fixture through the attached disk — i.e. it proves `AttachVirtualDisk` works
  over a user-mode filesystem with **zero extra disk space**.
- `winfsp_mount_selected_partition_only` backs up a two-partition disk and
  mounts it with only the second partition selected: exactly that partition
  gets a (new) drive letter, its fixture verifies, the unselected sibling
  surfaces no letter, and the letter is removed again on unmount — the engine
  path behind the GUI Mount page's partition checkboxes and the CLI's
  `mount --partitions`.

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
(`RealDisk::acquire`) **refuses** — before every destructive step — the
boot/system disk, an out-of-size disk, or (by default) any non-removable disk.
Optionally pin the exact device with `PHOENIX_T3_SERIAL`. If any gate fails it
panics without writing.

```powershell
$env:PHOENIX_T3_DISK    = "2"                        # the disk number to wipe
$env:PHOENIX_T3_SERIAL  = "04018bdbdd996130c3c9"     # optional exact-device pin
cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
```

Without `PHOENIX_T3_DISK` set, the tests skip cleanly (nothing is wiped).

**Targeting a FIXED disk (for GPT).** Windows won't make removable media GPT, so
the GPT scenario needs a fixed (non-removable) disk — which the gate allows only
under a stricter opt-in: set `PHOENIX_T3_ALLOW_FIXED=1` **and** pin the exact
`PHOENIX_T3_SERIAL` (mandatory here, since a fixed disk has no throwaway-USB
safety net). Size bounds default to 16–64 GB and widen via `PHOENIX_T3_MIN_GB` /
`PHOENIX_T3_MAX_GB`. The boot/system-disk refusal always applies.

```powershell
$env:PHOENIX_T3_DISK        = "3"
$env:PHOENIX_T3_ALLOW_FIXED = "1"
$env:PHOENIX_T3_SERIAL      = "<exact-serial>"   # MANDATORY for a fixed disk
$env:PHOENIX_T3_MAX_GB      = "4100"             # widen if the disk is >64 GB
$env:PHOENIX_T3_LAYOUT_GB   = "16"               # cap restore layout (see below)
cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture real_gpt_multifs_roundtrip
```

`PHOENIX_T3_LAYOUT_GB` caps how far the restore lays partitions into a big disk
so NTFS doesn't auto-grow to fill (e.g.) 4 TB and drag out the final `chkdsk`.
It does **not** reduce GPT coverage — the partition table still spans the whole
disk (the backup GPT header is written at the disk's true last LBA). None of the
scenarios image the full disk regardless: they use small fixed-size partitions
(a few GB) and capture used-blocks, so the unallocated remainder is never
touched. Leave it unset on a small (USB-stick) disk to exercise grow-to-fill.

| Scenario | Covers |
|----------|--------|
| `real_mbr_multifs_roundtrip` | MBR NTFS + FAT32 image → full-disk restore, NTFS auto-grow, partition-table + data, offline `chkdsk /F` on the restored NTFS |
| `real_gpt_multifs_roundtrip` | Same round-trip on a **GPT** disk — real-hardware GPT partition-table read/write. Needs a fixed disk (see opt-in above); skips cleanly on a USB target |
| `real_mbr_restore_shrink` | NTFS relocation (shrink to 512 MB), online `chkdsk /scan` + offline `chkdsk /F /X` structural check |
| `real_mbr_exfat_roundtrip` | exFAT (used-block capture via allocation bitmap) + NTFS round-trip |
| `real_mbr_bitlocker_roundtrip` | BitLocker on real flash: unlocked → plaintext backup/restore; locked → ciphertext backup/restore, ciphertext proven by reading `-FVE-FS-` off the physical disk (the in-session OS unlock leg is best-effort — Windows won't re-recognize a restored removable BitLocker volume without a re-plug/reboot) |
| `real_vss_backup_roundtrip` | the lock-then-VSS escalation on real media, adaptive — both legs hold a file handle open, which is what makes the lock fail: non-removable → the engine must escalate to a shadow and succeed (only a real shadow read passes) + restore/verify; removable flash (unshadowable) → no second arm, so the backup must abort with a lock error, then a locked round-trip with handles closed |
| `real_clone_to_vhd` | clone the real disk → a VHD, read-back verify |

Every scenario runs with **verify-after-backup on** (re-reads the source and
confirms the backup matches before restoring).

---

## Tier 3B (automated) — boot-disk cloning from the LIVE system disk

`phoenix-systests/tests/boot_disk.rs` images this machine's **running boot
disk** (read-only, via VSS) and restores it to the opt-in test disk in a
matrix of layouts. The source disk is never written; every destructive step
goes through the standard `RealDisk` gate (which refuses boot/system disks),
plus a check that the image file doesn't live on the target disk.

```powershell
$env:PHOENIX_BOOT_SRC_DISK = "0"                # imaged read-only via VSS
$env:PHOENIX_BOOT_IMG_DIR  = "D:\phoenix-boot"  # .phnx location (not on src/target!)
# target = the standard T3 gate (non-removable disks need ALLOW_FIXED + SERIAL)
cargo test -p phoenix-systests --test boot_disk -- --ignored --test-threads=1 --nocapture
```

| Stage | Covers |
|-------|--------|
| `boot_a_capture_image` | live VSS capture of the whole boot disk (EFI/MSR/OS/Recovery) with verify-after + independent full verify; asserts GPT + boot-clone fidelity fields (partition unique GUIDs + attribute bits) landed in the manifest |
| `boot_b_restore_asis` | full-disk restore, every partition at its ORIGINAL size; target must be GPT; type/unique GUIDs + attributes match; \Windows artifacts + ESP BCD present; offline chkdsk |
| `boot_c_restore_shrink` | main NTFS shrunk to used+25% (relocation); all other partitions keep exact size and order |
| `boot_d_restore_grow` | main NTFS grown (default plan); trailing partitions (Recovery) move but keep exact size |
| `boot_e_partial_ntfs` | partial restore: lock+dismount the target's existing (mounted) NTFS slot, then plop only the NTFS from the image over it; all other partitions byte-identical before/after |

**Boot-clone fidelity:** backups record each GPT partition's **unique GUID**
(`PartitionId`) and **attribute bits** in the manifest, and restore writes
them back — the BCD references boot partitions by unique GUID, so this is
what makes a restored disk actually bootable without Startup Repair.

**Same-machine GUID dedup (measured, not theoretical):** Windows forbids
duplicate GPT identities among online disks. Restoring while the source disk
is attached makes Windows regenerate the clone's disk GUID **and every
partition unique GUID** as a sequential batch when the disk comes online —
the engine wrote the source identities; the OS replaced them. The T3B
fidelity check therefore treats a unique-GUID mismatch as a failure **only
when no other online disk holds the expected GUID**; collision-free
preservation is proven by T2 `gpt_identity.rs` (source detached first).
Consequence for real clones made with the source attached: the clone's BCD
still references the ORIGINAL GUIDs, so booting it requires detaching the
source or a post-restore BCD fixup (see ROADMAP). Booting the clone with
BOTH disks attached would chain-load the ORIGINAL Windows volume.

**Re-running just the checks:** `PHOENIX_BOOT_SKIP_RESTORE=1` makes a stage
validate the target disk's EXISTING layout instead of re-restoring — for
finishing a stage whose multi-hour restore succeeded but whose assertions
tripped. Only meaningful when the disk currently holds that stage's layout.
**Unset it before a full run** — with it set, every stage after the one
matching the disk's current layout fails by design (and no restore code is
actually exercised).

**Drive letters:** the T2 VHD fixtures hard-require letter `X:` (real-disk
tests use `R:`/`S:`). The harness pre-flights each requested letter: a stale
mount-manager mapping left by a crashed run is dropped automatically
(`mountvol <L>: /D`), but a letter held by a LIVE volume — typically a
restored test-clone disk that came online and auto-mounted — fails fast with
a message naming the owner (letters held by the target disk itself are fine;
the layout wipes it anyway). Park such a disk with
`Set-Disk -Number <n> -IsOffline $true` before running the suites — if the
parked disk IS the `PHOENIX_T3_DISK` target, `RealDisk::acquire` brings it
back online automatically (the env opt-in is the consent).

**Timing / perf log:** every `ConsoleProgress`-wrapped engine operation
prints per-phase and total elapsed + throughput lines, each T3B stage prints
its wall-clock at PASSED, and one CSV row per operation
(`timestamp,label,elapsed_s,bytes,mb_per_s,workers`) is appended to
`target/perf-log.csv` (override with `PHOENIX_PERF_LOG=<path>`; the default
lives under `target/`, so `cargo clean` wipes it). The engine itself also
logs elapsed + effective MB/s at the end of `run_backup` (capture and
verify phases separately), `run_restore`, and `run_clone` via `tracing`, so
real CLI/GUI runs are measurable too. Use these numbers — not memory of how
long the last run felt — when judging throughput changes.

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
- **BitLocker tests** (T2 `bitlocker.rs`, T3 `real_mbr_bitlocker_roundtrip`):
  a Windows SKU with the BitLocker cmdlets (Pro/Enterprise). Data volumes use
  a password protector, so no TPM or group-policy change is needed.

## CI

`.github/workflows/windows.yml` runs fmt + clippy (`-D warnings`) + T1 on x64
(and builds aarch64), plus an elevated job for the T2 suite. The winfsp and T3
tiers are run manually on hardware.
