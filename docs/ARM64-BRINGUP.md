# ARM64 bring-up runbook

**Read this on the ARM64 machine. It is written to be executed, not skimmed.**

This document was written when Phoenix Simulacra had never been *run* on ARM64. That
is no longer true, and the phases below now carry their results inline.

> ### Where it stands (2026-07-14)
>
> **Backup and mount work on ARM64, end to end, on true 4Kn UFS hardware.** The
> GUI renders (wgpu/DX12), the live system disk images correctly (VSS escalation
> on C:, `FSCTL_LOCK_VOLUME` on Recovery, used-block capture off a 4Kn bitmap),
> and the result mounts through `winfsp-a64.sys` with drive letters and **no
> space blow-up** — the zero-space path, not the materialize fallback. T1 is
> 103/103 natively. VSS works on a Home SKU.
>
> It cost **three real bugs**, all of which only this hardware could have found:
> the `PhysicalDriveN` scan reporting UFS **firmware LUNs** as disks (and
> offering them as clone/restore *targets* — writing one bricks the machine); a
> **512-byte boot-sector read that is illegal on 4Kn**, which had silently killed
> both no-drive-letter FS detection and all BitLocker volume-view detection; and
> an unreadable error when the backup **destination** fell off the USB bus.
>
> **Still unrun here: restore, clone, T2, T3, and BitLocker** (Home SKU can't
> build the fixture). Those phases below are still a plan, not a record.

Companion documents, for the *why* behind the rules here:

- [WINDOWS-ARM64.md](WINDOWS-ARM64.md) — the parity contract, and the WinFsp
  arch problem in detail.
- [TESTING.md](TESTING.md) — the full test-tier model (T1/T2/T3) this runbook
  drives.

---

## If you are the agent on the ARM64 box, read this first

You are being handed a machine nobody has run this software on. Your job is to
**find out what is true**, not to produce a green checklist.

**This machine is a test target, not a dev box.** Binaries — app *and* tests — are
cross-built on the x64 machine and copied here (Phase 1). Do not sink hours into
standing up a native Rust/MSVC/LLVM toolchain unless someone explicitly asks for
it; that is not where the value is.

1. **A failure is a result.** Do not "fix" a failing test by relaxing it,
   skipping it, or rerunning until it passes. Capture the exact error text and
   report it. An ARM64-only failure is the single most valuable thing this
   exercise can produce — it is the entire reason for the exercise.
2. **Do not rationalize.** "Probably just a flaky test" is a claim that needs
   evidence. There is exactly **one** known-flaky test in the tree (named in
   Phase 3); everything else that goes red is a finding until proven otherwise.
3. **Phases gate each other.** A failure in Phase 0 or 1 invalidates every
   phase after it. Do not skip ahead to the interesting parts.
4. **Never point a destructive test at the internal drive.** The T3 safety gate
   refuses the boot/system disk, but do not rely on that as your only guard.
   Destructive tests target a **throwaway USB stick**, and nothing else.
5. **Record numbers, not impressions.** Throughput on ARM64/UFS is genuinely new
   data — the pipeline's worker defaults were tuned on an x64 desktop. "It felt
   slow" is worthless; `target/perf-log.csv` is not.
6. **There is no CI.** Nothing validates this but you and the machine in front
   of you. Do not write "runs in CI" into any note or doc.

---

## Phase 0 — Characterize the machine (BEFORE you install or build anything)

This is the most important measurement in the document. It decides which of the
later phases are even meaningful.

```powershell
# LOGICAL sector size is the one that matters. Physical is a red herring.
Get-Disk | Select-Object Number, FriendlyName, BusType,
    LogicalSectorSize, PhysicalSectorSize, PartitionStyle, Size

fsutil fsinfo ntfsinfo C:          # cross-check: the volume's view

Get-BitLockerVolume                # a Snapdragon laptop very likely has C: encrypted
Get-ComputerInfo -Property CsProcessors, CsTotalPhysicalMemory, OsArchitecture
Get-Volume | Select-Object DriveLetter, FileSystem, SizeRemaining, Size
```

**Record all of it in your report.** Then read the sector size:

| `LogicalSectorSize` | Meaning | What it changes |
|---|---|---|
| **512** | 512e (4096 physical / 512 logical). The common case, including most UFS and NVMe. | Nothing. Fully supported. You are testing *ARM64*, not 4Kn. |
| **4096** | True 4Kn. | Also fully supported (fixed 2026-07-13, covered by `ntfs_4kn_backup_restore_roundtrip` and `winfsp_mount_a_4kn_backup`). Run the whole plan. **A 4Kn failure here is a finding, not a known limitation** — the entire 4Kn surface was reproduced and fixed on the x64 box against a synthesized 4Kn VHDX. |

Do not assume 4Kn because the drive is UFS. **Measure.**

### Measured, 2026-07-14 — the reference machine

> **This machine is TRUE 4Kn.** Not 512e. Every 4Kn code path that until now had
> only ever run against a *synthesized* VHDX is, on this box, the ordinary case.
>
> ```text
> Disk 0   SAMSUNG KM5V8001DM-B622   BusType UFS   GPT   127,758,499,840 bytes
>          LogicalSectorSize  4096      PhysicalSectorSize 4096
> fsutil C:  Bytes Per Sector 4096   Bytes Per Cluster 4096
>
> CPU      Snapdragon (TM) 7c Gen 2 @ 2.55 GHz — 8 cores / 8 logical
> RAM      3.68 GB              OS   Windows 10 **Home**, ARM 64-bit
> BitLocker  C: FullyDecrypted, protection Off  (no encrypted volume on this box)
> Volumes  C: NTFS 117.8 GB, 68 GB free
> ```
>
> Consequences, all of which bind the phases below:
>
> - **4Kn is live.** Treat any 4Kn failure as a first-class finding.
> - **3.68 GB of RAM.** The default worker count is `min(cores, 8)` = **8** here,
>   which targets ~128 MiB of in-flight buffers on a machine with under 4 GB.
>   Expect to need `$env:PHOENIX_WORKERS = "2"`.
> - **Home SKU.** See the note in Phase 5 on `bitlocker.rs` — it is a *test
>   fixture* limitation, **not** a product limitation. Nothing in the shipping
>   product is gated on a Pro/Enterprise SKU, and VSS is confirmed working here
>   (Phase 6).

---

## Phase 1 — Toolchain

> **Default lane: DON'T build on the ARM64 box.** It is a **test target, not a dev
> box** — low-powered, and a native build needs the whole stack below plus a
> `winfsp-sys` registry workaround. Cross-build everything on the x64 machine and
> copy the binaries over. See "Cross-build lane" immediately below; do Phase 2
> (native build) only if you actually need it.

### Cross-build lane (preferred)

Both the app **and the tests** cross-compile. Run these on the **x64** box:

```powershell
# App binaries
cargo build --release --target aarch64-pc-windows-msvc

# Test binaries — compiles them WITHOUT running; prints each exe's path
cargo test --no-run --target aarch64-pc-windows-msvc `
    -p phoenix-core -p phoenix-capture -p phoenix-restore `
    -p phoenix-clone -p phoenix-mount -p phoenix-vss          # T1
cargo test --no-run --target aarch64-pc-windows-msvc `
    -p phoenix-systests --features winfsp                     # T2/T3
```

Copy the resulting `.exe`s to the ARM64 machine and run them **directly** — they
are self-contained, so no Rust, no MSVC, no LLVM over there:

```powershell
.\phoenix_core-<hash>.exe                                   # T1: just run it
.\<systest>-<hash>.exe --ignored --test-threads=1 --nocapture   # T2/T3, ELEVATED
```

Cargo's usual flags still apply because they are the **test harness's** flags, not
cargo's: `--ignored`, `--test-threads=1`, `--nocapture`, and a filter substring all
work when passed straight to the exe. Env vars (`PHOENIX_T3_DISK`,
`PHOENIX_WORKERS`, …) are read at runtime, so set them in the ARM64 shell as usual.

Verified 2026-07-14: `cargo test --no-run --target aarch64-pc-windows-msvc` emits
ARM64 PE test executables (`Machine = 0xAA64`).

What the ARM64 box **still** needs, because these are *runtime*, not build-time:

- **Elevation** for T2/T3 (diskpart, raw disk handles).
- **WinFsp** if you are exercising mount — `winfsp-a64.dll` is delay-loaded, so
  the app starts without it and only *mounting* fails.

> **WinFsp on ARM64: the stock MSI is all you need. Verified 2026-07-14.**
>
> There is no separate "ARM64 WinFsp" to hunt for. The ordinary release MSI
> (tested: **2.1.25156**) is a single multi-arch package that installs the ARM64
> binaries *and registers the ARM64 kernel driver*:
>
> ```text
> C:\Program Files (x86)\WinFsp\bin\winfsp-a64.dll     202 KB   2.1.25156
> C:\Program Files (x86)\WinFsp\bin\winfsp-a64.sys     168 KB   2.1.25156
>
> Service  WinFsp+<sxs-stamp>   Type=2 (file system driver)  Start=3 (on demand)
>   ImagePath = ...\SxS\...\bin\winfsp-a64.sys        <-- the ARM64 driver
> Service  WinFsp.Launcher      launcher-a64.exe, running
> ```
>
> It is the **runtime** MSI, not the dev/SDK package. That is the right one: the
> SDK is only needed on the machine that *builds* (for `winfsp-sys`' bindgen and
> link libs), which under the cross-build lane is the x64 box.
>
> The runtime lookup resolves correctly on aarch64 — `winfsp-rs`'s
> `get_system_winfsp()` reads `SOFTWARE\WOW6432Node\WinFsp` **first** (falling
> back to the native key), then appends `bin\winfsp-a64.dll` under
> `cfg!(target_arch = "aarch64")`. Both halves match what the MSI actually wrote.
>
> Implication for shipping: **the stock MSI can be bundled with our installer as-is
> for ARM64.** No custom build, no arch-specific variant.

### Native-toolchain lane (only if you really need it)

Everything ARM64-native. Do not install x64 builds and hope emulation carries
them.

| Need | Why | Note |
|---|---|---|
| Rust (aarch64-pc-windows-msvc host) | Builds. | `rustup show` must report an **aarch64** host triple. |
| VS Build Tools, MSVC v143 ARM64 + Windows SDK | Links; `zstd-sys` needs `cl.exe`. | |
| **LLVM (ARM64)** | `libclang` for `winfsp-sys` bindgen. | Set `$env:LIBCLANG_PATH` if not on PATH. |
| **WinFsp (ARM64 MSI)** | The `winfsp` feature is a **default feature**. Needed at build *and* run time. | https://winfsp.dev |
| Git | `build.rs` stamps provenance into the binary. | Without it the banner says `unknown`, which is survivable. |

> **Already installed on the reference Snapdragon box (2026-07-14).** Don't redo
> this. Present and working:
>
> - **rustup 1.97.0**, host `aarch64-pc-windows-msvc` (native, not emulated).
> - **VS Build Tools, MSVC 14.44.35207** with the native `Hostarm64\arm64` toolset
>   and **Windows 11 SDK 22621**.
>
> Cargo needs the MSVC environment, so prefix it:
>
> ```powershell
> & "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" arm64
> cargo test -p phoenix-core   # etc.
> ```
>
> **Not** installed: **LLVM/libclang**. `winfsp-sys` really does run bindgen —
> its `lib.rs` is `include!(concat!(env!("OUT_DIR"), "/bindings.rs"))`, and the
> `src/bindings.rs` it ships is `docsrs`-only — so `phoenix-mount`, `phoenix-gui`
> and `phoenix-systests --features winfsp` cannot be built natively here without
> it. Everything else builds and tests natively today, which is enough to iterate
> on a core-layer fix against real 4Kn hardware without a round trip to the x64
> box. It does **not** change the default lane: cross-building on x64 is still
> faster and still preferred.
>
> The **WinFsp MSI (2.1) is installed** — see Phase 1's WinFsp row. That is a
> *runtime* dependency and is all the ARM64 box needs to mount.

---

## Phase 2 — Build natively (SKIP THIS if you cross-built), and prove the binary is really ARM64

> Skip straight to "Prove it is native" below if you took the cross-build lane —
> that check still matters, and it is the only part of this phase that does.

### The failure this document used to tell you to expect — it does not happen

**This section was a prediction, and the prediction was wrong. Measured
2026-07-14; corrected here so nobody spends a day on it.**

`winfsp-sys-0.12.1/build.rs` does read **only**
`HKLM\SOFTWARE\WOW6432Node\WinFsp`, with a hard
`.expect("WinFsp installation directory not found.")` and no fallback to the
native `SOFTWARE\WinFsp` key. That part is true. What was *assumed* — that an
ARM64 installer would write the native key and so break the build — is false:

```text
HKLM\SOFTWARE\WOW6432Node\WinFsp   InstallDir = C:\Program Files (x86)\WinFsp\   ✅
HKLM\SOFTWARE\WinFsp               ABSENT                                        ❌
```

**The WinFsp MSI writes WOW6432Node on ARM64 too.** It is a single MSI carrying
every architecture, not an arch-specific installer, and it puts itself under
`Program Files (x86)` regardless. So the build script finds what it wants, and
**there is nothing to work around.** Do not mirror the registry key; there is no
native key to mirror *from*.

If a future WinFsp release *does* start writing the native key, the panic will
say `WinFsp installation directory not found.` — and then, and only then, this is
the mirror:

```powershell
Get-ItemProperty 'HKLM:\SOFTWARE\WinFsp'             -Name InstallDir -EA SilentlyContinue
Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Name InstallDir -EA SilentlyContinue

# ONLY if the native key exists and WOW6432Node does not:
New-Item -Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Name InstallDir `
  -Value (Get-ItemProperty 'HKLM:\SOFTWARE\WinFsp').InstallDir
```

⚠️ **Do not "solve" this with `--no-default-features`.** That drops the `winfsp`
feature, and without it `mount` falls back to materializing a **full-size temp
VHD** — the dev-only stopgap that doubles a backup's on-disk footprint and will
likely exhaust this machine's disk outright. It is a **degraded mode, not a
neutral one**. Use it to unblock a build if you must; never to evaluate mount.

### Build

```powershell
cargo build --release
```

### Prove it is native (not emulated x64)

There is **no `--version` flag**. The proof is the startup banner, which every
run emits at INFO by default:

```powershell
.\target\release\simulacra.exe list-disks
```

The `phoenix_build` banner must report:

```
target = "aarch64-pc-windows-msvc"   arch = "aarch64"   profile = "release"
```

If `arch` says `x86_64`, you are running an emulated binary and **every result
after this point is worthless**. Stop and fix the toolchain.

---

## Phase 3 — T1 unit tests (no admin, no disks)

Pure logic. This is where a genuine architecture bug — endianness, struct
layout, integer width — would surface, and it is the highest-value thing this
whole exercise could find.

```powershell
# Cross-build lane: just run the exes you copied over
.\phoenix_core-<hash>.exe
.\phoenix_capture-<hash>.exe        # …and the rest

# Native lane:
cargo test -p phoenix-core -p phoenix-capture -p phoenix-restore `
           -p phoenix-clone -p phoenix-mount -p phoenix-vss
```

**Pass criterion: 114/114 green, identical to x64** (summed across the binaries if
you are running them individually). Any delta is a real bug.
Report it with the failing test name and full output; do not proceed to
destructive phases until you understand it.

**Expected, not a bug:** `cargo test --workspace` fails with **os error 740**
("requires elevation") — the `phoenix-cli` and `phoenix-gui` test binaries carry
a `requireAdministrator` manifest, so cargo can't launch them from a normal
shell. This happens on x64 too. Use the crate list above.

**The one known-flaky test** (T2, Phase 5 — not this phase):
`vss.rs :: locked_backup_blocks_writers_for_duration` fails intermittently
(~1-in-3) because it races a writer thread against the capture window instead of
synchronizing to it. The lock itself is separately and deterministically proven
by `volume_lock_blocks_external_writes`. **Re-run it before believing it.** This
is the *only* test with a standing excuse. It may well be *worse* on a slow ARM
machine — if so, say so; that is useful.

---

## Phase 4 — Read-only smoke (nothing is written)

```powershell
.\target\release\simulacra.exe list-disks
```

Confirm the internal drive enumerates and that its **sector size, bus type, and
model** are right. This is the first real exercise of `get_sector_size` and the
drive-type/USB-bridge naming on ARM64. Cross-check against Phase 0's `Get-Disk`
output — **if `list-disks` and `Get-Disk` disagree about the sector size, stop.**
Everything downstream trusts that number.

> ### FINDING (2026-07-14, FIXED) — six disks on a one-disk machine
>
> The GUI's Select Source page listed **Disk 0 through Disk 5**. Only Disk 0 is
> real. `Get-Disk`, `Win32_DiskDrive`, and Disk Management each report exactly
> one disk.
>
> They were not phantoms. `\\.\PhysicalDrive1..5` **open successfully** and carry
> genuine GPTs full of genuine (tiny) partitions. They are the **UFS firmware
> LUNs**: same Samsung UFS chip, same SCSI port/path/target, differing only by
> **LUN 1–5** — 8 MB, 8 MB, 4 MB, 152 MB, 12 MB of Snapdragon boot/firmware
> partitions (`xbl`, `tz`, `hyp`, `abl`, `devcfg`, modem, …). Windows names them
> in the legacy `PhysicalDriveN` alias table but does not surface them as disks.
>
> `enumerate_disks()` brute-forced `\\.\PhysicalDrive0..63` and reported whatever
> opened. That namespace is **an alias table, not the list of disks.** So the
> firmware LUNs were offered as backup sources — and, because `enumerate_disks()`
> also feeds the clone and restore target pickers, **as write targets.** The
> boot/system-disk gate does not catch them: a firmware LUN is neither.
> **Writing one bricks the machine.**
>
> **Fix:** enumerate the `GUID_DEVINTERFACE_DISK` device-interface class — the
> question we actually mean, *what are the disks?* — and map each interface to
> its index with `IOCTL_STORAGE_GET_DEVICE_NUMBER`. That is the same source Disk
> Management, `Get-Disk`, and `Win32_DiskDrive` use. The firmware LUNs never
> register the interface, so they are excluded **by construction**, not by a
> size/name heuristic we would have to keep tuning. Verified on this box: the
> class returns Disk 0 alone, and an attached VHDX still enumerates (so the T2
> suite is unaffected).
>
> **This is not ARM-specific.** Any multi-LUN device — some USB card readers,
> eMMC/UFS phones in mass-storage mode — could have done this on x64. ARM64 is
> just where we finally pointed the tool at hardware that has one.

### The GUI — the renderer already bit us once

```powershell
.\target\release\simulacra-gui.exe          # UAC prompt expected: it requires admin
```

> **This phase has already produced TWO findings (2026-07-14), and it is the most
> productive phase in this document so far.**
>
> 1. The first launch died on `egui_glow requires opengl 2.0+`. **Windows on ARM
>    ships no desktop OpenGL driver** — `opengl32` is Microsoft's GDI software
>    rasterizer at OpenGL 1.1. The GUI moved from eframe's `glow` backend to
>    **`wgpu`**.
> 2. The first *wgpu* build then died on **`no suitable adapter found`**, having
>    tried Vulkan (no usable surface extensions on Adreno) and GLES (back to the
>    same GL 1.1) — but **never DX12, because DX12 wasn't compiled in.** wgpu 22
>    makes `dx12` an opt-in feature while Vulkan/GLES self-enable by target cfg,
>    and eframe asks for neither. `phoenix-gui` now depends on wgpu directly with
>    `features = ["dx12", "wgsl"]`.
>
> Both are fixed, and **the GUI now renders on ARM64** (observed 2026-07-14 — the
> first pixel this app has ever drawn on the architecture). Full story in
> [WINDOWS-ARM64.md](WINDOWS-ARM64.md) → "The OpenGL problem".
>
> **This phase is therefore CLEARED for the GUI.** Re-run it only to confirm your
> build renders; the open questions all live in the phases after this one.

**Read the error text, not just the exit.** These two crashes looked similar and
had completely different causes; the diagnosis lived in *which backends wgpu
enumerated before giving up*. If it fails again, capture every `wgpu_hal` line —
they name the backends tried, and that list is the finding.

If it renders, check: the sidebar, the disk cards, the theme toggle, and the
sticky action bar (the full-width gradient Start bar pinned above the status
bar). Jump straight to a page with `--page clone` / `--page mount` / etc., and
exercise the mounts table without a real mount using `--demo-mounts`.

If it *doesn't*, capture the error verbatim — and note that a wgpu failure will
name an adapter/backend (DX12, Vulkan) rather than a GL version. Do **not**
"solve" it by reinstating glow or shipping an OpenGL DLL (ANGLE is GL ES; Mesa
llvmpipe is software) — that trades a hard failure for a slow one on the machine
least able to afford it.

Take screenshots. Font rendering and DPI on ARM64 are worth an eyeball.

---

## Phase 5 — T2 virtual-disk system tests (elevated)

These attach real VHDX disks via diskpart and run the production engine against
genuine NTFS/FAT/exFAT volumes. **No risk to the host's real disks.** Elevated
shell required.

> ### ✅ T2 PASSES ON ARM64 (2026-07-14) — 11/12 binaries, 32 tests green
>
> First full T2 sweep on ARM64, staged, no hangs. On the 4Kn Snapdragon box:
>
> ```text
> resize_roundtrip  sector_4kn  backup_restore_roundtrip  clone  fat_family
> gpt_identity  partial_mbr  partial_clone  mount  vhdx_container  vss     all PASS
> bitlocker                                                                    FAIL
> ```
>
> - **`vss` went 6/6 — including `locked_backup_blocks_writers_for_duration`,**
>   the one known-flaky test, on the first run on a *slow* machine. The runbook
>   feared it would be worse here; it wasn't. (Re-run it a few times before
>   trusting that — one clean pass isn't proof a race is gone.)
> - **`vhdx_container` passed both arms** — Windows attached our synthesized 4Kn
>   *and* 512 VHDX and reported the right `LogicalSectorSize` for each.
> - **`sector_4kn` 5/5** — 4Kn backup/restore/clone, now on hardware that is 4Kn.
> - **`bitlocker` FAILED as expected**, and only because it is Home SKU:
>   `Enable-BitLocker` -> `0x8031005A "This version of Windows does not support
>   this feature"`. That is the *test* failing to build its fixture, not the
>   product. Read it as skipped-and-why. **BitLocker still has zero ARM64
>   coverage** as a result.
> - `winfsp_mount` was **not** in this run (needs `--features winfsp`, hence LLVM
>   on the box). The mount itself is already proven by hand — see Phase 8.
>
> Wall time was substantial (several minutes per binary; `partial_clone` ~363 s).
> That is a real low-power-ARM data point, not a problem.

### ⚠️ Run it STAGED. A single one-shot run has hard-hung Windows — twice.

**There is now a script for this: `scripts\run-system-tests-staged.ps1`** (one
cargo process per binary, a settle between, and a breadcrumb log). Prefer it. Run
`scripts\arm64-dev-env.ps1` first to put the native ARM64 MSVC toolchain on PATH
(`& vcvarsall.bat arm64` does **not** work from PowerShell — it sets the vars in a
child cmd that then exits). The manual loop below is what the script automates,
kept here so the mechanism is legible.

Not a specific test: **cumulative VHD attach/detach churn** inside one uptime
exhausts the storage stack (the crashes correlated with Windows handing out
`\Device\HarddiskVolume1065`). Every binary passes cleanly on its own after a
reboot. So do **not** use `scripts\run-system-tests.ps1` here — it is a single
cargo invocation. Drive one cargo process per test binary, with a settle between:

```powershell
# `vhdx_container` is a real T2 binary; the old list omitted it. `winfsp_mount`
# is left OUT here because it is #![cfg(feature = "winfsp")] — without
# --features winfsp it compiles to an empty binary and reports zero tests
# (misleading), and *with* it you need libclang on this box. Add it only when
# you have LLVM, via the script's -Winfsp switch.
foreach ($b in @('resize_roundtrip','sector_4kn','backup_restore_roundtrip','clone',
                 'fat_family','gpt_identity','partial_mbr','partial_clone','mount',
                 'vhdx_container','vss','bitlocker')) {
    Add-Content arm64-t2.log "START $b $(Get-Date -Format o)"
    cargo test -p phoenix-systests --test $b -- --ignored --test-threads=1
    Add-Content arm64-t2.log "END   $b exit=$LASTEXITCODE $(Get-Date -Format o)"
    Start-Sleep -Seconds 5   # VHD detach is asynchronous; let it finish
}
```

**Log the breadcrumb to a file, as above.** A hard hang loses whatever is sitting
in a stdout pipe; an append that already reached disk survives. That breadcrumb
is the only reason the x64 crash was ever localized.

`--test-threads=1` is mandatory: diskpart, disk-index discovery, and drive-letter
assignment are process-global and race in parallel.

Notes:
- **This machine is likely slower and lower-memory than the x64 box.** If it
  thrashes, throttle the pipeline: `$env:PHOENIX_WORKERS = "2"` (clamped 1..=64;
  in-flight buffers cap around 128 MiB at the default `min(cores, 8)`).
- **`bitlocker.rs` needs a Pro/Enterprise SKU — the *test* does, the *product*
  does not.** The test calls `Enable-BitLocker` to *create* an encrypted fixture
  volume to back up, and that cmdlet is Pro+. Phoenix Simulacra's BitLocker
  *handling* — detecting `-FVE-FS-`, choosing plaintext-vs-ciphertext capture —
  is pure sector inspection and is not gated on any SKU. **This machine is Home,
  so the test cannot run here.** Report it as `skipped-and-why`, not as a
  failure, and do not let it become a claim that BitLocker support needs Pro.
  It also means we currently have **no BitLocker coverage on ARM64** — say so.
- **Nothing in the shipping product is gated on a Windows SKU.** If you ever find
  a normal backup/restore/clone/mount path that needs Pro+, that is a bug, and a
  serious one — Home is a first-class target. (Audited 2026-07-14: every Pro+
  dependency in the tree lives in `phoenix-systests`, never in product code.)
- **`sector_4kn.rs` is not hardware-dependent** — it synthesizes a true 4Kn VHDX
  via `CreateVirtualDisk` (core Win32, every SKU). It should pass regardless of
  what Phase 0 said about the internal drive.
- **`winfsp_mount.rs` is the first-ever run of `winfsp-a64.dll`.** Everything
  *around* it is now confirmed on this box (2026-07-14): the ARM64 DLL and the
  ARM64 kernel driver are installed and registered by the stock MSI, and
  `winfsp-rs` resolves `bin\winfsp-a64.dll` from the key the MSI actually wrote —
  see the WinFsp box in Phase 1. So the pieces line up. **But lining up is not
  running.** Nobody has yet watched a `.phnx` mount through `winfsp-a64.sys` and
  handed Windows a working drive letter. This is a headline result either way.

---

## Phase 6 — VSS (the highest-risk *runtime* path on this machine)

VSS is **not** `vssadmin`. `phoenix-vss` shells out to **PowerShell** and calls
the CIM method `Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create`.
Both PowerShell and WMI are native on ARM64, so the mechanism is expected to
work — but it is **PowerShell-startup-latency sensitive**, and that latency is far
more noticeable on a low-powered ARM laptop than on a desktop.

> **Confirmed on this machine, 2026-07-14 (ARM64, Windows 10 Home).** The CIM
> call is the right one and it works:
>
> ```text
> Invoke-CimMethod Win32_ShadowCopy Create  -> ReturnValue 0
>   ShadowID     {38B6926F-...}
>   DeviceObject \\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1
>
> vssadmin create shadow /for=C:           -> "Error: Invalid command."
> ```
>
> The second line is the point: `vssadmin` does not even *list* `create shadow`
> as a supported command on a client SKU. That is the exact silent failure this
> module was rewritten to escape, and it reproduces here. **The snapshot
> mechanism itself is therefore proven on ARM64 + Home.** What remains open in
> this phase is *latency and the escalation arms*, not whether VSS works.

`vss.rs` in Phase 5 covers it, but watch the three arms explicitly, because the
escalation is not a user choice — the engine locks, escalates, or aborts:

- **Idle volume** → takes the lock arm. Log: `FSCTL_LOCK_VOLUME acquired`.
- **Busy/system volume** → must escalate. Logs: `volume is in use; reading a VSS
  shadow instead of the live volume`, then `VSS snapshot created`.
- **Freezable neither way** (e.g. FAT32 with a held handle — VSS is NTFS-only) →
  must **abort**, not smear a live read.

**Watch for timeouts specifically.** If the lock/VSS escalation is racing
PowerShell startup on a slow machine, that is a real, fixable, ARM-relevant
finding — and exactly the kind of thing only this machine can teach us.

---

## Phase 7 — T3 destructive tests on a real USB stick

**Destroys the target disk.** Use a throwaway USB stick. Not the internal drive —
the gate refuses boot/system disks, but that is a backstop, not your plan.

```powershell
Get-Disk    # identify the stick: number, and confirm it is Removable

$env:PHOENIX_T3_DISK   = "<n>"          # the disk number to WIPE
$env:PHOENIX_T3_SERIAL = "<serial>"     # optional exact-device pin — use it
cargo test -p phoenix-systests --test real_disk -- --ignored --test-threads=1 --nocapture
```

Without `PHOENIX_T3_DISK` the tests skip cleanly and nothing is wiped.

This is the MBR NTFS/FAT32/exFAT/shrink/BitLocker/VSS matrix — already green on
x64, so **any failure isolates cleanly to ARM64**. Real fragmented NTFS on real
flash is what caught the `div_ceil` phantom-cluster silent-data-loss bug that
aligned VHD fixtures could not.

Keep partitions small (a few GB) for this machine's memory and disk budget;
`PHOENIX_T3_LAYOUT_GB` caps restore grow-to-fill. Throttle with
`$env:PHOENIX_WORKERS = "2"` if needed.

The GPT scenario (`real_gpt_multifs_roundtrip`) needs a **fixed** disk — Windows
won't make removable media GPT — and skips cleanly on a USB target. It requires a
stricter opt-in (`PHOENIX_T3_ALLOW_FIXED=1` **plus** a mandatory serial pin). On
this machine, **skip it**: the only fixed disk is the one you are booting from.

---

## Phase 8 — Mount, and restore end-to-end through the GUI

**Mount** (the zero-space WinFsp path — the whole point of the feature):

```powershell
.\target\release\simulacra.exe mount <backup.phnx> --partitions 2
```

Confirm: the drive letter appears, files read back correctly, **free disk space
does not drop by the size of the backup** (if it does, you are on the degraded
non-winfsp fallback — see Phase 2), and the letter disappears on unmount.

> ### ✅ MOUNT PASSES ON ARM64 (2026-07-14) — the first run of `winfsp-a64.sys`, ever
>
> A GUI backup of the live 4Kn system disk, mounted through WinFsp:
>
> - **All partitions got drive letters.**
> - **Zero-space confirmed by arithmetic, not by eyeball.** Destination: 238.4 GB
>   total, image 31.31 GB, 207.0 GB free afterwards — the `.phnx` is the *only*
>   thing consuming space. The materialize fallback would have cost ~119 GB more.
> - **Clean unmount**: no stray drive letters, no lingering attached virtual disk.
> - **No leaked VSS shadows** — `VssSession::Drop` did its job.
> - The driver is real and it ran: service `WinFsp+<sxs>` → `State: Running`,
>   `ImagePath = ...\bin\winfsp-a64.sys`.
>
> The backup that fed it is itself a result: **61 GB used → a 31.3 GB image**,
> with VSS escalation on C:, `FSCTL_LOCK_VOLUME` on Recovery, and unfrozen +
> verify-image on the un-lettered ESP. All three freeze arms, correct, first try.
>
> **Restore is still unrun.** Mount proves we can *read* a `.phnx` back on ARM64;
> it does not prove we can write one back to a disk.

**Restore**: take the Phase 7 backup back onto the USB stick **through the GUI**,
exercising the layout editor (drag to resize a partition), then confirm the
fixture and run `chkdsk`. This exercises the restore path and the ARM64 GUI's
pointer math in one shot.

---

## What to report back

Per phase: **pass / fail / skipped-and-why**, plus the **exact error text** on any
failure. Then, specifically:

Items 1, 2, 4 and 5 below are **answered** — recorded in the phases above, and
kept here only so a future run on *different* ARM64 hardware knows to re-check
them. The open ones are **3** (on a machine that can build `phoenix-mount`) and
**6**, plus everything in Phases 5–8 that is still marked unrun.

1. ~~**The Phase 0 machine characterization.**~~ Done: true 4Kn, UFS, Snapdragon
   7c Gen 2, 3.68 GB RAM, Windows 10 **Home**. See Phase 0.
2. ~~**Which WinFsp registry key the ARM64 MSI wrote.**~~ `WOW6432Node` — same as
   x64. The predicted build panic does not happen. See Phase 2.
3. **T1 count** — anything other than 114/114 is the headline. *(103/103 observed
   natively; `phoenix-mount`'s 11 need LLVM on the box, or a cross-built exe.)*
4. ~~**Whether the GUI renders**~~ — yes, wgpu/DX12.
5. ~~**Whether `winfsp-a64.dll` mounts**~~ — **yes.** Zero-space, drive letters,
   clean unmount. See Phase 8.
6. **Throughput numbers.** Every `ConsoleProgress`-wrapped operation prints
   elapsed + MB/s, and one CSV row per operation lands in `target/perf-log.csv`
   (`timestamp,label,elapsed_s,bytes,mb_per_s,workers`; override with
   `PHOENIX_PERF_LOG`, and note `cargo clean` wipes the default). **Send the CSV.**
   Real ARM64-on-UFS numbers are new data — the worker defaults were tuned on an
   x64 desktop, and this is the machine that can tell us they're wrong.

[WINDOWS-ARM64.md](WINDOWS-ARM64.md)'s "the engine remains unrun there" caveat is
**gone** as of 2026-07-14 — backup and mount are observed, and that document now
carries the evidence table. It is *not* fully retired: restore, clone, T2, T3 and
BitLocker are still expectation rather than observation, and both documents say
so. Delete a caveat only when the thing it warns about has actually been watched
working. Deleting the rest early would be the single easiest way to throw away
everything this exercise bought.
