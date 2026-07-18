# Phoenix Simulacra

**[Download the latest release](https://github.com/steeb-k/phoenix-simulacra-binaries/releases/latest)** -- installer and portable ZIP, x64 + ARM64 in one download.

A disk backup, restore, and cloning tool for Windows, written in Rust. Simulacra is designed to be fast, portable, and low on resources. Intended for use by computer repair technicians with no paywalled features, no complicated setup, and an easy-to-use interface. A CLI version is available for headless use, scripting or automating, if required. 

Phoenix Simulacra images the partitions you pick into a single compressed `.phnx` file, restores them with resizing, clones disk-to-disk (running system disk included), and lets you browse a backup's files in Explorer without extracting anything. It runs on Windows x64 and ARM64, with the same features on both.

## Features

- **Backs up only what's used.** NTFS, FAT, and exFAT partitions are captured used-blocks-only, so images stay close to the size of your data, not the size of the disk.
- **Backs up live systems.** Idle volumes are captured under an exclusive lock; busy ones (like the running `C:`) automatically go through a VSS snapshot. There's no switch to get wrong; the engine picks the strongest freeze the volume allows, and refuses to capture anything it can't freeze.
- **Verifies its own work.** Every chunk is BLAKE3-hashed, and by default every backup ends with a full verification pass — the CLI re-reads the source and confirms the image matches it; the GUI re-checks every chunk of the written image.
- **Restores with resizing.** Grow or shrink partitions during restore, with a drag-and-drop layout editor in the GUI and an editable plan file in the CLI.
- **Clones disk-to-disk.** Full or partial, with resizing, straight from a live system.
- **Mounts backups in place.** Browse a `.phnx` in Explorer with drive letters and essentially zero extra disk space, read-only or with a writable copy-on-write overlay. (Requires [WinFsp](https://winfsp.dev); the installer sets it up.)
- **Repairs boot files.** A built-in Boot Repair tool (`bcdboot`/`bootsect`, UEFI and BIOS), and clone/restore can fix up the target's boot files automatically so a restored disk actually boots.
- **Handles BitLocker.** Unlocked volumes back up as normal plaintext images; locked volumes are captured as raw ciphertext (clearly flagged) and restore still locked.
- **Handles 4Kn disks.** Native 4096-byte-sector support end to end, plus opt-in 4Kn→512e conversion when restoring or cloning to a 512-byte-sector disk, meaning you can clone from UFS media to standard drives.
- **Works in WinPE.** The GUI renders on the CPU. No GPU, WebView2, or OpenGL needed; so it runs in recovery environments.

## Download

Grab the installer (`Simulacra-Setup-<version>.exe`) or the portable ZIP from the [releases page](https://github.com/steeb-k/phoenix-simulacra-binaries/releases). One download covers both x64 and ARM64 — a launcher picks the right binary for your machine. The installer also sets up WinFsp so mounting works out of the box, and the app can update itself from the same releases feed.

To build from source instead, see [docs/BUILDING.md](docs/BUILDING.md).

## Quick start (CLI)

The GUI (`simulacra.exe`) covers everything below with the same engine. Both request Administrator via UAC on launch.

```powershell
simulacra-cli list-disks                                              # see what's attached
simulacra-cli backup --disk 0 --partitions 1,2,3 -o backup.phnx       # back up (freeze is automatic)
simulacra-cli verify backup.phnx                                      # re-hash every chunk
simulacra-cli mount backup.phnx                                       # browse it in Explorer
simulacra-cli plan backup.phnx --disk 1 -o plan.toml                  # draft a restore layout
simulacra-cli restore backup.phnx --plan plan.toml                    # restore it
```

For capture modes, freeze behavior, and the full CLI/GUI reference, see the [backup guide](docs/BACKUP.md).

## Documentation

- [docs/BACKUP.md](docs/BACKUP.md) — user guide: how backups work, capture modes, CLI and GUI reference
- [docs/BUILDING.md](docs/BUILDING.md) — building from source, release bundles, the installer
- [docs/phnx-format.md](docs/phnx-format.md) — the `.phnx` file format specification
- [docs/TESTING.md](docs/TESTING.md) — the test tiers (unit / virtual disk / real disk) and how to run them
- [docs/ARM64.md](docs/ARM64.md) — Windows on ARM: parity contract, platform notes, validation status
- [docs/ROADMAP.md](docs/ROADMAP.md) — what's planned, known caveats, what's out of scope

## Requirements

- Windows 10/11, x64 or ARM64
- [WinFsp](https://winfsp.dev) for mounting backups — bundled with the installer
- Restoring to different hardware may need Windows Startup Repair; the built-in Boot Repair covers the common cases

## License

GNU General Public License v3.0 or later (GPL-3.0-or-later). See [LICENSE](LICENSE).

The bundled [Inter](https://github.com/rsms/inter) fonts are licensed under the SIL Open Font License 1.1 — see [assets/OFL.txt](assets/OFL.txt).
