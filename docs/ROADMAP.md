# Carbon Phoenix — Roadmap & Remaining Work

The engine is **feature-complete and validated on both virtual and real
hardware.** This document tracks what's left: known caveats, follow-up work, and
things noted during development that could use extra love. Nothing here blocks
using the tool; these are polish, packaging, and coverage-expansion items.

Branch: `feature/engine-completion`.

## What's done (for context)

Backup, restore (with NTFS grow + shrink-relocation, FAT/exFAT grow), disk-to-disk
clone (incl. live VSS), **verify-after-backup** (default on), format-v2 bulletproof
verification, the **zero-space WinFsp mount**, and **lock-state-aware BitLocker
capture** (see below). Validated by the full T1/T2 suites and the real-disk (T3)
matrix — see [TESTING.md](TESTING.md). Multiple real integrity bugs were found
and fixed along the way, including a silent data-loss bug (`div_ceil` phantom
cluster) that only real fragmented NTFS exposed.

### BitLocker: lock-state-aware capture (implemented)

**Lock state — not merely "is this BitLocker" — drives the capture mode:**

- **Unlocked** BitLocker volume → a **completely normal** capture. Detection
  compares the *physical* view of the boot sector (reads `-FVE-FS-` ciphertext
  through `\\.\PhysicalDriveN`) with the *volume-device* view (reads the
  decrypted filesystem once unlocked) — `classify_partition_on_disk` +
  `apply_bitlocker_state` in `phoenix-core/src/disk.rs`. Since capture reads
  through the volume device (or its VSS shadow), an unlocked volume streams
  plaintext with used-block sizing, and the `.phnx` is a normal, unencrypted,
  restorable image.
- **Locked** BitLocker volume → **raw ciphertext** capture, loudly warned in
  logs/CLI and flagged in the manifest; restore reproduces the locked volume,
  which still needs the original key/recovery password. VSS is skipped for
  locked volumes (nothing to snapshot); verify-after-backup works for both
  states (ciphertext at rest is deterministic).
- The per-partition manifest records `bitlocker: "unlocked" | "locked"`
  (absent for non-BitLocker; old manifests deserialize unchanged). Restore
  warns when writing ciphertext; GUI labels show "NTFS (BitLocker, unlocked)" /
  "BitLocker (locked)"; `list` and `list-backup` in the CLI show the state.

Design decisions (settled 2026-07): detection via dual boot-sector read (no
WMI/COM dependency — it can never disagree with the bytes the capture handle
actually reads); locked volumes are captured with a warning rather than
refused or auto-unlocked; unlocked-capture images restore as plaintext, no
re-encrypt-on-restore. Covered by T1 unit tests, the T2 `bitlocker.rs`
lifecycle test (VHDX + password protector), and the T3
`real_mbr_bitlocker_roundtrip` scenario — the latter **validated on real USB
hardware** (2026-07-08): the locked ciphertext round-trips byte-correct (the
restored partition's on-disk boot sector reads `-FVE-FS-`) and the unlocked
path captures/restores plaintext with used-block sizing.

Note on capturing a **locked** volume: fvevol rejects reads of a locked volume
through the *volume device* (`ReadFile` → `FVE_E_LOCKED_VOLUME` 0x80310000), so
both the backup and clone engines read a locked BitLocker partition through the
*physical disk* handle at the partition offset. This is wired into
`phoenix-capture`/`phoenix-clone` (a locked partition forces the PhysicalDrive
read path, skipping VSS and the volume lock, which don't apply to an unmounted
locked volume).

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
- **BitLocker locked volumes** are captured as raw ciphertext by design (with
  loud warnings); unlocking first yields a normal plaintext backup. There is no
  prompt-to-unlock or re-encrypt-on-restore flow. Mounting a ciphertext image
  surfaces an encrypted volume that prompts for the recovery key.
- **Restoring a locked BitLocker (ciphertext) image may need a reboot/reconnect
  to be recognized.** Restore writes the `-FVE-FS-` sectors correctly and now
  does a best-effort post-restore rescan (dismounts the restored volume so
  Windows re-reads it), and the CLI/summary flag the encrypted volume. On a
  **fixed** disk this usually makes Windows recognize the locked volume
  immediately; on **removable** USB media Windows can still serve a stale
  "decrypted / unknown" view from its per-session FVE cache until a **reboot or
  unplug/replug** (which we can't force from software). The on-disk bytes are
  correct the whole time — this is purely an OS cache-refresh quirk.
- **NTFS shrink leaves a one-time `chkdsk` reconciliation.** The shrink-
  relocation path deliberately doesn't perfectly rewrite `$Bitmap`; it leaves
  the trailing allocation slack past the new boundary for `chkdsk /F` to
  reconcile on first mount (documented in `phoenix-capture/src/ntfs.rs`). No
  data is at risk — `validate_extents_fit` guarantees nothing captured lives
  past the boundary, so it's pure metadata cleanup, and a single `chkdsk /F`
  leaves the volume clean (the T3 `real_mbr_restore_shrink` offline check
  asserts exactly this: reconcile-once, then a second pass must be clean).
  A perfect-`$Bitmap`-on-shrink rewrite (so no `chkdsk` is ever needed) is a
  possible future P3 improvement.
- **Restore to dissimilar hardware** may need Windows Startup Repair / `bcdedit`
  (expected; noted in the live checklist).

## Out of scope (explicit)

Incrementals / differentials (the v2 format keeps reserved fields for them),
cloud / sync / scheduling, and ReFS. (BitLocker lock-state-aware capture is
**done** — see "What's done" above.)
