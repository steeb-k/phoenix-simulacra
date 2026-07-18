# Roadmap

What's planned, what's deliberately deferred, and what's out of scope. The engine — backup, restore, clone, mount, verify, BitLocker, boot repair, 4Kn — is feature-complete and validated on real hardware; see [TESTING.md](TESTING.md) for exactly what "validated" means.

## Planned

**Performance.** The parallel chunk pipeline already overlaps device I/O with hashing and compression; further avenues are deferred until real-hardware profiling says they'd pay:

- Adaptive compression (skip zstd for incompressible chunks — needs a stored-raw chunk flag, a format change)
- Device-aware parallel reads (multiple readers over disjoint extents on NVMe; strictly sequential on spinning/USB media)
- An optional sampled verify-after tier for speed-sensitive runs (full verification stays the default)

**Boot backups in a VM.** Writable copy-on-write overlay mounts shipped in 0.3.0; the follow-on is booting a mounted backup directly in QEMU. Exploratory plan: [QEMU-BOOT.md](QEMU-BOOT.md).

**ARM64 validation.** Backup, mount, and the full T2 virtual-disk suite (restore, clone, resize, 4Kn) are proven on real ARM64 hardware; the destructive real-disk (T3) tiers haven't run there yet, and BitLocker has no ARM64 test coverage (Pro-SKU test fixture). See [ARM64.md](ARM64.md).

**Boot repair, proven by booting.** The Boot Repair tool is validated end to end on scratch VHDs (GPT/UEFI and MBR/BIOS); a real-hardware boot-after-repair test is still owed. Same for a real-media partial-clone run.

**Restore layout editor polish.** Surface the true minimum shrink size (the editor currently floors at used bytes, which can be optimistic), a live validation banner, and sharing the editor with the Clone page.

**exFAT sector-size conversion.** NTFS and FAT32 are covered; exFAT's boot-region checksum makes it more work.

## Known caveats (by design)

- **Verification after backup is on by default** and adds a full extra read pass. The CLI re-reads the frozen source and confirms the image matches it (`backup --no-verify` opts out); the GUI releases the source immediately and instead runs a full BLAKE3 check of the written image.
- **BitLocker locked volumes are captured as raw ciphertext**, loudly flagged; restore reproduces the locked volume, which still needs the original key. There is no prompt-to-unlock or re-encrypt-on-restore flow. After restoring ciphertext to *removable* media, Windows may not recognize the locked volume until an unplug/replug or reboot (an OS cache quirk — the on-disk bytes are correct throughout).
- **Sector-size conversion is 4Kn→512e only, and opt-in.** 512e→4Kn is refused (fine-to-coarse can't be represented), a converted partition can't also be shrunk, and exFAT partitions are left unconverted with a warning. See [SECTOR-SIZE-CONVERSION.md](SECTOR-SIZE-CONVERSION.md).
- **GPT on removable USB media isn't possible** (Windows policy), so removable targets are MBR.
- **Mount requires WinFsp.** A build without the `winfsp` feature falls back to materializing a full-size temp VHD — a dev-only stopgap that doubles the backup's footprint and is never used in release builds.
- **Restoring to dissimilar hardware** may still need Windows Startup Repair beyond what the built-in Boot Repair covers.

## Out of scope

- **Incremental / differential backups** — the v2 format reserves fields for them, but they are not planned near-term.
- **Resumable (partial) backups** — considered and rejected: a VSS snapshot dies with the process, so resuming a live-source backup would mix two points in time. Interrupted backups restart.
- **Cloud upload, sync, scheduling** — write the `.phnx` wherever you like; orchestration is not this tool's job.
- **ReFS** and **32-bit Windows**.
