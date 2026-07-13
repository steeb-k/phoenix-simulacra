# Carbon Phoenix — Backup and Restore

Windows disk backup tool written in Rust. Creates single-file `.phnx` backups with partition metadata, used-block capture for NTFS/FAT/exFAT, and raw capture for EFI/MSR partitions.

**Platforms:** Tier-1 support for **Windows x64** and **Windows ARM64** with identical behavior; ship the native binary for each architecture. See [docs/WINDOWS-ARM64.md](docs/WINDOWS-ARM64.md).

## Features

- GPT/MBR disk enumeration
- Selective partition backup and restore
- Used-block imaging (smaller than full disk images)
- Restore with partition resizing (NTFS grow + shrink-with-relocation, FAT/exFAT grow)
- **Disk-to-disk cloning** (`clone`), including live system disks via VSS, with resize
- **Read-only mounting** (`mount`) — browse a backup's files in Explorer with **zero
  extra disk space**: a WinFsp filesystem serves a synthesized `backup.vhd` on demand
  straight from the compressed `.phnx`, and Windows attaches it read-only
- Bulletproof verification: format v2 checksums every metadata structure (footer CRC,
  total-length/truncation check, index-table hash, per-entry CRC); `verify --quick`
  runs a structural + sampled check, `verify` hashes every chunk
- **Verify-after-backup (default on):** immediately re-reads the source and confirms
  every chunk matches — proving the image faithfully captured the disk, not just that
  the file is internally consistent (opt out with `backup --no-verify`)
- Verify-on-restore (default) and optional read-back verification on clone
- **Automatic source freeze — no switch to get wrong:** every backup takes an exclusive
  volume lock when it can, falls back to a VSS shadow when the volume is in use (the
  running Windows volume always is), and refuses to capture a lettered volume it can
  freeze neither way
- CLI and egui GUI (WinPE-friendly, no WebView2)

> **Mounting requires WinFsp** — the `winfsp` build feature is **on by default**
> for the GUI and CLI, so dev and release builds behave the same. It adds no
> meaningful disk footprint — reads are served on demand from the compressed
> `.phnx`, and you pick which partition(s) get drive letters. Install WinFsp
> from [winfsp.dev](https://winfsp.dev) (release builds bundle it). Building
> with `--no-default-features` drops the feature; mount then falls back to
> materializing a full-size temp VHD — a dev-only stopgap.

## Build

Requires Rust 1.70+, Windows, and Visual Studio Build Tools (**Desktop development with C++**, including **x64** and **ARM64** MSVC when building both targets). The default `winfsp` feature additionally needs LLVM/libclang (set `LIBCLANG_PATH` if LLVM isn't at `C:\Program Files\LLVM`) and WinFsp installed for its SDK headers.

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

# Backup partitions 1,2,3 on disk 0. Idle volumes are captured under an exclusive
# lock; volumes in use (the live C: among them) are captured through a VSS shadow —
# there is nothing to choose.
carbon-phoenix backup --disk 0 --partitions 1,2,3 --output backup.phnx

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

- [docs/TESTING.md](docs/TESTING.md) — **test tiers (unit / VHD / real-disk), how to run them, integrity guarantees**
- [docs/ROADMAP.md](docs/ROADMAP.md) — **remaining work, known caveats, and out-of-scope items**
- [BACKUP.md](BACKUP.md) — Backup process, capture modes, VSS, and options
- [docs/WINDOWS-ARM64.md](docs/WINDOWS-ARM64.md) — x64 / ARM64 parity and build matrix
- [docs/phnx-format.md](docs/phnx-format.md) — `.phnx` file format specification
- [scripts/live-smoke-checklist.md](scripts/live-smoke-checklist.md) — manual pre-release live-system checklist

## Requirements

- Administrator privileges (requested automatically via embedded manifest)
- BitLocker volumes must be unlocked
- Restore to different hardware may require Windows Startup Repair / `bcdedit`

## License

MIT
