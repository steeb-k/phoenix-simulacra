# Carbon Phoenix — Backup and Restore

Windows-first disk backup tool written in Rust. Creates single-file `.phnx` backups with partition metadata, used-block capture for NTFS/FAT/exFAT, and raw capture for EFI/MSR partitions.

## Features

- GPT/MBR disk enumeration
- Selective partition backup and restore
- Used-block imaging (smaller than full disk images)
- BLAKE3 per-chunk integrity + verify command
- Verify-on-restore (default)
- VSS support for live Windows backups (`--vss` / GUI checkbox)
- CLI and egui GUI (WinPE-friendly, no WebView2)

## Build

Requires Rust 1.70+ and Windows for full functionality.

```bash
cargo build --release
```

Binaries (both embed a `requireAdministrator` manifest — Windows will prompt for UAC on launch):

- `target/release/carbon-phoenix.exe` — CLI
- `target/release/carbon-phoenix-gui.exe` — GUI

## CLI Usage

```bash
# List disks (UAC prompt appears automatically when launching the exe)
carbon-phoenix list-disks

# Backup partitions 1,2,3 on disk 0
carbon-phoenix backup --disk 0 --partitions 1,2,3 --output backup.phnx

# Live backup with VSS
carbon-phoenix backup --disk 0 --partitions 2 --output c.phnx --vss

# Inspect backup
carbon-phoenix list backup.phnx

# Verify
carbon-phoenix verify backup.phnx
carbon-phoenix verify backup.phnx --quick

# Restore
carbon-phoenix plan backup.phnx --disk 1 --output plan.toml
# Edit plan.toml (partition sizes/offsets)
carbon-phoenix restore backup.phnx --plan plan.toml
```

## Format

See [docs/phnx-format.md](docs/phnx-format.md).

## Requirements

- Administrator privileges (requested automatically via embedded manifest)
- BitLocker volumes must be unlocked
- Restore to different hardware may require Windows Startup Repair / `bcdedit`

## License

MIT
