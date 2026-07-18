# Testing

Phoenix Simulacra is a disk backup tool where a silent bug can mean **data loss**, so testing is tiered and deliberately paranoid. Integrity is verified at three levels: the backup file's internal consistency, the backup's fidelity to the source, and the full source → backup → restore/clone round-trip byte-for-byte.

There is **no CI** — every tier below is run locally; the elevated dev box is the gate.

## Test tiers at a glance

| Tier | What | Where | Admin? |
|------|------|-------|--------|
| **T1** | Pure unit tests (no disks) | every lib crate `#[test]` | no |
| **T2** | System tests on virtual disks (VHDX via diskpart) | `phoenix-systests/tests/` | **yes** |
| **T2-4Kn** | Backup/restore on a true **4096-byte-sector** virtual disk | `phoenix-systests/tests/sector_4kn.rs` | **yes** |
| **T2-winfsp** | Zero-space WinFsp mount system tests | `phoenix-systests/tests/winfsp_mount.rs` | **yes** |
| **T3-auto** | Destructive backup/restore on a **real physical USB disk** | `phoenix-systests/tests/real_disk.rs` | **yes** |
| **T3-clone** | The **clone matrix between two real disks** (wipes both) | `phoenix-systests/tests/real_clone.rs` + `scripts/run-real-clone.ps1` | **yes** |
| **T3B** | Boot-disk cloning from the LIVE system disk | `phoenix-systests/tests/boot_disk.rs` | **yes** |
| **T3-manual** | Live system-disk VSS backup/clone + boot | `scripts/live-smoke-checklist.md` | **yes** |

`--test-threads=1` is mandatory for every T2/T3 run: diskpart, disk-index discovery, and drive-letter assignment are process-global and race in parallel.

### Run the T2 suite staged, one binary at a time

A single one-shot `cargo test -p phoenix-systests` run attaches and detaches enough VHDs to exhaust the Windows storage stack — observed to hard-hang a machine after hundreds of cumulative attach/detach cycles in one uptime. It is not any specific test; it is the churn. Drive the suite as one cargo process per test binary, with a short settle between:

```powershell
foreach ($b in @('resize_roundtrip','sector_4kn','sector_convert','backup_restore_roundtrip','clone',
                 'fat_family','gpt_identity','partial_mbr','partial_clone','mount','vss','bitlocker',
                 'winfsp_mount')) {
    cargo test -p phoenix-systests --features winfsp --test $b -- --ignored --test-threads=1
    Start-Sleep -Seconds 5   # VHD detach is asynchronous; let it finish
}
```

`scripts/run-system-tests.ps1` handles this (and the winfsp build flags) for you.

### Known flaky test

`vss.rs :: locked_backup_blocks_writers_for_duration` fails intermittently: it races a writer thread against a backup's lock window, and when the backup finishes fast enough no write attempt lands inside the window. It is a test-timing bug, not an engine bug — the lock itself is deterministically proven by `volume_lock_blocks_external_writes`. Re-run it before believing a red.

---

## Tier 1 — unit tests (no admin)

Pure logic, no real disks. Run the **engine crates** — not `--workspace`:

```powershell
cargo test -p phoenix-core -p phoenix-capture -p phoenix-restore `
           -p phoenix-clone -p phoenix-mount -p phoenix-vss
```

`cargo test --workspace` fails with `os error 740` ("requires elevation"): the `phoenix-cli` and `phoenix-gui` test binaries inherit a `requireAdministrator` manifest, so cargo cannot launch them from a normal shell. Use the crate list above, or an elevated shell (which also picks up the `#[ignore]`d T2/T3 tests).

Coverage highlights:

- **FAT parsing** — FAT12/16/32 cluster-count derivation, EOC-terminal cluster capture, 12-bit packed reads (regression tests for two data-loss bugs).
- **exFAT allocation bitmap** — used-cluster extents derived from the bitmap; a synthetic-image test proves a `NoFatChain` contiguous file (absent from the FAT) is captured.
- **NTFS metadata** — run-list parsing hardened against malformed bytes (never panics on untrusted input); truncation fuzz loop.
- **Container format v2** — footer CRC, total-length/truncation, index-table hash, per-entry CRC; a committed corruption matrix asserts each specific error variant.
- **Verify tiers** — `verify_structure`, `verify_sampled`, full chunk re-hash; chunk-count mismatch is a hard error.
- **Restore/clone planning** — full-disk layout packing, overlap/oversize rejection, NTFS shrink relocation map.
- **Mount** — VHD/VHDX synthesis, GPT synthesis + CRCs, on-demand chunk reads (straddles, gaps→zeros, corrupt chunk → error).
- **Parallel chunk pipeline** — the pipelined capture writer is proven byte-equivalent to a serial reference layout; parallel decode preserves order and propagates errors; the double-buffered clone loop matches a reference image and cancels without deadlock.

---

## Tier 2 — virtual-disk system tests (elevated)

These create, attach, format, fill, back up, restore, clone, and mount **real VHDX disks** via diskpart — they surface as ordinary `\\.\PhysicalDriveN`, so the production engine runs unmodified against genuine NTFS/FAT/exFAT volumes, with zero risk to the host's real disks. Every test hashes a deterministic fixture tree (multi-chunk, fragmented) and re-checks it after the operation, plus `chkdsk`.

```powershell
.\scripts\run-system-tests.ps1
# or a single test:
.\scripts\run-system-tests.ps1 -Test ntfs_backup_restore_roundtrip_same_size
```

| File | Covers |
|------|--------|
| `backup_restore_roundtrip.rs` | NTFS backup → restore, same size, fixture + chkdsk |
| `clone.rs` | disk-to-disk clone, same-size and expand-to-larger |
| `fat_family.rs` | FAT32 and exFAT round-trips; asserts both capture used-blocks and the backup is materially smaller than the partition |
| `resize_roundtrip.rs` | NTFS **grow** (`FSCTL_EXTEND_VOLUME`) and **shrink** (relocation + MFT/`$Bitmap`/`$LogFile` rewrite) |
| `mount.rs` | materialize-to-VHD mount (the dev fallback path) + browse fixture |
| `bitlocker.rs` | full BitLocker lifecycle (needs Windows Pro+): unlocked volume round-trips as a normal plaintext backup; locked volume backs up as ciphertext, restores locked, and yields the fixture only after `Unlock-BitLocker` |
| `gpt_identity.rs` | restore preserves the source's disk GUID, partition unique GUIDs, and attribute bits (the identities the BCD uses) — proven collision-free by detaching the source VHD before restoring |
| `partial_mbr.rs` | partial restores that rewrite the partition table: MBR shrunk-slot rewrite with a preserved sibling surviving byte-identical, deleting a live partition from the plan, and re-initializing an MBR disk as GPT |
| `partial_clone.rs` | partial clone (`UpdateExisting`, the Clone page's default): replace one slot and keep the sibling, shrink NTFS into a smaller slot, drop a live partition. The preserved sibling is checked block-wise, not hashed whole — it stays mounted through the clone, so NTFS drifts its own metadata mid-volume; an engine overspill would land at block 0. The fixture digest and `chkdsk` settle it. |
| `vss.rs` | the lock-then-VSS escalation pinned on all three arms, with VSS proven working rather than merely not-erroring: point-in-time shadow semantics; a backup escalating to a shadow read (proven by succeeding with a file handle held open, which no lock survives); an unfreezable FAT32 volume aborting rather than smearing a live read; idle FAT32 locking and round-tripping; and outbound lock exclusivity under a concurrent writer hammer |
| `sector_convert.rs` | 4Kn→512e conversion on restore: NTFS convert+grow, FAT32 convert, the 512e→4Kn refusal, the opt-in gate, per-partition partial restore — converted volumes mount and pass `chkdsk` |

Most T2 disks are GPT; `partial_mbr.rs` lays its fixtures out as MBR, so T2 covers MBR partial-restore table rewrites too. Full-disk MBR round-trips on real media are T3.

### WinFsp zero-space mount tests

The real (space-efficient) mount path needs the `winfsp` build feature, which needs libclang (LLVM) to build and WinFsp installed to run. `run-system-tests.ps1` sets this up automatically, or run the mount tests alone:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # if LLVM isn't on PATH
cargo test -p phoenix-systests --features winfsp --test winfsp_mount -- --ignored --test-threads=1 --nocapture
```

`winfsp_mount.rs` proves the zero-space path end to end: a synthesized `backup.vhd` served on demand from the `.phnx` through WinFsp, attached read-only, fixture verified through the attached disk — plus partition-selective mounting (only selected partitions get drive letters, removed again on unmount) and mounting a 4Kn backup (served as a VHDX so Windows sees 4096-byte sectors).

---

## Tier 2-4Kn — 4096-byte-sector virtual disks

True 4Kn disks can be synthesized on any dev box — no special hardware, no particular Windows edition. diskpart can't set a sector size and Hyper-V's `New-VHD` needs a Pro SKU, so the fixture (`TestVhd::create_4kn`) calls `CreateVirtualDisk` (virtdisk.dll) directly with Version2 parameters, which carry the sector sizes. VHDX is required — the older VHD format is 512-sector-only.

```powershell
cargo test -p phoenix-systests --test sector_4kn -- --ignored --test-threads=1 --nocapture
```

| Test | Covers |
|------|--------|
| `vhdx_4kn_reports_4096_byte_sectors` | The fixture itself: the attached disk enumerates with `sector_size == 4096`. A silently-512 fixture would make every conclusion below worthless, so it fails loudly instead. |
| `ntfs_4kn_backup_verifies_against_source` | NTFS capture off a 4Kn disk with verify-after-source and a full image verify, plus used-block planning and 4Kn geometry in the manifest. |
| `ntfs_4kn_backup_restore_roundtrip` | The full round-trip onto a second 4Kn disk: drive letter, `chkdsk` clean, fixture byte-identical. Covers the GPT reserve, usable offsets, extend-volume sector counts, and the NTFS MFT fixup stride. |
| `ntfs_4kn_clone_same_size_and_expanded` | Disk-to-disk clone between two 4Kn disks, same-size and expand-to-fill (clone reaches the geometry math by a different route than restore). |
| `ntfs_4kn_clone_shrinks_into_a_smaller_slot` | Clone + shrink on 4Kn — the only path that rewrites NTFS metadata in place, and where 4Kn hid its nastiest bug: NTFS on 4Kn reports 4096-byte sectors but still writes 1024-byte MFT records with a 512-byte fixup stride. A wrong stride writes a subtly corrupt filesystem, so `chkdsk` **and** the fixture digest must both agree. |
| `winfsp_mount_a_4kn_backup` | Mounting a 4Kn backup (in `winfsp_mount.rs`): served as VHDX, Windows reports 4096-byte sectors, the volume mounts, the fixture verifies. |
| `vhdx_container.rs` | Does **Windows** accept the VHDX we hand-synthesize? Attaches the bytes and asks `Get-Disk` for the sector size, at 4096 *and* 512. |
| `vhdx_diff.rs` | Diagnostic: dumps the structure of a Windows-built VHDX beside ours. This is what found the one bit that made Windows reject an otherwise perfect container; the next VHDX bug will be found the same way. |

A fixture only covers the code you point it at: the synthesized-4Kn tier caught six geometry bugs, and real 4Kn hardware later still caught one more (an enumeration-path read no T2 test exercised). "The surface is covered" is a claim about the tests, not the hardware.

---

## Tier 3 (automated) — real physical USB disk

`phoenix-systests/tests/real_disk.rs` runs the whole backup/restore/clone matrix against a real, physical USB disk — real sectors, real fragmentation, real removable-media behavior, and **MBR** partition tables (which T2's all-GPT VHDs never touch). Fragmented real NTFS is what caught a `div_ceil` phantom-cluster **silent data-loss** bug that aligned VHD fixtures could not.

### These tests WIPE the target disk — safety model

They only run when explicitly opted in via `PHOENIX_T3_DISK`, and the harness (`RealDisk::acquire`) refuses — before every destructive step — the boot/system disk, an out-of-size disk, or (by default) any non-removable disk. Optionally pin the exact device with `PHOENIX_T3_SERIAL`. If any gate fails it panics without writing.

```powershell
$env:PHOENIX_T3_DISK    = "2"              # the disk number to wipe
$env:PHOENIX_T3_SERIAL  = "<serial>"       # optional exact-device pin
cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
```

Without `PHOENIX_T3_DISK` set, the tests skip cleanly.

**Targeting a fixed disk (for GPT).** Windows won't make removable media GPT, so the GPT scenario needs a fixed disk — allowed only under a stricter opt-in: `PHOENIX_T3_ALLOW_FIXED=1` **and** a mandatory `PHOENIX_T3_SERIAL`. Size bounds default to 16–64 GB and widen via `PHOENIX_T3_MIN_GB` / `PHOENIX_T3_MAX_GB`. The boot/system-disk refusal always applies. `PHOENIX_T3_LAYOUT_GB` caps how far the restore lays partitions into a big disk so `chkdsk` on a grown multi-terabyte volume doesn't dominate the run (GPT coverage is unaffected — the table still spans the whole disk).

| Scenario | Covers |
|----------|--------|
| `real_mbr_multifs_roundtrip` | MBR NTFS + FAT32 image → full-disk restore, NTFS auto-grow, offline `chkdsk /F` |
| `real_gpt_multifs_roundtrip` | The same round-trip on a **GPT** disk (fixed-disk opt-in; skips cleanly on a USB target) |
| `real_mbr_restore_shrink` | NTFS relocation (shrink), online + offline structural checks |
| `real_mbr_exfat_roundtrip` | exFAT (used-block capture via allocation bitmap) + NTFS round-trip |
| `real_mbr_bitlocker_roundtrip` | BitLocker on real flash: unlocked → plaintext; locked → ciphertext, proven by reading `-FVE-FS-` off the physical disk |
| `real_vss_backup_roundtrip` | the lock-then-VSS escalation on real media, adaptive: non-removable must escalate to a shadow and succeed; unshadowable removable flash must abort rather than smear a live read |
| `real_clone_to_vhd` | clone the real disk → a VHD, read-back verify |

Every scenario runs with verify-after-backup on.

---

## Tier 3-clone — the clone matrix between TWO real disks

`phoenix-systests/tests/real_clone.rs` runs every clone mode the Clone page can reach, between two real physical disks. **Both are wiped.**

```powershell
.\scripts\run-real-clone.ps1 -SourceDisk 2 -TargetDisk 3 -AllowFixed `
    -SourceSerial "<flash serial>" -TargetSerial "<hdd serial>" -MaxGB 4200
# -WhatIf shows the plan and the resolved drives, and writes nothing.
```

The script prints both drives' model and serial before touching anything and makes you type the target's disk number to proceed. It runs one cargo process per scenario with a settle between — staged, for the reason at the top of this document.

Both disks pass the full `RealDisk` gate, source included — the source is read *and written* (each test lays its own fixture on it). Each disk carries its own serial pin (`PHOENIX_T3_SRC_SERIAL` / `PHOENIX_T3_SERIAL`).

| Scenario | Covers |
|----------|--------|
| `real_clone_full_mbr_source_to_gpt_target` | Full clone with a table-style switch: MBR → GPT |
| `real_clone_full_match_source_keeps_mbr` | `ReinitMatchSource` keeps a >2 TiB target MBR, where the table's 2 TiB limit is not hypothetical |
| `real_clone_grow_ntfs_into_larger_slot` | Clone + grow |
| `real_clone_shrink_ntfs_into_smaller_slot` | Clone + shrink (relocation + MFT rewrite) on real fragmented NTFS |
| `real_partial_clone_preserves_the_sibling` | Partial clone into a live partition table: one slot rewritten, the mounted sibling untouched |
| `real_clone_back_from_hdd_to_flash` | The reverse trip, onto removable media |

The full matrix has been validated on real hardware (USB flash → multi-TB external HDD and back).

---

## Tier 3B (automated) — boot-disk cloning from the LIVE system disk

`phoenix-systests/tests/boot_disk.rs` images the machine's **running boot disk** (read-only, via VSS) and restores it to the opt-in test disk in a matrix of layouts. The source disk is never written; every destructive step goes through the standard `RealDisk` gate, plus a check that the image file doesn't live on the target disk.

```powershell
$env:PHOENIX_BOOT_SRC_DISK = "0"                # imaged read-only via VSS
$env:PHOENIX_BOOT_IMG_DIR  = "D:\phoenix-boot"  # .phnx location (not on src/target!)
# target = the standard T3 gate (non-removable disks need ALLOW_FIXED + SERIAL)
cargo test -p phoenix-systests --test boot_disk -- --ignored --test-threads=1 --nocapture
```

| Stage | Covers |
|-------|--------|
| `boot_a_capture_image` | live VSS capture of the whole boot disk with verify-after + independent full verify; boot-clone fidelity fields in the manifest |
| `boot_b_restore_asis` | full-disk restore at original sizes; GUIDs + attributes match; `\Windows` artifacts + ESP BCD present; offline chkdsk |
| `boot_c_restore_shrink` | main NTFS shrunk (relocation); all other partitions keep exact size and order |
| `boot_d_restore_grow` | main NTFS grown; trailing partitions move but keep exact size |
| `boot_e_partial_ntfs` | partial restore: lock+dismount the target's mounted NTFS slot, restore only the NTFS; all other partitions byte-identical before/after |

**Boot-clone fidelity:** backups record each GPT partition's unique GUID and attribute bits, and restore writes them back — the BCD references boot partitions by unique GUID, so this is what makes a restored disk bootable without repair.

**Same-machine GUID dedup (measured):** Windows forbids duplicate GPT identities among online disks. Restoring while the source disk is attached makes Windows regenerate the clone's GUIDs when it comes online, so the fidelity check treats a mismatch as failure only when no other online disk holds the expected GUID (collision-free preservation is proven by T2 `gpt_identity.rs`). A clone made with the source attached therefore needs a BCD fixup to boot — which is exactly what the built-in Boot Repair does.

**Drive letters:** the T2 fixtures hard-require letter `X:` (real-disk tests use `R:`/`S:`). The harness pre-flights each letter: stale mount-manager mappings are dropped automatically, but a letter held by a live volume fails fast with a message naming the owner. Park a stray auto-mounted test disk with `Set-Disk -Number <n> -IsOffline $true`.

**Timing / perf log:** engine operations print per-phase elapsed + throughput, and one CSV row per operation is appended to `target/perf-log.csv` (`PHOENIX_PERF_LOG` overrides). Use these numbers when judging throughput changes.

## Tier 3 (manual) — live system disk + boot

Some behavior can't be automated safely on a build machine: backing up and cloning the running Windows system disk via VSS, and **booting** a cloned disk. Work through `scripts/live-smoke-checklist.md` on real hardware before a release.

---

## Integrity guarantees (what the tests protect)

- **`verify` command** — re-hashes every chunk against the per-chunk BLAKE3 manifest, plus format-v2 metadata checksums. `verify --quick` = structure + sample.
- **verify-after-backup** (default on) — the CLI and the test suites immediately re-read the **source** (snapshot/lock still held) and confirm every chunk matches, proving the image faithfully captured the disk; opt out with `backup --no-verify`. The GUI releases the source first and instead decompresses and BLAKE3-checks every chunk of the written image — the same check as a standalone full verify.
- **verify-on-restore** (default on) — re-hashes chunks before writing them.
- **Fixture round-trips** (T2/T3) — a known, independently-hashed fixture compared after a full round-trip; validates the *engine's* correctness, which a self-referential `verify` structurally cannot.
- **Loud failure on short reads** — capture errors rather than silently dropping a used extent that reads 0 bytes.
- **Torn-read handling** — reading a VSS shadow device while the live volume takes writes can transiently return a wrong read once for a freshly copied-on-write block; verify-after re-reads any mismatched chunk, accepts a re-read that matches the recorded hash (warned and counted), and still fails loudly on stable divergence.

## Prerequisites

- **T1**: Rust stable.
- **T2/T3**: Rust + an **elevated** shell (diskpart, raw disk handles, formatting).
- **winfsp tests / feature**: LLVM (`libclang`) + WinFsp installed (https://winfsp.dev).
- **T2-4Kn**: nothing beyond T2 — the fixture uses core-Windows `CreateVirtualDisk` (no Hyper-V, no Pro SKU).
- **T3-auto**: a spare USB disk you can fully erase.
- **BitLocker tests**: a Windows SKU with the BitLocker cmdlets (Pro/Enterprise). Data volumes use a password protector, so no TPM is needed.

## The local gate

The gate before a commit that touches the engine, in order:

```powershell
cargo fmt --all --check
cargo clippy --workspace -- -D warnings          # the tree is clippy-clean; keep it that way
cargo test -p phoenix-core -p phoenix-capture -p phoenix-restore `
           -p phoenix-clone -p phoenix-mount -p phoenix-vss    # T1
.\scripts\run-system-tests.ps1                   # T2, elevated
```

T3 / T3B / the manual checklist are pre-release steps on real hardware. Release binaries are built with `scripts/build-release.ps1`.
