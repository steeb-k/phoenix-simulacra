# Changelog

Notable changes per release. Releases are published on the [releases page](https://github.com/steeb-k/phoenix-simulacra-binaries/releases).

## 0.4.1 — 2026-07-18

- Backup pages gained a backup chooser dropdown fed by the job history.
- About page shows a full identity column with provenance links (version, git hash, build time).
- Boot Repair results render as a full-width installs table.

## 0.4.0 — 2026-07-17

- **Boot Repair**: a new page (and `simulacra-cli boot-repair`) that detects Windows installations on a disk, previews a repair plan, and rewrites boot files (`bcdboot`/`bootsect`, UEFI and BIOS). Strictly drive-local — NVRAM is never touched.
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
