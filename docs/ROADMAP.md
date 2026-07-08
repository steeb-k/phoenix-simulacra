# Carbon Phoenix — Roadmap & Remaining Work

The engine is **feature-complete and validated on both virtual and real
hardware.** This document tracks what's left: known caveats, follow-up work, and
things noted during development that could use extra love. Nothing here blocks
using the tool; these are polish, packaging, and coverage-expansion items.

Branch: `feature/engine-completion`.

## What's done (for context)

Backup, restore (with NTFS grow + shrink-relocation, FAT/exFAT grow), disk-to-disk
clone (incl. live VSS), **verify-after-backup** (default on), format-v2 bulletproof
verification, and the **zero-space WinFsp mount**. Validated by the full T1/T2
suites and the real-disk (T3) matrix — see [TESTING.md](TESTING.md). Multiple
real integrity bugs were found and fixed along the way, including a silent
data-loss bug (`div_ceil` phantom cluster) that only real fragmented NTFS
exposed.

---

## Remaining work (prioritized)

### P1 — WinFsp installer bundling
The mount feature requires the WinFsp driver at run time. The binary already
delay-loads `winfsp-<arch>.dll` and finds it via the registry, so the app just
needs to **bundle the WinFsp MSI and silent-install it** (`msiexec /i winfsp.msi
/qn`) from its own installer — the standard redistribution pattern (rclone,
sshfs-win do this). WinFsp is GPLv3-with-FLOSS-exception, fine for an
open-source project. End users then never do a separate install. This is
packaging/installer work, not engine work. Until it's done, mount requires the
user to install WinFsp manually.

### P2 — GUI resize UX revamp (deferred "Phase 7")
The engine-first decision deferred this. The numeric partition-size entry is
already delivered; remaining polish:
- Snap-to-alignment drag handles (wider hit target than the current ~6 px).
- Surface the **minimum shrink size** from the same computation
  `build_relocation_map` uses, so the UI can't propose an impossible shrink.
- Live validation banner driven by `validate_layout`.
- Share one layout editor between the Restore and Clone pages.

### P2 — GPT on a real fixed disk
T3 covers **MBR** on real hardware; Windows won't make a removable USB flash
drive GPT, so real-hardware **GPT** validation needs a spare *fixed*
(non-removable) disk. GPT is already thoroughly covered by the all-GPT T2 VHD
suite, so this is confidence-on-real-hardware, not a correctness gap. When a
spare fixed disk is available, extend `real_disk.rs` with a GPT variant
(`RealDisk::layout(true, …)` already supports it).

### P3 — CI coverage for the winfsp + real-disk tiers
- The `winfsp` feature isn't built in CI (needs LLVM/`libclang` + WinFsp on the
  runner), so the mount code isn't clippy-checked or tested there. Add a CI job
  that installs both and runs `--features winfsp` clippy + the `winfsp_mount`
  test (WinFsp via `choco install winfsp`).
- The T3 real-disk tier is inherently manual (it wipes a physical disk); keep it
  as a documented pre-release step.

### P3 — Deeper structural validation in T3
Restore/shrink verification on the real USB drive relies on the fixture BLAKE3
digest + the volume mounting cleanly, because `chkdsk /scan` can't create a VSS
snapshot on a small/full volume. Add an **offline `chkdsk /F`** pass (dismount →
check) for a deeper structural check of shrunk/restored NTFS.

### P3 — Used-block exFAT capture
exFAT is currently captured **raw** (the whole partition), because exFAT's
`NoFatChain` contiguous files aren't discoverable from the FAT alone. This makes
exFAT backups larger/slower than they need to be. Capturing via the exFAT
**allocation bitmap** would give used-block sizing like NTFS/FAT. Correctness is
fine today; this is an efficiency improvement.

### P3 — Automate the manual T3 items (where safe)
`scripts/live-smoke-checklist.md` covers live VSS system-disk backup/clone,
boot-the-clone, and 4Kn media manually. The live-system and boot checks are
hard to automate safely; **4Kn media** could be automated if a 4Kn test device
(or a 4Kn-emulating VHD) is available.

### Nice-to-have
- **Progress step for verify-after-backup** — it currently shows as a detail
  message under "Finalizing"; a formal step in the GUI progress checklist would
  make the (read-doubling) verify pass visible.

---

## Known caveats (documented, by design)

- **Mount without the `winfsp` feature** falls back to materializing a
  full-size temp VHD — a **dev-only stopgap**. It's never used in a `winfsp`
  build; a `winfsp` build that can't reach WinFsp errors rather than silently
  doubling disk usage. Mounting must never double a backup's footprint.
- **GPT on removable USB** is not possible (Windows policy), hence MBR-only T3.
- **verify-after-backup roughly doubles source read time** (it re-reads the
  source). On by default; opt out with `backup --no-verify` when speed matters.
- **BitLocker-locked volumes** are captured raw only (must be unlocked for
  file-aware capture), as today.
- **Restore to dissimilar hardware** may need Windows Startup Repair / `bcdedit`
  (expected; noted in the live checklist).

## Out of scope (explicit)

Incrementals / differentials (the v2 format keeps reserved fields for them),
cloud / sync / scheduling, ReFS, and file-aware capture of BitLocker-locked
volumes.
