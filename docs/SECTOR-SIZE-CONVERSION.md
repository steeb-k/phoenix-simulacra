# Sector-size conversion on restore/clone — build plan

**Status: proven on hardware, not yet built.** This document is the plan to turn
the two hardware spikes (2026-07-14) into a shipping, opt-in feature. Read it on
the dev box before implementing.

## What it is

Restore or clone a backup taken from a **4Kn** (4096-byte logical sector) disk
onto a **512e** (512-byte logical sector) disk, and end up with a disk Windows can
actually mount and boot. Today that restore comes up **RAW**, because used-block
capture copies each filesystem's boot sector verbatim — so a 4Kn volume's
`BytesPerSector = 4096` lands on a 512e device, and Windows rejects the
contradiction. (This is why most imaging tools refuse cross-sector clones.)

The fix is a **metadata-only boot-sector rewrite** — no file data moves — applied
per partition after its bytes land. Both filesystem families on a Windows disk are
proven convertible:

- **NTFS** (data, Recovery): mounts, reads files, `chkdsk /scan` clean, cluster
  count identical to source.
- **FAT32** (the ESP): mounts, reads the whole `\EFI\Microsoft\Boot\` tree
  including `bootmgfw.efi`, `chkdsk /scan` clean.

MSR is empty (no filesystem), so a full Windows GPT disk — ESP + MSR + NTFS +
Recovery — is **fully** convertible, and the result can be **bootable**.

### Why it's safe (the load-bearing fact)

Both NTFS and FAT are **cluster-addressed**. Every file's data, the `$MFT`/
`$Bitmap` (NTFS), the FATs and directories (FAT32) are located by cluster, and the
byte offset of a cluster is `cluster_number × cluster_size_bytes`. If we **preserve
the cluster size in bytes**, nothing moves. For a 4096→512 flip that means every
sector-denominated field scales by `ratio = 4096/512 = 8`, and every byte offset
is invariant. NTFS's MFT-record fixup stride is always 512 regardless of the
volume's sector size (`phoenix-restore/src/grow.rs`), so MFT records need zero
change.

## Scope (v1) and non-goals

**In scope:**
- Direction: **4Kn → 512e only.**
- Filesystems: **NTFS and FAT32.** (Together these make a bootable Windows disk.)
- Mode: **both full-disk and partial** restore, and clone. Conversion is
  per-partition, so it is *not* full-disk-only — that earlier assumption was wrong.

**Explicitly out (v1):**
- **512e → 4Kn** (and any other mismatch): hard refusal. Fine→coarse can't be
  faked — sub-4096-byte clusters can't exist on 4Kn, and 512-but-not-4096-aligned
  structures break. No override.
- **exFAT**: deferred. It has a boot-region checksum and more sector-denominated
  fields; convert it later. An image containing an exFAT *data* partition is still
  restorable — that partition is just left unconverted (and unreadable on the
  target), with a specific warning (see Alert E).
- **Actual UEFI boot verification.** We prove the ESP mounts/reads/chkdsks; we do
  not (cannot, here) verify firmware boot. That's a dev-box/real-deploy check.

## The conversion recipe (proven)

Per partition, after its data has been written to the target, read the just-landed
boot sector at the partition's target offset and, if it is 4Kn (`BytesPerSector ==
4096`) and a supported FS, patch these fields (all offsets are within the first
512 bytes of the VBR), then write the patched VBR to **both** the primary location
and the backup boot sector. `ratio = old_BytesPerSector / 512`.

**NTFS** (offsets): `0x0B BytesPerSector → 512`, `0x0D SectorsPerCluster ×ratio`,
`0x1C HiddenSectors → target_partition_offset / 512`, `0x28 TotalSectors ×ratio`.
Backup boot at partition-relative `old_TotalSectors × old_BytesPerSector`
(== `new_TotalSectors × 512`, i.e. same offset — overwrite in place).

**FAT32** (offsets): `0x0B → 512`, `0x0D SectorsPerCluster ×ratio`,
`0x0E ReservedSectors ×ratio`, `0x1C HiddenSectors → target_offset / 512`,
`0x20 TotSec32 ×ratio`, `0x24 FATSz32 ×ratio`, `0x30 FSInfoSector ×ratio`,
`0x32 BkBootSector ×ratio`. Backup boot at partition-relative
`old_BkBootSector × old_BytesPerSector`. FSInfo body needs no change.

`HiddenSectors` is set from the **target** partition offset (not just old×ratio),
because a full-disk restore may place the partition at a different offset than the
source. `TotalSectors`/`TotSec32` are intrinsic to the volume and stay ×ratio.

## Pre-flight decision matrix

Computed from `manifest.disk.sector_size` (source, e.g. 4096) vs the target disk's
`DiskInfo.sector_size` (e.g. 512), per `manifest.partitions[].fs`:

| Source | Target | Convertible FS present | Outcome |
|---|---|---|---|
| 512 | 512 | — | Normal restore/clone, no conversion. |
| 4096 | 4096 | — | Normal restore/clone, no conversion. |
| 4096 | 512 | NTFS / FAT32 | **Conversion available** — opt-in (Alert A / hazard dialog). |
| 512 | 4096 | any | **Hard refuse** (Alert B). |
| any | any (other mismatch) | any | **Hard refuse** (Alert B). |

Within an available conversion, classify each partition: NTFS/FAT32 → convert;
MSR/empty → nothing needed; exFAT/other data FS → left unconverted, warn (Alert E).
Bootability: bootable only if the ESP (FAT32) is among the converted set.

The pre-flight lives in the restore planner —
`RestorePlan::validate_against_backup` (`phoenix-restore/src/plan.rs`, TODO already
marked) gains the source vs target comparison; the seam that already reads the
target sector size is `build_full_disk_plan` at `plan.rs:451-462`. Clone computes
target geometry in `phoenix-clone` (`lib.rs:556` already takes `target.sector_size`).

## Alerts — the complete set, with where each is shown

| # | Trigger | Severity | CLI | GUI |
|---|---|---|---|---|
| **A** | 4Kn→512e, convertible FS, user has **not** opted in | Gate | `restore`/`clone` **error out** unless `--convert-sector-size` is passed; message explains the flag and what it does | The **hazard dialog** (below) opens instead of spawning the job |
| **B** | 512e→4Kn or other unsupported mismatch | Hard refuse | Error exit; explain fine→coarse can't be represented | Error dialog (danger, no hazard tape), no Start |
| **C** | *(removed)* partial-mode restriction | — | — | — (conversion is per-partition; partial is allowed) |
| **D** | Converted set does **not** include the ESP → disk won't boot | Warn | Printed in the pre-flight banner and the post-op summary | A line in the hazard dialog details + the results modal |
| **E** | Image has a data partition we won't convert (exFAT) | Warn | Printed banner naming the partition(s) | A line in the hazard dialog details |
| **F** | Standard destructive wipe/overwrite (already exists) | Warn | Existing clone banner (`main.rs:274-283`); restore has none today — add one | Existing `pending_restore`/`pending_clone` copy, carried into the hazard dialog |
| **G** | Post-op summary: which partitions converted + bootability | Info | `println!` summary after `run_restore`/`run_clone` | Results/status modal |

Note C is intentionally struck: the earlier "full-disk only" constraint does not
hold now that per-partition conversion (incl. the ESP) is proven.

## The hazard dialog (GUI)

Reuse the existing hazard-tape confirm component — `confirm_dialog.rs`
(`ConfirmView` with `hazard_tape: true`, `confirm_danger: true`). Every destructive
action already funnels through it; the restore/clone confirms are
`show_restore_confirm_dialog` (`main.rs:1411`) and `show_clone_confirm_dialog`
(`main.rs:1378`), each parking a `Pending*` struct and spawning the worker only on
Confirm. This dialog **replaces** the plain restore/clone confirm when a conversion
is pending (so the user isn't double-prompted).

Content:
- **Title:** "Convert 4Kn → 512e and restore" (or "…and clone").
- **Message:** this backup is from a 4096-byte-sector disk; the target uses
  512-byte sectors; to make it usable the filesystem boot sectors will be
  rewritten. The result is a **converted copy, not a byte-identical restore.**
- **Details (`details: &[String]`):**
  - The standard destructive line (target disk / partitions will be erased).
  - Partitions that **will be converted** (NTFS/FAT32), listed.
  - **Bootability** (Alert D): "The EFI System Partition will be converted — the
    restored disk should be bootable." OR "The ESP is not being converted — the
    restored disk will hold its data but **will not boot**."
  - Any **unconvertible** data partitions (Alert E), named.
- **Acknowledgement checkbox (NET-NEW to `confirm_dialog.rs`):** "I understand this
  rewrites filesystem metadata, and the result is a converted copy — not an
  identical one." The Confirm button stays **disabled until it is ticked.**
- **Buttons:** Cancel / "Convert and restore" (danger fill).

The checkbox is the one piece the dialog component lacks today: add an optional
`ack: Option<&mut bool>` (label + state) to `ConfirmView`, render it via
`ui.checkbox` in the body closure (`confirm_dialog.rs:107-165`; idiom at
`main.rs:2224`), and gate the confirm `clicked()` on it. A new `Pending*`-style
field on `PhoenixApp` holds the ack bool and the conversion plan; add it to
`modal_open()` (`main.rs:555`) so the page goes inert while it's up.

## CLI

- **New flag `--convert-sector-size`** on both `restore` and `clone`
  (`main.rs:57-64`, `65-88`). It is the explicit opt-in — equivalent to the GUI
  checkbox — so passing it *is* the acknowledgement.
- Without it, a detected 4Kn→512e mismatch is a **hard error** (Alert A) naming the
  flag. This also gives `restore` its first safety gate — today it has none
  (clone has the "type the target disk number" prompt at `main.rs:285-294`; restore
  runs immediately at `main.rs:163-188`).
- Print the full warning banner (model on the clone banner) before proceeding, and
  a post-op summary (Alert G).
- **`list-disks`**: add a sector-size column (it doesn't show one today); `inspect`
  already prints `manifest.disk.sector_size` (`main.rs:421-424`).

## Internal correctness — do not miss these

- **verify-after must account for the rewrite.** Restore/clone verify re-reads raw
  sectors and compares to the image. We *modified* the boot sectors (primary +
  backup) post-write, so a raw compare of those sectors would **false-fail**. For a
  converted partition: exclude the boot-sector regions from the raw compare, and
  add a mount + `chkdsk /scan` check as the real "did the conversion take"
  verification. Do not let a converted restore report a spurious mismatch.
- **Where conversion attaches:** in the restore write loop after each partition's
  data lands (around `restore.rs:383-386`, where `PartitionWriter::open_disk` runs
  with `disk.sector_size`), and the clone equivalent. It needs the partition's
  target offset (from the plan) and its FS kind (from the manifest).
- **The target disk is written offline / the volume must not be mounted** during
  the boot-sector write, same as any raw restore write.
- **Guard field ranges:** `SectorsPerCluster` must stay ≤ 255 (huge-cluster
  encoding not handled in v1); FAT32 `ReservedSectors`/`FSInfo`/`BkBoot` must stay
  within their u16 fields. Refuse cleanly if a scale would overflow.

## Test plan (T2, no hardware needed)

The existing harness synthesizes both 4Kn and 512e VHDX (`sector_4kn.rs`,
`vhdx_container.rs`). New T2 tests:

1. **`convert_4kn_ntfs_to_512e`** — make a 4Kn NTFS VHDX with files, back it up,
   full-restore to a 512e VHDX with `--convert-sector-size`, then mount + `chkdsk`
   and digest-compare the files. (Mirrors the proven NTFS spike.)
2. **`convert_4kn_fat32_esp_to_512e`** — same for a FAT32 volume; mount, read a
   known file tree, `chkdsk`. (Mirrors the proven ESP spike.)
3. **`convert_refuses_512e_to_4kn`** — assert the hard refusal (Alert B).
4. **`convert_requires_optin`** — assert a 4Kn→512e restore without the flag errors
   (Alert A), and with it succeeds.
5. **`convert_partial_restore`** — convert a single partition in partial mode
   (proves it's not full-disk-only).
6. **verify-after on a converted restore does not false-fail** (the correctness
   note above).

## Implementation checklist (suggested order)

1. Pre-flight comparison + `PhoenixError` variants (refuse / opt-in-required) in
   `phoenix-restore` + `phoenix-clone`; the guard alone is shippable and stops the
   silent-RAW footgun.
2. The conversion function (NTFS + FAT32) in `phoenix-restore` (shared helper),
   applied in the restore/clone write path behind the opt-in.
3. verify-after handling for converted partitions.
4. CLI: `--convert-sector-size`, banners, summary, `list-disks` sector column.
5. GUI: `ConfirmView` ack-checkbox, the conversion hazard dialog, wiring
   (`Pending*` field, `modal_open`, options thread-through), results summary.
6. The T2 tests above.

## Open decisions

- **exFAT**: leave deferred (recommended), or bring into v1? It only matters for
  exFAT *data* partitions in the image; the Windows system set doesn't need it.
- **Same-size, different-sector edge** (e.g. a 512e image onto another 512e disk):
  no conversion — already handled by the matrix, listed for completeness.
