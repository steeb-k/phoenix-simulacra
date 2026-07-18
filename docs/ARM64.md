# Windows on ARM (ARM64)

Phoenix Simulacra targets **x86_64** and **ARM64** Windows from one source tree: the same CLI flags, GUI, capture modes, VSS behavior, and `.phnx` format on both.

## Parity contract

| Rule | Reality |
|------|---------|
| One codebase | No `#[cfg(target_arch)]` branches in application logic. The only OS gate is `#[cfg(windows)]`. |
| Same binaries per arch | `simulacra.exe` (GUI) + `simulacra-cli.exe` (CLI) per target; native PE, not x64 emulation. |
| Same GUI stack | The GUI renders on the **CPU** (softbuffer + a span-based parallel rasterizer) on both arches — see "Rendering" below. |
| Same Win32 APIs | Disk IOCTLs, volume locking, and the admin manifest are arch-neutral. |
| Same VSS mechanism | PowerShell + `Win32_ShadowCopy` — see below. |
| Arch-sensitive dependencies | `winfsp` / `winfsp-sys` only — see "WinFsp" below. |

If an x64 and an ARM64 build of the same commit behave differently, that is a bug unless the difference comes from the OS or the hardware (disk geometry, sector size).

32-bit Windows is out of scope by design.

## Validation status

Proven on real ARM64 hardware (a Snapdragon laptop with a true-4Kn UFS system disk):

- **GUI renders**, disks enumerate, and the **live system disk backs up** end to end — all three freeze arms fired correctly (volume lock, VSS escalation on `C:`, unfrozen-with-image-verify on the un-lettered ESP), with used-block NTFS capture off a 4Kn bitmap.
- **Mount works** through the native `winfsp-a64.sys` driver: drive letters appear, and free space drops by nothing beyond the image itself — the zero-space path, not the fallback.
- **VSS works on a Home SKU** (one reason the engine uses `Win32_ShadowCopy` rather than the Server-only `vssadmin create shadow`).
- **T1 unit tests pass natively**, and the **full T2 virtual-disk suite passes on ARM64** — backup/restore round-trips, clone, resize, partial restore/clone, mount, VSS, and the 4Kn tier, run staged with no hangs. (The BitLocker binary is the one exception: its fixture needs a Pro+ cmdlet and the reference box is Home.)

Bringing the engine up on ARM64 hardware found three real bugs — UFS firmware LUNs enumerating as disks (and being offered as clone targets), a hardcoded 512-byte boot-sector read that 4Kn devices reject outright, and an unreadable error when the backup destination fell off the USB bus — all since fixed for both arches.

Still unvalidated on ARM64 hardware: the **destructive real-disk (T3) tiers**, and BitLocker has no ARM64 *test* coverage (the fixture needs `Enable-BitLocker`, a Pro+ cmdlet); the product's BitLocker handling is not SKU-gated.

## Rendering: why the GUI draws on the CPU

Windows on ARM ships **no desktop OpenGL driver** — `opengl32.dll` falls back to a software rasterizer stuck at OpenGL 1.1, so any GL-based renderer (egui's `glow` backend included) is dead on arrival. GPU-less environments like WinPE have the same problem for every hardware backend.

The GUI therefore renders egui output on the **CPU** via `softbuffer` and a span-based, parallelized rasterizer. One renderer, every environment: desktop x64, ARM64, and WinPE all draw the same way, and there is no fallback path nobody exercises.

Do not "fix" rendering by shipping an OpenGL implementation (ANGLE provides GL *ES*, not desktop GL; Mesa's llvmpipe is just a slower software rasterizer than the one we have).

## VSS: `Win32_ShadowCopy`, not `vssadmin`

`phoenix-vss` creates snapshots by shelling out to PowerShell and calling `Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create`. `vssadmin create shadow` is Windows-Server-only and is not used (except in cleanup). This matters on ARM64 because the path depends on PowerShell + WMI — both native on ARM64 Windows, but slower to start on low-powered machines, so freeze escalation can feel latency-sensitive there.

## WinFsp: the one arch-sensitive dependency

The `winfsp` crate resolves the right DLL at run time (`winfsp-a64.dll` on ARM64) through the registry, including a fallback from `SOFTWARE\WOW6432Node\WinFsp` to the native `SOFTWARE\WinFsp` key. The stock WinFsp MSI is multi-arch and ships both drivers. Verified working on ARM64 hardware.

One build-time gotcha: `winfsp-sys`'s build script reads **only** `HKLM\SOFTWARE\WOW6432Node\WinFsp`. Cross-compiling from an x64 host is fine (the x64 MSI populates that key), but building natively on an ARM64 box whose WinFsp registered under the native key only will panic. Mirror the key if you hit it:

```powershell
New-Item -Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Name InstallDir `
  -Value (Get-ItemProperty 'HKLM:\SOFTWARE\WinFsp').InstallDir
```

(The installer does this mirroring automatically on ARM64.)

## Developing and testing for ARM64

The intended workflow is to **develop on x64 and cross-build** (see [BUILDING.md](BUILDING.md)) — an ARM64 machine is a test target, not a dev box. Cross-build the test executables too, so the target machine needs no toolchain:

```powershell
cargo test --no-run --release --target aarch64-pc-windows-msvc -p phoenix-systests
```

then copy the binaries over and run them there.

Notes for low-spec ARM64 hardware (4 GB RAM class):

- The parallel chunk pipeline caps in-flight buffers at roughly 128 MiB at its default worker count; lower it with `$env:PHOENIX_WORKERS = "2"` if the machine thrashes.
- Keep destructive-test partitions small (a few GB); `PHOENIX_T3_LAYOUT_GB` caps restore grow-to-fill.
- Never use the non-winfsp mount fallback here — it materializes a full-size temp VHD and will likely exhaust the disk.

Many UFS/NVMe devices are **512e** (4096-byte physical, 512-byte logical sectors) rather than true 4Kn — both are fully supported, but measure before assuming:

```powershell
Get-Disk | Select-Object Number, FriendlyName, BusType, LogicalSectorSize, PhysicalSectorSize
```
