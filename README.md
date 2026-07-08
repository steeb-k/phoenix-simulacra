# Carbon Phoenix — Backup and Restore

Windows disk backup tool written in Rust. Creates single-file `.phnx` backups with partition metadata, used-block capture for NTFS/FAT/exFAT, and raw capture for EFI/MSR partitions.

**Platforms:** Tier-1 support for **Windows x64** and **Windows ARM64** with identical behavior; ship the native binary for each architecture. See [docs/WINDOWS-ARM64.md](docs/WINDOWS-ARM64.md).

## Features

- GPT/MBR disk enumeration
- Selective partition backup and restore
- Used-block imaging (smaller than full disk images)
- Restore with partition resizing (NTFS grow + shrink-with-relocation, FAT/exFAT grow)
- **Disk-to-disk cloning** (`clone`), including live system disks via VSS, with resize
- **Read-only mounting** (`mount`) — browse a backup's files in Explorer; the image
  is materialized to a sparse VHD (footprint ≈ used size) and attached read-only
- Bulletproof verification: format v2 checksums every metadata structure (footer CRC,
  total-length/truncation check, index-table hash, per-entry CRC); `verify --quick`
  runs a structural + sampled check, `verify` hashes every chunk
- Verify-on-restore (default) and optional read-back verification on clone
- VSS support for live Windows backups (`--vss` / GUI checkbox)
- CLI and egui GUI (WinPE-friendly, no WebView2)

> **Mount is not yet space-efficient.** The current mount materializes a full-size
> temporary VHD (Windows rejects sparse fixed VHDs). The required implementation —
> tracked as top priority — is a **WinFsp on-demand mount** that serves reads
> straight from the `.phnx` with **zero extra disk space**; mounting must never
> double a backup's footprint. Until that lands, mount is a stopgap suitable only
> for small backups.

## Build

Requires Rust 1.70+, Windows, and Visual Studio Build Tools (**Desktop development with C++**, including **x64** and **ARM64** MSVC when building both targets).

**Release builds (x64 + ARM64):**

```powershell
.\scripts\build-release.ps1
```

**Single-target dev build** (host default):

```bash
cargo build --release
```

Binaries embed a `requireAdministrator` manifest (UAC on launch). Paths depend on target:

| Target | CLI | GUI |
|--------|-----|-----|
| Host default | `target/release/carbon-phoenix.exe` | `target/release/carbon-phoenix-gui.exe` |
| `x86_64-pc-windows-msvc` | `target/x86_64-pc-windows-msvc/release/...` | same |
| `aarch64-pc-windows-msvc` | `target/aarch64-pc-windows-msvc/release/...` | same |

Packaged copies from the release script: `dist/<target>/`.

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

## Documentation

- [BACKUP.md](BACKUP.md) — Backup process, capture modes, VSS, and options
- [docs/WINDOWS-ARM64.md](docs/WINDOWS-ARM64.md) — x64 / ARM64 parity and build matrix
- [docs/phnx-format.md](docs/phnx-format.md) — `.phnx` file format specification

## Requirements

- Administrator privileges (requested automatically via embedded manifest)
- BitLocker volumes must be unlocked
- Restore to different hardware may require Windows Startup Repair / `bcdedit`

## License

MIT
