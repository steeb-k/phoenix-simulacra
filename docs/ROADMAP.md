# Carbon Phoenix — Roadmap & Remaining Work

The engine is **feature-complete and validated on both virtual and real
hardware.** This document tracks what's left: known caveats, follow-up work, and
things noted during development that could use extra love. Nothing here blocks
using the tool; these are polish, packaging, and coverage-expansion items.

Status: **merged to `master`** (2026-07). The engine work (formerly
`feature/engine-completion`), the parallel chunk pipeline, and the
partial-restore volume-lock fix are all on `master`.

## What's done (for context)

### VSS + volume-lock consistency: verified on virtual and real media (2026-07)

Both consistency strategies are now **proven**, not just plumbed, across T2
(VHDX) and T3 (real USB flash + real external HDD) — with tests constructed so
`VssSession`'s silent live-volume fallback *fails* them rather than sneaking
past (see TESTING.md, `vss.rs` + `real_vss_backup_roundtrip`): point-in-time
shadow semantics; backup genuinely reading through the shadow (proven by
succeeding with a file handle held open); the no-VSS fallback enforcing
`FSCTL_LOCK_VOLUME` in both directions (refuses to start past open handles,
and blocks external opens/writes for the capture's duration); and byte-exact
round-trips on every path.

**Field finding — volsnap torn reads:** reading a shadow *device* while the
live volume takes writes can return a wrong read **once** for a block volsnap
has just copied-on-written; the very next read returns the frozen content.
Frequency scales with elapsed time × live write churn (1 occurrence on a fast
VHDX, 9 on a 4 TB USB HDD in a 15-second window). verify-after-backup now
re-reads any mismatched chunk: a re-read matching the recorded hash proves the
image faithful for that chunk (WARN + counted, unbounded — the count tracks
churn, not device health); stable divergence or unstable re-reads still fail
loudly. A capture-side tear is caught by the same mechanism as a hard
"source changed after capture" verify failure — never silent corruption.

Backup, restore (with NTFS grow + shrink-relocation, FAT/exFAT grow), disk-to-disk
clone (incl. live VSS), **verify-after-backup** (default on), format-v2 bulletproof
verification, the **zero-space WinFsp mount**, and **lock-state-aware BitLocker
capture** (see below). Used-block capture covers NTFS (volume bitmap), FAT12/16/32
(FAT scan), and **exFAT (allocation bitmap** — including `NoFatChain` contiguous
files a FAT scan would miss). Validated by the full T1/T2 suites and the real-disk
(T3) matrix — see [TESTING.md](TESTING.md). Multiple real integrity bugs were found
and fixed along the way, including a silent data-loss bug (`div_ceil` phantom
cluster) that only real fragmented NTFS exposed.

### Engine throughput: parallel chunk pipeline (2026-07)

Capture, restore, full image verify, and clone no longer run serial per-chunk
loops. `phoenix_core::pipeline` overlaps device I/O with parallel BLAKE3 +
zstd workers while keeping chunk commit order — and therefore the `.phnx`
file layout — byte-identical to the old serial code (equivalence-tested
against a serial reference). The clone engine double-buffers its raw copy so
source reads overlap target writes. Synthetic bench: capture-side CPU
throughput 786 → 2040 MB/s, full verify 976 → 3284 MB/s vs one worker. The
frozen-source verify (`verify_partition_against_source`) intentionally stays
serial — its torn-read re-read logic needs ordered device reads and it is
device-bound regardless. Details under the P3 throughput item below.

### Partial restore over a live target (2026-07)

A partial restore (writing one partition into an **existing** slot, rather than
a full-disk restore) now **locks and dismounts** the target slot's mounted
volume before writing its sectors — via the `phoenix_core::disk::LockedVolume`
guard (`FSCTL_LOCK_VOLUME` with backoff retry → `FSCTL_DISMOUNT_VOLUME`, holding
the handle open so nothing re-mounts mid-write; released right after the layout
is re-stamped so the volume re-mounts fresh — and *before* the NTFS grow pass,
which needs it mounted). Without this, raw writes to a still-mounted slot were
rejected with `ERROR_ACCESS_DENIED` (Win32 5) — the failure the T3B `boot_e`
partial-restore stage hit. Full-disk restore never needed it: it plants the
partition table last, so no volume is ever mounted while data lands. Covered by
the T1 restore unit suite; end-to-end hardware proof is the `boot_e` T3B stage
(not yet re-run on hardware since the fix landed).

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

### P2 — Post-restore BCD fixup for same-machine clones
Windows regenerates a clone's disk GUID and all partition unique GUIDs when
it comes online while the source disk is attached (duplicate GPT identities
are forbidden; observed live in T3B). The clone's BCD then still references
the ORIGINAL GUIDs — booting it either fails or, with both disks attached,
silently chain-loads the ORIGINAL Windows volume. A restore option should
rewrite the clone's BCD device elements to its actual (post-dedup) ESP +
Windows partition identities — effectively `bcdboot <clone>\Windows /s
<clone-ESP> /f UEFI`, or in-place BCD device-element editing. Without it,
"bootable clone" only holds when the clone is created with the source absent
or moved to a machine where the source never appears.

### ~~P2 — GUI resize UX revamp (deferred "Phase 7")~~ (LARGELY DONE 2026-07)
Delivered as the restore layout editor (modeled on Macrium/Backupper):
- Drag ghost, replace-on-drop with bright highlight (keep size → spill into
  trailing free space → shrink → refuse with a NotAllowed cursor), drop into
  unallocated space creates a new partition.
- Move/ResizeHorizontal cursors, glow on the click-target partition, 8 px
  edge handles, snap-to-fill within ~12 px, and pointer-crossing hysteresis
  so a drag can't accidentally reorder partitions (pure math + unit tests in
  `phoenix-restore::layout_edit`).
- Editable slot model with a toolbar: MBR/GPT style switch, blank layout,
  delete selected (or Delete key), reset. Plans are built straight from the
  slots (`reinit_style` on `RestorePlan`); the engine rewrites the table on
  partial MBR now too and locks volumes of deleted partitions
  (`partial_mbr.rs` T2 tests).

Remaining polish:
- Surface the **minimum shrink size** from the same computation
  `build_relocation_map` uses, so the UI can't propose an impossible shrink
  (the editor currently floors at used bytes, which can be optimistic when
  data sits beyond the shrink point).
- Live validation banner driven by `validate_layout`.
- Share the layout editor with the Clone page.
- Pixel↔byte mapping is linear while segments render with log-scaled minimum
  widths, so pointer tracking is approximate on very skewed layouts.

### ~~P2 — GPT on a real fixed disk~~ (DONE — validated on real hardware 2026-07)
`real_disk.rs` has `real_gpt_multifs_roundtrip` (NTFS + FAT32 + the diskpart
auto-MSR, full GPT round-trip), gated behind a fail-closed opt-in for
non-removable disks (`PHOENIX_T3_ALLOW_FIXED=1` + mandatory `PHOENIX_T3_SERIAL`
+ configurable `PHOENIX_T3_MIN_GB`/`MAX_GB`; boot/system always refused).
**Validated on a real 4 TB external HDD** — GPT primary+backup tables, MSR, and
both filesystems round-tripped, offline `chkdsk /F` clean on the first pass.
`PHOENIX_T3_LAYOUT_GB` caps grow-to-fill so a multi-TB disk runs in seconds
without shrinking GPT coverage (table still spans the whole disk). This closes
the last real-hardware coverage gap: the engine is now validated on both MBR
(real USB) and GPT (real fixed disk). See TESTING.md.

### P3 — CI coverage for the winfsp + real-disk tiers
- The `winfsp` feature isn't built in CI (needs LLVM/`libclang` + WinFsp on the
  runner), so the mount code isn't clippy-checked or tested there. Add a CI job
  that installs both and runs `--features winfsp` clippy + the `winfsp_mount`
  test (WinFsp via `choco install winfsp`).
- The T3 real-disk tier is inherently manual (it wipes a physical disk); keep it
  as a documented pre-release step.

### P3 — Automate the manual T3 items (where safe)
`scripts/live-smoke-checklist.md` covers live VSS system-disk backup/clone,
boot-the-clone, and 4Kn media manually. The live-system and boot checks are
hard to automate safely; **4Kn media** could be automated if a 4Kn test device
(or a 4Kn-emulating VHD) is available.

### P3 — Engine throughput: pipeline/parallelize backup, restore, AND verify
**Core work DONE (2026-07)** — see "What's done"; the deliberately deferred
avenues remain below. The old shape: every data path ran one serial loop per
chunk (read → compress/decompress → hash → write, each stage waiting on the
previous); real-world baseline from the first production-scale run (2026-07,
~709 GB used, NVMe source → SATA/USB targets) was capture+verify ≈ 3 h
(~130 MB/s effective), restore projected ~2 h.

Landed in `phoenix_core::pipeline` (+ the clone loop):
- **Capture**: `PartitionStreamWriter::write_chunk` feeds a worker pool
  (BLAKE3 + zstd in parallel) while a dedicated writer thread commits chunks
  in submission order — the `.phnx` layout stays **byte-identical** to the
  serial loop (proven by a serial-reference equivalence test). Source reads
  overlap the CPU work via bounded-channel backpressure (≤ ~128 MiB in
  flight at the default `min(cores, 8)` workers; `PHOENIX_WORKERS`
  overrides).
- **Restore + full image verify**: `process_chunks_parallel` — a reader
  thread streams compressed chunks, workers decompress + hash-check, the
  caller consumes plaintext in order on its own thread (raw-disk writes stay
  on the calling thread; target offsets stay monotonic for spinning media).
- **Clone**: `stream_extents` is double-buffered — a scoped reader thread
  reads ahead through a bounded channel while the caller writes, so source
  read and target write overlap instead of alternating.
- Synthetic bench (release, 512 MiB half-compressible, this dev machine):
  capture-side 786 → 2040 MB/s and full verify 976 → 3284 MB/s vs one
  worker — the CPU ceiling now clears any single source device.
- **Preserved by design**: `verify_partition_against_source` (frozen-source
  re-read) stays serial — its torn-read re-read-confirm logic needs ordered
  device reads, and it is device-bound anyway (BLAKE3 is a rounding error
  next to the source read). The frozen-vs-unfrozen verify strategy in
  run_backup phase 3 is untouched.

Remaining avenues (deferred until real-hardware profiling says otherwise):
- **Adaptive compression**: skipping zstd for incompressible chunks needs a
  stored-raw chunk flag — a format change — and the parallel workers already
  hide zstd below device speed. Revisit only if capture on real hardware
  still shows CPU as the limiter.
- **Device-aware parallel I/O**: multiple readers over disjoint extents on
  NVMe; strictly sequential on spinning/USB media (gate on bus/media type).
  Only pays once a single reader can't saturate the source.
- **Pipeline verify with capture**: verify partition N while capturing N+1 —
  helps only multi-partition backups and contends for the same source
  device; the common one-big-data-partition case gains nothing.
- Optional **sampled verify-after** tier for speed-sensitive runs (full
  re-read stays the default; mirrors `verify --quick` semantics).

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

**Resumable (partial) backups** — considered and rejected (2026-07). The
container format could support it (validate partial chunks, truncate to the
last complete one), but the VSS snapshot dies with the process, so any resume
of a *live* source mixes two points in time — including sector-by-sector
mode, which changes what we read, not whether the source mutates. The only
clean cases are frozen sources (locked BitLocker, offline disks), too niche
to justify the checkpoint machinery. Interrupted backups restart.
