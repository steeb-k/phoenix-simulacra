# Building Phoenix Simulacra

## Prerequisites

- **Rust** (stable, 1.70+) with the MSVC toolchain
- **Visual Studio Build Tools** — *Desktop development with C++*, including the **x64** and **ARM64** MSVC components if you build both targets
- **LLVM/libclang** — the default `winfsp` feature generates bindings at build time. Set `LIBCLANG_PATH` if LLVM isn't at `C:\Program Files\LLVM`
- **WinFsp** installed ([winfsp.dev](https://winfsp.dev)) — its SDK headers are needed at build time, and the driver at run time for mounting

## Dev build

```powershell
cargo build --release
```

Builds for the host architecture. Binaries land in `target/release/`:

| Binary | What |
|--------|------|
| `simulacra.exe` | GUI |
| `simulacra-cli.exe` | CLI |

Both embed a `requireAdministrator` manifest, so launching them triggers UAC. The GUI's console output is hidden unless launched with `--debug`.

Building with `--no-default-features` drops the `winfsp` feature (useful to sidestep the LLVM/WinFsp build prerequisites), but mount then falls back to materializing a full-size temp VHD — a dev-only stopgap, never how mount should be evaluated or shipped.

## Release bundle (x64 + ARM64)

```powershell
.\scripts\build-release.ps1
```

Cross-compiles both targets from an x64 host and assembles a single shippable bundle in `dist/simulacra/`:

| File | Arch | What |
|------|------|------|
| `simulacra-launcher.exe` | x64* | Picks and launches the right GUI for the machine |
| `simulacra.exe` | x64 | GUI |
| `simulacra-cli.exe` | x64 | CLI |
| `simulacra-arm.exe` | ARM64 | GUI |
| `simulacra-cli-arm.exe` | ARM64 | CLI |

\* The launcher ships as x64 only — Windows on ARM runs it under emulation, it detects the native machine (`IsWow64Process2`), and launches the matching GUI. It's the normal entry point; the per-arch exes can also be run directly.

Single-target cross-build, if that's all you need:

```powershell
cargo build --release --target aarch64-pc-windows-msvc
```

The `cc` crate finds the ARM64 cross toolchain on its own when the MSVC ARM64 component is installed; if `zstd-sys` fails with "failed to find tool cl.exe", that component is what's missing.

The bundle does not include WinFsp — mounting on a target machine needs WinFsp installed there. The installer (below) takes care of that.

## Installer

```powershell
pwsh scripts\build-installer.ps1
# -> dist\Simulacra-Setup-<version>.exe
```

Runs `build-release.ps1`, downloads and verifies the pinned WinFsp MSI, and compiles the Inno Setup script. Requires Inno Setup 6.6.0+ (`winget install JRSoftware.InnoSetup`). Details — what it installs, silent-install flags for the auto-updater, uninstall behavior, WinFsp licensing — are in [../installer/README.md](../installer/README.md).

## Verifying a build

```powershell
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo test -p phoenix-core -p phoenix-capture -p phoenix-restore `
           -p phoenix-clone -p phoenix-mount -p phoenix-vss
```

The workspace is kept rustfmt- and clippy-clean. Note `cargo test --workspace` fails from a normal shell with `os error 740`: the CLI and GUI test binaries inherit the `requireAdministrator` manifest, so use the crate list above (or an elevated shell). The full test-tier story — including the elevated virtual-disk and real-disk suites — is in [TESTING.md](TESTING.md).
