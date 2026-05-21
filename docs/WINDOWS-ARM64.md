# Windows platform support (x64 and ARM64)

Carbon Phoenix treats **x86_64** and **ARM64** Windows as **tier-1, equal platforms**. The same source tree, CLI flags, GUI, capture modes, VSS behavior, and `.phnx` format apply to both. There are no alternate code paths, feature flags, or degraded modes for ARM64.

## Parity contract

| Rule | Implementation |
|------|----------------|
| One codebase | No `#[cfg(target_arch = …)]` in application logic (only `#[cfg(windows)]` for the OS). |
| Same binaries per arch | `carbon-phoenix.exe` and `carbon-phoenix-gui.exe` for each target; native PE, not x64 emulation on ARM. |
| Same GUI stack | `eframe` with `glow` on **both** targets (no ARM-only `wgpu` default). |
| Same Win32 APIs | Disk IOCTLs, volume access, VSS via `vssadmin`, admin manifest — identical on x64 and ARM64. |
| CI builds both | [.github/workflows/windows.yml](../.github/workflows/windows.yml) matrix: `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`. |
| Releases ship both | Use [scripts/build-release.ps1](../scripts/build-release.ps1) to produce `dist/x86_64-pc-windows-msvc/` and `dist/aarch64-pc-windows-msvc/`. |

If behavior differs between an x64 and an ARM64 build of the **same version**, that is a bug unless the difference comes from the OS or hardware (e.g. disk layout on the machine), not from Carbon Phoenix.

## Supported targets

| Rust target | Windows | Use |
|-------------|---------|-----|
| `x86_64-pc-windows-msvc` | 64-bit Intel/AMD | Desktop, laptops, x64 WinPE |
| `aarch64-pc-windows-msvc` | 64-bit ARM | Surface / Snapdragon PCs, ARM64 WinPE |

32-bit Windows is out of scope (not supported by design).

## Build

### All tier-1 targets (recommended for releases)

From a Windows dev machine with **Desktop development with C++** and both **x64** and **ARM64** MSVC components:

```powershell
.\scripts\build-release.ps1
```

Outputs:

- `dist/x86_64-pc-windows-msvc/carbon-phoenix.exe`
- `dist/x86_64-pc-windows-msvc/carbon-phoenix-gui.exe`
- `dist/aarch64-pc-windows-msvc/carbon-phoenix.exe`
- `dist/aarch64-pc-windows-msvc/carbon-phoenix-gui.exe`

### Single target (local dev)

Default host (whatever `rustc -vV` reports as host):

```powershell
cargo build --release
```

Explicit target (same flags and features as the other arch):

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

Cross-compiling ARM64 from an x64 PC requires the **MSVC v143 – ARM64 build tools** workload and an environment where ARM64 `cl.exe` is available (e.g. `ilammy/msvc-dev-cmd` with `arch: arm64`, or the ARM64 Native Tools prompt). If `zstd-sys` fails with “failed to find tool cl.exe”, the ARM64 C++ toolchain is not active in that shell.

On an ARM64 PC, building x64 likewise requires the x64 MSVC tools installed.

## WinPE

Use the binary that matches the WinPE architecture:

- x64 WinPE → `x86_64-pc-windows-msvc` build
- ARM64 WinPE → `aarch64-pc-windows-msvc` build

CLI and GUI behavior in WinPE are the same; only the PE architecture changes.

## Verification checklist (ARM64 hardware)

Run the same scenarios you would on x64 (elevated):

1. `carbon-phoenix list-disks`
2. Backup a small data partition → `list` / `verify`
3. GUI: backup with progress bar
4. Optional: `backup --vss` on a mounted system volume
5. Lab restore to a spare disk with `plan` → `restore`

Results should match x64 expectations for the same disks and options.

## Maintainer notes

- **GPT disk GUID** is read via `DRIVE_LAYOUT_INFORMATION_EX.Anonymous.Gpt.DiskId` (typed `windows-sys` field), not a manual byte offset — correct on all 64-bit Windows ABIs.
- **Dependencies** (`windows-sys`, `blake3`, `zstd`, `eframe`, `rfd`) publish `aarch64-pc-windows-msvc` artifacts; the workspace does not pin arch-specific crates.
- Do not add `target_arch`-gated behavior without updating this document and the parity table above; prefer fixes that apply to both targets.
