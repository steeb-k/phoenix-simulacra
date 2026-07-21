# Changelog

Notable changes per release. Releases are published on the [releases page](https://github.com/steeb-k/phoenix-simulacra-binaries/releases).

## 0.7.1 — 2026-07-21

- **Cancelling a backup's verification no longer deletes the backup.** The final verify pass runs *after* the image is already written and finalized, so cancelling it (or having it fail) now leaves the finished `.phnx` in place instead of discarding it. Only a backup cancelled mid-capture — which never finished and can't be opened — is still cleaned up.
- **The Restore + Verify list gained a third shield.** Alongside the green shield (verified on this machine) and the yellow click-to-verify outline, a **red** shield now marks a backup whose verification actually *failed*, so a bad image is visible at a glance. A cancelled verify stays yellow — the image is kept and its integrity is simply unknown, so click the shield to check it.

## 0.7.0 — 2026-07-21

- **New Utilities section.** The Boot Repair page has become *Utilities* — a home for one-off maintenance tools you run on a drive you've just cloned, restored, or moved. Each tool is a card that opens a guided modal and only touches the installation you pick.
- **Windows Boot Repair** now lives there, unchanged: point a drive's boot configuration at a Windows installation on it and rebuild its boot files with Windows' own tools.
- **Reset Windows Hello** (new). Cloning or restoring a system disk changes the machine identity Windows ties Windows Hello to, so PINs and fingerprint or face sign-in on the copy stop working — and often can't even be removed from Settings. This clears the credential store on the installation you pick so Windows Hello can be set up fresh at the next sign-in. Works on an offline clone or the running system; passwords are unaffected.
- **Enable/Disable Built-in Administrator** (new). Turn the hidden built-in Administrator account on or off on a chosen installation — a way back into a locked-out clone. The modal shows whether the account is currently enabled or disabled and the button switches it; enabling also blanks its password (on the running system). Works on an offline installation (by editing its account database directly) or on the live system.
- Virtualize's keyboard shortcut moved to **Alt+V**, freed up now that Verify folded into the Restore + Verify page.

## 0.6.1 — 2026-07-20

- **Shrinking is reliable.** Restoring or cloning a fragmented NTFS volume into a smaller partition could fail with "relocated run list grew to N bytes, past the N-1 byte attribute budget" — and it failed *after* the data had been copied, leaving the target holding data its metadata no longer described. Three changes fix it: relocated run lists now grow into the MFT record's free space instead of being limited to the handful of spare bytes inside their own attribute; relocated clusters are packed to keep each piece contiguous rather than splitting it across free runs; and every shrink is now checked against the source's MFT *before* anything is written, re-planning the relocation where a record would not fit.
- **A shrink that genuinely cannot work is refused up front**, with the smallest target size that would work, instead of aborting partway through with the target already overwritten. On the clone path this check moved into the prepare phase, so it happens before the target's partition table is touched.
- Large shrinks are substantially faster: translating relocated clusters was a linear scan per cluster, which made the metadata rewrite quadratic in the size of the move.
- **Boot repair works on UEFI again.** It passed `bcdboot` an option that made it fail outright, so the EFI path never completed.
- **Boot repair rebuilds the boot configuration instead of merging into it.** `bcdboot` adds to an existing BCD rather than replacing it, so repairing a disk with a stale store kept the very entries that made it unbootable. The old store is now set aside (kept as `.bak`) before the rebuild.

## 0.6.0 — 2026-07-20

- **Boot a backup as a virtual machine** (experimental): a new Virtualize page runs a `.phnx` as a QEMU VM without modifying the backup or materializing it to disk. The guest's disk is served on demand through WinFsp and every write lands in a copy-on-write overlay, so sessions are resumable — stop the VM, come back, continue. Validated by booting real Windows 11 (GPT/UEFI) and Windows 10 (BIOS/MBR) captures to a desktop.
- **MBR/BIOS backups boot too.** Captures now preserve the NT disk signature, partition type bytes, the active flag, and the MBR's own boot code — none of which can be reconstructed afterwards, and all of which a BIOS guest needs. A GPT capture already preserved its disk and partition GUIDs for the same reason.
- **QEMU is bundled.** The installer downloads a curated QEMU (223 MB, pruned from upstream's 1.17 GB) during setup and installs it privately, never touching a QEMU you installed yourself. Skipping or failing the download is not fatal — the Virtualize page offers the same download later.
- **Host↔guest clipboard**, plus a shared folder and a `VMSCRIPTS` drive carrying a guest-tools installer, so moving files and text in and out of a booted guest doesn't mean typing commands by hand.
- **Guest networking is opt-in** and granted while the VM runs, rather than at boot. A connected Windows guest is offered a display driver by Windows Update within moments of first login, and that driver blanks the screen on the next reboot.
- **Acceleration problems are explained, not silent.** When WHPX is unavailable the app says which of three causes applies — virtualization disabled in firmware, the Windows Hypervisor Platform feature not enabled, or the hypervisor not running — and can apply the two Windows-side fixes itself. Guests still run under software emulation meanwhile, slowly.
- Backups reached via Browse… are remembered in every page's dropdown, and recorded whenever one is actually used.
- The Virtualize page is hidden in portable builds and on ARM64, where it could not work.

## 0.5.0 — 2026-07-18

- **ReFS support**: ReFS volumes are detected (mounted-volume query plus boot-sector `ReFS`/`FSRS` signature), captured used-blocks via `FSCTL_GET_VOLUME_BITMAP`, restored, cloned, and mountable like any other volume. BitLocker-on-ReFS is recognized in both locked and unlocked states. Resize policy: shrink is refused (ReFS has no offline shrink); grow extends the restored volume via `FSCTL_EXTEND_VOLUME`. Validated end-to-end on real hardware (352 GB ReFS partition → 360 MB used-block image, byte-verified restore, grow, and mount).
- **Grow-restore fixes** (affected NTFS too, on USB/slow targets): the restored disk is now brought online *before* the volume-extend pass — a re-signatured disk that Windows' SAN/duplicate-signature policy left offline has no volume devices, so the extend silently timed out and the volume stayed at source size. The extend pass also finds un-lettered `\\?\Volume{GUID}` devices now, instead of waiting for a drive letter that can lag the mount by many seconds.

## 0.4.1 — 2026-07-18

- Backup pages gained a backup chooser dropdown fed by the job history.
- About page shows a full identity column with provenance links (version, git hash, build time).
- Boot Repair results render as a full-width installs table.

## 0.4.0 — 2026-07-17

- **Boot Repair**: a new page (and `simulacra-cli boot-repair`) that detects Windows installations on a disk, previews a repair plan, and rewrites boot files (`bcdboot`/`bootsect`, UEFI and BIOS).
- Clone and Restore gained a "Repair boot files on the target" option (`--repair-boot`), default-on when the source layout looks bootable.

## 0.3.0 — 2026-07-17

- **Writable mounts**: mount a backup with a copy-on-write overlay, so the mounted volumes can be written to without touching the `.phnx`.

## 0.2.5 — 2026-07-16

- Much faster GUI rendering: span-based, parallel CPU rasterizer with steadier frame pacing.
- Full-disk restore plans grow the largest NTFS partition, not the latest-starting one.
- Binaries statically link the MSVC CRT (no VC++ redistributable needed).

## 0.2.2 – 0.2.4 — 2026-07-16

- Mount falls back to a drive letter when the chosen mount point can't be used as a directory.
- Start Backup stays disabled until a partition is selected.
- GUI polish: theme-toggle contrast, no dropped pixels on whole-pixel edges, window appears only once the first frame is drawn.

## 0.2.0 / 0.2.1 — 2026-07-16

- In-app updates offer "Restart to update"; portable bundles are never auto-updated.
- Portable ZIP bundle published alongside the installer.

## 0.1.x — 2026-07-15/16

First releases under the Phoenix Simulacra name. Highlights of everything that landed on the way here:

- The engine: selective partition backup to `.phnx` with used-block capture (NTFS/FAT/exFAT), restore with grow/shrink resizing, disk-to-disk clone (full and partial), format-v2 integrity checking with verify-after-backup on by default, and automatic lock-then-VSS source freezing.
- Zero-space read-only mounting via WinFsp, including partition-selective drive letters.
- BitLocker lock-state-aware capture (unlocked → plaintext, locked → ciphertext).
- Native 4Kn (4096-byte-sector) support end to end, plus opt-in 4Kn→512e sector-size conversion on restore/clone.
- x64 + ARM64 dual-arch bundle with an arch-selecting launcher; GUI renders on the CPU so it runs in WinPE.
- Inno Setup installer that bundles WinFsp; Azure Trusted Signing; self-update from the releases feed.
- Relicensed to GPL-3.0-or-later.
