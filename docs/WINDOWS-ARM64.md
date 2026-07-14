# Windows platform support (x64 and ARM64)

Carbon Phoenix targets **x86_64** and **ARM64** Windows from one source tree: the
same CLI flags, GUI, capture modes, VSS behavior, and `.phnx` format. There are
no `#[cfg(target_arch)]` branches in application logic.

**Status (2026-07-14): the engine RUNS on ARM64 — backup and mount, end to end.**
On a Snapdragon 7c Gen 2 / Windows 10 Home box whose internal UFS disk is **true
4Kn**, Carbon Phoenix imaged the live system disk and mounted the result:

| Proven on ARM64 hardware | Evidence |
|---|---|
| **GUI renders** | wgpu/DX12, after two renderer crashes (glow → wgpu, then DX12 by name). See "The OpenGL problem". |
| **Disk enumeration** | …after a real bug: the `PhysicalDriveN` scan was reporting the UFS **firmware LUNs** as disks, and offering them as clone/restore *targets*. Now enumerates `GUID_DEVINTERFACE_DISK`. |
| **Backup, live system disk** | All three freeze arms fired correctly: `FSCTL_LOCK_VOLUME` on Recovery, **VSS escalation** on C:, unfrozen+verify-image on the un-lettered ESP. Used-block NTFS capture off a 4Kn bitmap. 61 GB used → 31.3 GB image. |
| **Mount (`winfsp-a64.sys`)** | First-ever run of the ARM64 WinFsp driver. All partitions got drive letters; **destination free space dropped by the image size and nothing more**, so the zero-space path — not the materialize fallback — is what ran. Clean unmount, no leaked shadows. |
| **VSS on a Home SKU** | `Win32_ShadowCopy.Create` → 0. (`vssadmin create shadow` isn't even a supported command on a client SKU — which is exactly why we don't use it.) |
| **T1 unit tests, natively** | 103/103 across `core`/`capture`/`restore`/`clone`/`vss`. |

**4Kn and UFS are no longer untested-on-any-hardware.** They are this machine's
ordinary case, and they cost two real bugs — the firmware-LUN enumeration above,
and a 512-byte boot-sector read that is *illegal* on 4Kn and had silently
disabled both no-drive-letter FS detection and all BitLocker volume-view
detection.

**Still unrun on ARM64, and therefore still expectation rather than observation:**
**restore**, **clone**, the **T2 virtual-disk** and **T3 destructive** tiers, and
`phoenix-mount`'s own unit tests. **BitLocker has no ARM64 coverage at all** — the
test needs `Enable-BitLocker` to build its fixture, and that cmdlet is Pro+ while
this box is Home. (The *product's* BitLocker handling is not SKU-gated; the test
is.) Treat the checklist at the bottom as the plan for what remains.

## Parity contract

| Rule | Reality |
|------|---------|
| One codebase | ✅ No `#[cfg(target_arch)]` in application logic. The only OS gate is `#[cfg(windows)]` / `target_os = "windows"` (GUI fonts, titlebar, theme). |
| Same binaries per arch | ✅ `carbon-phoenix.exe` + `carbon-phoenix-gui.exe` per target; native PE, not x64 emulation. |
| Same GUI stack | ✅ `eframe = { default-features = false, features = ["wgpu"] }` (phoenix-gui/Cargo.toml) — **wgpu (DX12) on both, glow on neither**. It *was* glow, until glow turned out not to start on ARM64 at all. See "The OpenGL problem". |
| Same Win32 APIs | ✅ Disk IOCTLs, volume locking, and the admin manifest are arch-neutral. |
| Same VSS mechanism | ✅ …but **not** `vssadmin`. See below. |
| Arch-sensitive dependencies | ✅ **`winfsp` / `winfsp-sys` only.** These *do* branch on `target_arch` — and as of 2026-07-14 that branch is **observed correct**: a cross-built ARM64 binary delay-loads `winfsp-a64.dll` and mounts through `winfsp-a64.sys`. The stock WinFsp MSI is multi-arch and ships both. See "The WinFsp problem". |
| CI builds both | ❌ **There is no CI.** `.github/` was deleted 2026-07-13; all builds and validation are local and manual. |

If an x64 and an ARM64 build of the same commit behave differently, that is a bug
unless the difference comes from the OS or the hardware (disk geometry, sector
size), not from Carbon Phoenix.

### VSS is PowerShell + `Win32_ShadowCopy`, not `vssadmin`

Earlier revisions of this document claimed "VSS via `vssadmin`". That is **wrong**
and was never true of the shipped code. `phoenix-vss` shells out to PowerShell and
calls the CIM/WMI method `Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName
Create` (`phoenix-vss/src/lib.rs:158-190`). The module doc explicitly rejects both
`vssadmin create shadow` (Windows **Server** only) and the native
`IVssBackupComponents` COM API. `vssadmin` survives in exactly one place:
`delete_all_snapshots()` (`lib.rs:120`).

This matters for ARM64 because it means VSS goes through WMI and PowerShell, both
of which are native on ARM64 Windows — so the mechanism is expected to work, but
it is **PowerShell-startup-latency sensitive**, which is far more noticeable on a
low-powered ARM laptop than on a desktop.

## Supported targets

| Rust target | Windows | Use |
|-------------|---------|-----|
| `x86_64-pc-windows-msvc` | 64-bit Intel/AMD | Desktop, laptops, x64 WinPE |
| `aarch64-pc-windows-msvc` | 64-bit ARM | Snapdragon / Surface PCs, ARM64 WinPE |

32-bit Windows is out of scope by design.

---

## The OpenGL problem (found on real hardware, 2026-07-14)

The first time the GUI was ever launched on a Windows 11 ARM machine, it refused
to start:

```
Failed to set swap interval due to error: swap control extensions are not supported
Exiting because of error: egui_glow: OpenGL: egui_glow requires opengl 2.0+
```

**This is the platform, not a bug in our code, and no driver update fixes it.**
Windows on ARM ships **no desktop OpenGL driver**. `opengl32.dll` falls back to
Microsoft's GDI generic software rasterizer, which is **OpenGL 1.1** — so there is
no `WGL_EXT_swap_control` (hence the first warning) and no GL 2.0 (hence the hard
error). Qualcomm's Adreno driver exposes **D3D11/12 and Vulkan**, not a desktop GL
ICD. Any renderer that asks for desktop OpenGL is dead on arrival here, and
`egui_glow` asks.

**Fix: the GUI now uses eframe's `wgpu` backend, which targets DX12 on Windows.**
Both arches use it — the alternative, gating the renderer on `target_arch`, would
have broken the parity contract at the top of this document *and* left the x64
build as the only one anyone had ever seen render.

### …and DX12 must be asked for BY NAME (the second crash, same day)

Switching to wgpu was necessary but **not sufficient**. The first wgpu build
crashed on ARM64 too:

```
wgpu_hal::vulkan::instance: Unable to find extension: VK_KHR_surface, VK_KHR_win32_surface, …
wgpu_hal::gles::adapter: Returned GL context is 1.1, when 3.3+ is needed
eframe::native::run: Exiting because of error: WGPU error: Failed to create wgpu adapter, no suitable adapter found
```

Read what it *tried*: Vulkan, then GLES. **Never DX12** — because DX12 wasn't in
the binary. In wgpu 22, **`dx12` is an opt-in cargo feature**, while Vulkan and
GLES enable themselves by target cfg; and eframe 0.29 pulls wgpu with only
`metal` + `webgpu`, which on Windows amounts to **no backend at all**. So the
build offered Vulkan and GLES and nothing else.

That is invisible on x64 (Vulkan works on Intel/AMD/NVIDIA — the x64 GUI rendered
fine). On Snapdragon it is fatal: the Adreno Vulkan ICD exposed no usable surface
extensions, GLES fell straight back into the same OpenGL 1.1 software rasterizer,
and wgpu correctly concluded there was no adapter it could use.

Fix: `phoenix-gui` now depends on `wgpu` **directly**, with
`features = ["dx12", "wgsl"]`, so cargo's feature unification turns DX12 on inside
eframe's copy of wgpu. Confirmed linked, not merely flagged — the ARM64 PE imports
`D3D12CreateDevice`, `d3d12.dll`, and `dxgi.dll`.

**The lesson generalizes:** an x64 box has three working backends and hides this
class of bug completely. If the renderer stack ever changes again, check what wgpu
*enumerates*, not just that it builds.

Three notes for whoever touches this next:

- **Do not "fix" this by shipping an OpenGL implementation.** ANGLE gives GL *ES*,
  not desktop GL, and Mesa's `opengl32.dll` (llvmpipe) is a software rasterizer —
  we would be shipping a DLL to make the GUI slower on the machine that needs the
  most help. wgpu talks to the driver that actually exists.
- **The `on_exit` signature changes with the backend.** eframe hands `on_exit` a
  `glow::Context` only when the `glow` feature is on; with wgpu it takes no
  argument. If a future eframe bump reintroduces a build error there, that is why.
- **`glow` and `wgpu` can both be compiled in**, with the renderer chosen at
  runtime via `NativeOptions::renderer`. We deliberately don't: one backend on one
  code path is the whole point of the parity contract, and a fallback nobody
  exercises is a fallback nobody can trust.

**Verified 2026-07-14: the wgpu/DX12 GUI renders on BOTH arches**, ARM64 included —
observed on real Windows 11 ARM hardware, not inferred. That is the first time any
Carbon Phoenix binary has drawn a pixel on ARM64. It says nothing yet about the
*engine* on ARM64: see the test plan below, all of which is still unrun.

## The WinFsp problem (read this before building on ARM64)

`winfsp` is a **default feature** of both `phoenix-cli` and `phoenix-gui`, so a
plain `cargo build` pulls in `winfsp-sys`, which needs the **WinFsp SDK headers at
build time** (via bindgen/libclang) and the **WinFsp driver at run time**. This is
the only arch-sensitive part of the tree, and it cuts three ways.

**1. Runtime DLL selection is ARM64-correct.** The `winfsp` crate's `system`
feature resolves the DLL through the registry and picks `winfsp-a64.dll` on
`target_arch = "aarch64"` (`winfsp-0.13.0/src/init.rs`), including a documented
fallback from `SOFTWARE\WOW6432Node\WinFsp` to `SOFTWARE\WinFsp` because "ARM64
native installers write directly under SOFTWARE". The delay-load link arg
(`/DELAYLOAD:winfsp-a64.dll`) is emitted the same way. Verified against the
upstream crate source, not a local patch. **Nothing to do here.**

**2. The build script is NOT ARM64-safe — this is a likely first-build failure.**
`winfsp-sys-0.12.1/build.rs` reads **only** `HKLM\SOFTWARE\WOW6432Node\WinFsp` and
`.expect("WinFsp installation directory not found.")` — it has **no fallback** to
the native `SOFTWARE\WinFsp` key that its own runtime counterpart documents.

- Cross-compiling to aarch64 **from an x64 host**: fine (reads the host's
  WOW6432Node key, which the x64 MSI populates).
- Building **natively on the ARM64 laptop**: if the ARM64 WinFsp installer
  registers under the native key only, **the build panics**.

Workarounds, cheapest first:
```powershell
# Check which key the ARM64 WinFsp MSI actually wrote:
Get-ItemProperty 'HKLM:\SOFTWARE\WinFsp' -Name InstallDir -EA SilentlyContinue
Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Name InstallDir -EA SilentlyContinue

# If only the native key exists, mirror it so winfsp-sys' build.rs finds it:
New-Item -Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Name InstallDir `
  -Value (Get-ItemProperty 'HKLM:\SOFTWARE\WinFsp').InstallDir
```
Or sidestep it entirely for a first smoke test — `cargo build --no-default-features`
builds without WinFsp. **But note that is a degraded mode** (see next point).

**3. `--no-default-features` is a degraded mode, not a neutral one.** Without the
`winfsp` feature, `mount` falls back to materializing a **full-size temp VHD** —
the dev-only stopgap that doubles a backup's on-disk footprint. On a 4 GB ARM
laptop with limited storage this will likely fail outright on disk space. Use it
to unblock a build, never to evaluate mount behavior.

**Build-time prerequisites on ARM64:** LLVM (for `libclang`, bindgen) and the
WinFsp MSI, both ARM64 builds.

---

## Build

### Both targets (releases)

```powershell
.\scripts\build-release.ps1
```
Produces `dist/<target>/carbon-phoenix.exe` and `carbon-phoenix-gui.exe` for both
targets.

✅ **Verified working, x64 host → both targets (2026-07-13.)** All four binaries
build from an ordinary shell, and the PE `Machine` field confirms them: `0x8664`
for x64, `0xAA64` for ARM64. An earlier revision of this document warned that the
script does no per-target MSVC environment switch and would likely die in
`zstd-sys` with "failed to find tool cl.exe" — **it doesn't.** The `cc` crate finds
the ARM64 cross toolchain by itself when the MSVC ARM64 component is installed. If
you do hit that error, the ARM64 C++ workload is what's missing; the fallback is a
matching Native Tools prompt.

⚠️ The script does **not** stage WinFsp, so `dist/` binaries cannot *mount* until
WinFsp is installed on the target machine (see "P2 — WinFsp installer bundling" in
the roadmap). Everything else works without it.

Note this is **cross-compilation from an x64 host**, which is a different question
from *building natively on the ARM64 laptop* — see the `winfsp-sys` registry hazard
above, which still stands and applies only to the native case.

### Single target

```powershell
cargo build --release --target aarch64-pc-windows-msvc
```

Cross-compiling ARM64 from x64 needs the **MSVC v143 ARM64 build tools** and an
environment where ARM64 `cl.exe` is on PATH (ARM64 Native Tools prompt, or
`ilammy/msvc-dev-cmd -arch arm64`).

---

## Sector size: the real ARM64 test risk

The engine reads the device's logical sector size
(`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` → `get_sector_size`, `phoenix-core/src/disk.rs`)
and the `.phnx` format was designed for 4Kn: extents are addressed in a fixed
512-byte *format* unit that is deliberately decoupled from device geometry, and
the disk's true sector size is recorded in the manifest's `disk.sector_size`.

**As of 2026-07-13 the engine honors it end-to-end.** A 4Kn audit found the engine
counted a 4096-byte disk in 512-byte units in six places — capture died on the very
first read, and restore reserved the backup GPT, computed `StartingUsableOffset`,
sized `FSCTL_EXTEND_VOLUME`, and applied NTFS MFT fixups all in the wrong unit.
**Mounting** a 4Kn backup works too: the mount synthesizes a **VHDX** (which can state
its logical sector size) instead of a fixed VHD (which cannot), and Windows attaches it
as a genuine 4096-byte-sector disk. Covered by `ntfs_4kn_backup_restore_roundtrip` and
`winfsp_mount_a_4kn_backup`.

**Crucially, all of it was found and fixed on the x64 dev box**, against a 4Kn VHDX we
synthesize ourselves. The ARM machine was never required — so the laptop's UFS drive is
no longer a risk to the plan, just a data point. **4Kn is not a known gap any more**,
which means a 4Kn failure on that machine is a *finding*, not an expected limitation.

Note that many UFS/NVMe devices are **512e**: 4096-byte *physical* sectors with
512-byte *logical* sectors. 512e is the common case and is fully supported today —
only `BytesPerSector = 4096` (true 4Kn) trips the bugs. Do not assume; measure.

## Low-memory (4 GB) notes

Memory is bounded by design, so 4 GB is workable:

- The parallel chunk pipeline caps in-flight buffers at roughly **128 MiB** at its
  default `min(cores, 8)` workers (`phoenix-core/src/pipeline.rs`).
- Lower it with `PHOENIX_WORKERS` (clamped 1..=64) if the machine thrashes:
  `$env:PHOENIX_WORKERS = "2"`.
- Keep T3 test partitions small (a few GB) — the harness supports this, and
  `PHOENIX_T3_LAYOUT_GB` caps restore grow-to-fill.
- The non-winfsp mount fallback materializes a full-size temp VHD and will likely
  exhaust the disk. Do not use it here.

---

## ARM64 hardware test plan

> **Executing this on the ARM64 machine? Use
> [ARM64-BRINGUP.md](ARM64-BRINGUP.md) instead.** It is the same plan written as a
> self-contained runbook — exact commands, pass criteria, the known traps, and a
> reporting template — and it is what to hand to an agent sitting on that box. The
> summary below stays here for readers who want the plan next to the parity
> contract that motivates it.

Run everything from an **elevated** shell. Each phase gates the next: do not skip
ahead, because a failure in phase 0 or 1 invalidates everything after it.

### Phase 0 — Characterize the machine (do this FIRST, before any build)

The single most important measurement. It decides whether the rest of the plan is
even runnable.

```powershell
# Logical vs physical sector size for every disk. LOGICAL is what matters.
Get-Disk | Select-Object Number, FriendlyName, BusType,
    LogicalSectorSize, PhysicalSectorSize, PartitionStyle, Size

# Cross-check (fsutil reports the volume's view):
fsutil fsinfo ntfsinfo C:
```
Record the results. Then:

- **`LogicalSectorSize = 512`** (512e — the likely case): the 4Kn bugs do not
  fire. Proceed with the whole plan; you are testing *ARM64*, not 4Kn.
- **`LogicalSectorSize = 4096`** (true 4Kn): **fully supported — backup, restore,
  and mount** (fixed 2026-07-13; covered by `ntfs_4kn_backup_restore_roundtrip` and
  `winfsp_mount_a_4kn_backup`, both against a 4Kn VHDX we synthesize ourselves).
  Run the whole plan, phase 5 included. Nothing about 4Kn is a known gap any more,
  so a failure here is a **finding**, not an expected limitation — capture it.

Also record: core count, RAM, free disk space, and whether BitLocker is on
(`Get-BitLockerVolume`) — a Snapdragon laptop very likely has it enabled on C:.

### Phase 1 — Build natively on the ARM64 machine

Expect the `winfsp-sys` registry panic (see "The WinFsp problem"). Resolving it is
itself a test result worth recording.

```powershell
cargo build --release                    # exercises the winfsp default feature
cargo test -p phoenix-core -p phoenix-capture -p phoenix-restore `
           -p phoenix-clone -p phoenix-mount -p phoenix-vss     # T1, no admin
```
T1 is pure logic and should be **114/114 green** — identical to x64. Any T1 delta
is a genuine arch bug (endianness, struct layout, integer width) and is the
highest-value thing this whole exercise could find.

Note: `cargo test --workspace` **fails** with error 740 (elevation) because the CLI
and GUI test binaries carry a `requireAdministrator` manifest. That is expected on
both arches; use the crate list above.

### Phase 2 — Read-only smoke (non-destructive)

Nothing here writes to a disk.

```powershell
.\target\release\carbon-phoenix.exe list-disks
```
Confirm the UFS drive enumerates, and that its **sector size, bus type, and model**
are reported correctly — this is the first real exercise of `get_sector_size` and
the drive-type/USB-bridge naming on ARM64.

Then launch the GUI and confirm it renders: `.\target\release\carbon-phoenix-gui.exe`
(UAC prompt expected). This is the first-ever run of **glow/OpenGL on ARM64** — the
plausible failure is the GL context, not our code. Check the sidebar, the disk
cards, the theme toggle, and the sticky action bar.

### Phase 3 — Backup on a small external USB stick (destructive to the stick only)

Do **not** point T3 at the internal UFS drive — the safety gate refuses the
boot/system disk anyway. Use a small USB stick.

```powershell
$env:PHOENIX_T3_DISK   = "<n>"
$env:PHOENIX_T3_SERIAL = "<serial>"
cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
```
Keep partitions small (a few GB) for the 4 GB / low-power budget. This runs the MBR
NTFS/FAT32/exFAT/shrink/BitLocker/VSS matrix that is already green on x64 — so any
failure isolates cleanly to ARM64.

If the machine struggles: `$env:PHOENIX_WORKERS = "2"`.

### Phase 4 — VSS on ARM64 (the highest-risk *runtime* path)

Covered by `real_vss_backup_roundtrip` in phase 3, but worth an explicit manual
run, because this is the path most likely to behave differently: it depends on
PowerShell + WMI, which are slower to start on this class of machine.

- Backing up an **idle** partition must take the lock arm — log:
  `FSCTL_LOCK_VOLUME acquired`.
- Backing up a **busy/system** volume must escalate — logs: `volume is in use;
  reading a VSS shadow instead of the live volume`, then `VSS snapshot created`.
- A volume that can be frozen **neither** way must **abort**, not smear a live read.

Watch for timeouts specifically. If the lock/VSS escalation is racing PowerShell
startup on a slow machine, that is a real (and fixable) ARM-relevant finding.

### Phase 5 — Mount (WinFsp on ARM64)

The first-ever run of `winfsp-a64.dll`.

```powershell
.\target\release\carbon-phoenix.exe mount <backup.phnx> --partitions 2
```
Confirm: the drive letter appears, files read back correctly, **disk usage does not
grow** (the whole point of the zero-space mount), and the letter disappears on
unmount.

### Phase 6 — Restore + GUI end-to-end

Restore the phase-3 backup back to the USB stick via the GUI, exercising the
layout editor (drag/resize) on ARM64, and confirm the fixture and `chkdsk`.

### What to report back

For each phase: pass/fail, the exact error text on failure, and the throughput
lines (the engine logs elapsed + MB/s; `target/perf-log.csv` collects one row per
operation). Real ARM64 throughput numbers on UFS are genuinely new data — the
pipeline's worker defaults were tuned on an x64 desktop.

---

## Maintainer notes

- **GPT disk GUID** is read via `DRIVE_LAYOUT_INFORMATION_EX.Anonymous.Gpt.DiskId`
  (a typed `windows-sys` field), not a manual byte offset — correct on all 64-bit
  Windows ABIs.
- **Arch-sensitive deps are `winfsp` + `winfsp-sys` and nothing else.**
  `windows-sys`, `blake3`, `zstd`, `eframe`, and `rfd` all publish
  `aarch64-pc-windows-msvc` artifacts and are not arch-pinned.
- **Resource embedding works on aarch64.** `winres` embeds
  `windows/admin.manifest` + the app icon; it dispatches on `CARGO_CFG_TARGET_ENV`
  (`msvc` for both) and `rc.exe` output is machine-neutral. On an ARM64 *host* it
  selects the x86 `rc.exe` and runs it under emulation — works, but not a native
  path. (`winres` 0.1.12 is unmaintained; `winresource` is the maintained fork.)
- Several build.rs comments still say "the **winfsp-x64.dll** delay-load link arg"
  (`phoenix-gui/build.rs`, `phoenix-cli/build.rs`, `phoenix-systests/build.rs`).
  The call they wrap is arch-generic; only the comments are wrong.
  `phoenix-mount/build.rs` gets it right.
- Do not add `target_arch`-gated behavior without updating this document and the
  parity table; prefer fixes that apply to both targets.
