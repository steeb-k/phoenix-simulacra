# Carbon Phoenix — Roadmap & Remaining Work

The engine is **feature-complete and validated on 512-byte-sector x64 hardware**,
virtual and real. This document tracks what's left.

Status: all work is on `master` (2026-07). There is **no CI** — `.github/` was
deleted 2026-07-13; validation is local and manual (see [TESTING.md](TESTING.md)).

## ⚠️ Read this first — the "feature-complete" claim has two holes

A code audit on 2026-07-13, run while preparing for ARM64 testing, found two
**correctness** defects that outrank every polish/packaging item below. Neither is
hypothetical; both are traced to specific lines. Neither is caught by any existing
test, because every test formats NTFS at the 4K default and every test disk has
512-byte sectors.

1. **NTFS cluster size is hardcoded to 4096 at capture-plan time** (P0 below).
   Platform-independent. Affects any NTFS volume not formatted at 4K clusters.
2. **4Kn media is unsupported and capture fails on the first read** (P1 below).
   Blocks the ARM64 laptop's UFS drive *if* it reports 4096-byte logical sectors.

Until P0 is fixed, "validated" means "validated on 4K-cluster NTFS."

---

## Correctness backlog — do these first

### P0 — NTFS `bytes_per_cluster` is hardcoded to 4096 (data-integrity)

`ntfs_plan` parses the boot sector and computes the volume's real cluster size
(`phoenix-capture/src/ntfs.rs:349`) — and then **does not return it**. Its
signature is `-> Result<(Vec<Extent>, Option<String>)>`, so `plan_capture`
substitutes a literal:

```rust
// phoenix-capture/src/backup.rs:817-819
(FilesystemKind::Ntfs, CaptureMode::UsedBlocks) => {
    let (extents, bitmap_hash) = ntfs_plan(reader)?;
    Ok((extents, 4096, mode, fs, bitmap_hash))   // <-- the real value was discarded
}
```

FAT and exFAT do this correctly (`backup.rs:825`, `backup.rs:837` return the
parsed value); NTFS alone is wrong.

That literal becomes `bytes_per_cluster` in the manifest and is fed to
`build_relocation_map` on the shrink path (`phoenix-restore/src/restore.rs:411`,
`phoenix-clone/src/lib.rs:399`). **Shrinking an NTFS volume formatted with
non-4K clusters (64K is common on large data volumes) therefore builds its
relocation map with a cluster size up to 16× too small.** Extent capture itself is
unaffected (extents come straight from `ntfs_plan`), so this is a
restore/clone-shrink defect, not a capture one.

Fix: return `cluster_size` from `ntfs_plan` and thread it through. Then **add a T2
test that formats NTFS with 64K clusters and shrinks it** — the absence of such a
test is the reason this survived.

### P1 — 4Kn media support (blocks ARM64 UFS testing)

The `.phnx` format was designed for 4Kn (extents use a fixed 512-byte *format*
addressing unit deliberately decoupled from device geometry; the real logical
sector size is recorded in `disk.sector_size`), and the device sector size **is**
discovered correctly (`get_sector_size`, `phoenix-core/src/disk.rs:970`) and
threaded into the raw **write** path (`PartitionWriter` does a read-modify-write
bounce for sub-sector I/O). The read path and the geometry math never got the same
treatment.

**Capture fails on the first read of a 4Kn volume.** Raw disk/volume handles reject
any read whose length or offset is not a multiple of the *logical* sector size:

- **H1 (critical)** — `PartitionReader::read_at`
  (`phoenix-capture/src/reader.rs:143-184`) does a bare `ReadFile` with no
  alignment handling, and every FS parser hands it a hardcoded 512-byte
  boot-sector buffer: `ntfs.rs:158`, `ntfs.rs:346` (the main capture path),
  `fat.rs:78`, `fat.rs:506`. `fat.rs:89` rounds a FAT read up to 512 rather than
  to the device's sector size. All return Win32 87 on 4Kn → backup **and** clone
  fail before writing a byte. *Fix: give `PartitionReader` a `sector_size` and
  mirror the RMW bounce that `raw.rs:259-272` already implements.*
- **H2 (high)** — `extend_ntfs_volume` is passed
  `PartitionIndexEntry::sector_size`, which is **always the 512-byte format
  constant**, not the volume's sector size (`phoenix-restore/src/restore.rs:597`;
  the doc comment on `grow.rs:78` is simply wrong about what that field holds).
  `FSCTL_EXTEND_VOLUME` wants the *volume's* sectors, so on 4Kn the count is 8×
  too large. Note `phoenix-clone/src/lib.rs:330` passes the real
  `target.sector_size` and is correct — restore and clone disagree.
- **H3 (high)** — `pack_full_disk` reserves the backup GPT with
  `33 * 512` (`phoenix-restore/src/plan.rs:328-330`). On 4Kn the trailing GPT is
  33 × 4096, so the anchored last partition **overlaps the backup GPT entry
  array**. The GUI already gets this right
  (`phoenix-gui/src/restore_layout.rs:320` uses the real sector size), so planner
  and GUI disagree on 4Kn.
- **H4 (medium)** — `set_drive_layout` hardcodes `let sector_size: u64 = 512;`
  for `StartingUsableOffset` / `UsableLength`
  (`phoenix-restore/src/partition_table.rs:542-548`). On 4Kn that yields 17,408 —
  not even a multiple of 4096. Windows writes the actual GPT structures from byte
  offsets, so the likely failure is IOCTL rejection rather than corruption, but
  it is wrong.
- **H5 (medium)** — MBR `HiddenSectors: (entry.offset_bytes / 512)`
  (`partition_table.rs:488`) is in *device* LBAs; 8× too large on 4Kn. Low
  exposure (Windows wants GPT for 4Kn boot) but a genuine device assumption.
- **H6 (medium)** — the mount synthesizer is 512-only and self-consistent
  (`phoenix-mount/src/{gpt,synthetic,vhd}.rs`, `winfsp_mount.rs:266`
  `.sector_size(512)`); nothing reads `manifest.disk.sector_size`. A volume
  captured from 4Kn has `BytesPerSector = 4096` in its BPB, and NTFS **refuses to
  mount** on a device advertising 512. The fixed-VHD format is inherently
  512-sector, so real 4Kn mount needs the VHDX path `synthetic.rs:44-49` already
  flags as unimplemented. Minimum viable fix: detect and **error clearly** instead
  of producing an unmountable disk.

**All of this reproduces on x64 with a 4Kn VHDX** (`New-VHD -LogicalSectorSize
4096`) — see TESTING.md → "Tier 2-4Kn". Do it here; it does not need the ARM
machine.

### P1 — Partial clone (`UpdateExisting`) has no end-to-end test

`CloneTableMode::UpdateExisting` writes into an **existing** target partition and
rewrites that target's partition table in place. It is the **default** mode on the
GUI's Clone page (commit `3a4395c`), and it is reachable by drag. Its entire test
coverage is 5 planning-math unit tests in `phoenix-clone/src/plan.rs`; the write
path (`phoenix-clone/src/lib.rs:198`, `:259`) is exercised by **no T2 and no T3
test**.

This is the same class of bug as the partial-restore volume-lock failure that T3B
`boot_e` caught (a raw write to a still-mounted slot → `ERROR_ACCESS_DENIED`) —
and the clone path has never been run against a live mounted target. Owed: a T2
`partial_clone.rs` mirroring `partial_mbr.rs`, then a real-media run.

The packaging, polish, and performance backlog continues in **Backlog** below, after
the context section. Everything in it ranks *under* the three items above.

### Recommended order of work (2026-07-13)

Platform-independent, and deliberately front-loaded with the things that can be
done **on the x64 dev box** — none of the top four need the ARM64 laptop.

1. **Fix P0 (NTFS cluster size).** Small, contained, and a data-integrity defect.
   Return `cluster_size` from `ntfs_plan`; add a T2 test that formats NTFS with
   64K clusters and shrinks it.
2. **Build the 4Kn VHDX tier** (TESTING.md → "Tier 2-4Kn"). One `New-VHD` command
   turns 4Kn from an untestable hardware question into a normal local test. This
   is the highest-leverage single action available — it converts the entire ARM
   sector-size risk into work you can do today.
3. **Fix P1 (4Kn), H1 first.** H1 alone unblocks capture; H2–H5 unblock restore;
   H6 (mount) can trail. Verify each against the VHDX from step 2.
4. **Fix P1 (partial-clone coverage).** Write `partial_clone.rs` as a T2 test.
   A destructive engine mode that is the GUI's default should not be shipping on
   planning-math unit tests alone.
5. **Then go to ARM64** ([WINDOWS-ARM64.md](WINDOWS-ARM64.md)) — with 4Kn already
   fixed, ARM testing tests *ARM*, and a failure there isolates cleanly to the
   architecture instead of being confounded with sector size.
6. Resume the packaging backlog (WinFsp installer bundling, BCD fixup).

The one thing that genuinely **requires** the ARM hardware is step 5. Everything
above it is being blocked on hardware only by habit.

---

## What's done (for context)

### Automatic lock-then-VSS consistency: verified on virtual and real media (2026-07)

There is **no user switch** for how a source is frozen (the "Use VSS" checkbox
and `backup --vss` flag are gone as of 2026-07-12). Per partition the engine
takes an exclusive `FSCTL_LOCK_VOLUME` when it can, escalates to a VSS shadow
when the volume is too busy to lock (the running Windows volume always is), and
**aborts** when a lettered volume can be frozen neither way rather than smear a
live read into an image the user would trust. Un-lettered volumes (ESP/Recovery)
that Windows refuses to lock *or* snapshot are the one unfrozen case, and
verify-after checks the image instead of the source for them.

Both arms are **proven**, not just plumbed, across T2 (VHDX) and T3 (real USB
flash + real external HDD) — with tests constructed so `VssSession`'s silent
live-volume fallback *fails* them rather than sneaking past (see TESTING.md,
`vss.rs` + `real_vss_backup_roundtrip`): point-in-time shadow semantics; a
backup genuinely escalating to and reading through the shadow (proven by
succeeding with a file handle held open, which no lock could survive);
`FSCTL_LOCK_VOLUME` enforced in both directions (an unfreezable volume refuses
to start, and a locked one blocks external opens/writes for the capture's
duration); and byte-exact round-trips on every path.

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

### Mount: winfsp-by-default + per-partition selection (2026-07)

The `winfsp` feature is now a **default feature of the GUI and CLI**, so a
plain `cargo build`/`cargo run` gets the zero-space on-demand mount — the
stopgap full-size temp-VHD materialization had been silently reachable in dev
builds (and failed outright on low disk space, violating the never-double-the-
footprint rule). The fallback now requires an explicit `--no-default-features`.
`run-system-tests.ps1` builds T2 with the feature too.

The GUI Mount page also gained the shared partition map (same widget as
Backup): the chosen `.phnx`'s layout renders with checkboxes and **only the
selected partitions get drive letters**. Mechanism: attach with
`ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER`, correlate volumes to backup
partitions by disk-extent start offset (`phoenix_mount::letters`), assign
letters explicitly, remove them on unmount. CLI: `mount --partitions 2,3`.
Covered by the `winfsp_mount_selected_partition_only` T2 test.

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

### GUI: the app shell is built out (2026-07-11 → 07-13)

Roughly 40 commits of GUI work landed after the engine was declared complete and
were never recorded here. Summarized so this document stops implying the GUI is a
thin shell over the CLI:

- **Chromeless window** with a native-behaving custom titlebar, an L-shaped
  sidebar+status-bar surface, Alt-accelerator navigation, and a theme toggle in
  the sidebar. The Options page was replaced by About.
- **Sticky action bar** — a full-width gradient Start button pinned above the
  status bar. Every main page (Mount included) plugs into it via
  `current_page_action`; hazard-tape fill when disabled. No page has an inline
  start button.
- **Clone page** — disk dropdowns, flow arrow, and drag-to-partial-clone;
  **partial clone is the default mode**. (Its engine path is the P1 test gap
  above.)
- **Restore page** — rebuilt in the Clone page's image, with the layout editor
  (drag/resize/delete/blank/style-switch), a confirm-the-target dialog, and a
  backup preview before verifying.
- **Mount page** — always-visible mounts table, per-partition drive-letter
  selection, unmount-on-close guard, refusal of double-mounts.
- **History** as a full-width table with real details; a universal Refresh pill
  (F5); barber-pole progress bar; green-checkmark completion screens; every
  finished job gets an explicit verdict, and warnings are surfaced rather than
  buried in the log.
- **Disk enumeration** now reports drive type and names the drive behind a USB
  bridge.
- The workspace is **rustfmt-clean and clippy-clean** (`-D warnings`) as of
  `576d6ab` / `d5b22c6`.

---

## Backlog — packaging, polish, performance

Everything here ranks below the correctness items at the top of this document.

### P2 — WinFsp installer bundling
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

### ~~P3 — CI coverage for the winfsp tier~~ (MOOT — CI deleted 2026-07-13)
There is no CI. The `winfsp` feature is a GUI/CLI default, so a plain local
`cargo build` covers the mount code, and `run-system-tests.ps1` runs the
`winfsp_mount` tests. The T3 real-disk tier is inherently manual (it wipes a
physical disk); keep it as a documented pre-release step.

### ~~P3 — Automate the manual T3 4Kn item~~ (SUPERSEDED — now P1, above)
The live-system and boot-the-clone checks in `scripts/live-smoke-checklist.md`
remain hard to automate safely. **4Kn media is no longer a "nice to automate"
item** — a 4Kn VHDX is trivially creatable with `New-VHD -LogicalSectorSize 4096`
(see TESTING.md → "Tier 2-4Kn"), and doing so immediately exposes the P1 4Kn
defects above. Automate it as part of that fix, not after it.

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

> Note the distinction: the items below are **deliberate trade-offs**. The 4Kn and
> NTFS-cluster-size items at the top of this document are **bugs**, not caveats.

- **4Kn (4096-byte logical sector) media is not supported.** Capture fails on the
  first read. 512e media (4096 physical / 512 logical) is fully supported and is
  the common case. See "P1 — 4Kn media support".
- **Mount without the `winfsp` feature** (an explicit `--no-default-features`
  build — the feature is on by default for GUI/CLI) falls back to materializing
  a full-size temp VHD — a **dev-only stopgap**. It's never used in a `winfsp`
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
