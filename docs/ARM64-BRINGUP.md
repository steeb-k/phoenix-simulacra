# ARM64 bring-up runbook

**Read this on the ARM64 machine. It is written to be executed, not skimmed.**

Carbon Phoenix has never been *run* on ARM64 hardware. It has been *built* for
ARM64 (cross-compiled from x64, PE `Machine = 0xAA64`), and every unit test
passes on x64. That is all we know. This document is the plan to turn "it
compiles" into "it works" — or to find out where it doesn't.

Companion documents, for the *why* behind the rules here:

- [WINDOWS-ARM64.md](WINDOWS-ARM64.md) — the parity contract, and the WinFsp
  arch problem in detail.
- [TESTING.md](TESTING.md) — the full test-tier model (T1/T2/T3) this runbook
  drives.

---

## If you are the agent on the ARM64 box, read this first

You are being handed a machine nobody has run this software on. Your job is to
**find out what is true**, not to produce a green checklist.

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

---

## Phase 1 — Toolchain

Everything ARM64-native. Do not install x64 builds and hope emulation carries
them.

| Need | Why | Note |
|---|---|---|
| Rust (aarch64-pc-windows-msvc host) | Builds. | `rustup show` must report an **aarch64** host triple. |
| VS Build Tools, MSVC v143 ARM64 + Windows SDK | Links; `zstd-sys` needs `cl.exe`. | |
| **LLVM (ARM64)** | `libclang` for `winfsp-sys` bindgen. | Set `$env:LIBCLANG_PATH` if not on PATH. |
| **WinFsp (ARM64 MSI)** | The `winfsp` feature is a **default feature**. Needed at build *and* run time. | https://winfsp.dev |
| Git | `build.rs` stamps provenance into the binary. | Without it the banner says `unknown`, which is survivable. |

---

## Phase 2 — Build natively, and prove the binary is really ARM64

### The failure you should expect first

`winfsp-sys-0.12.1/build.rs` reads **only** `HKLM\SOFTWARE\WOW6432Node\WinFsp`
and `.expect("WinFsp installation directory not found.")`. It has **no fallback**
to the native `SOFTWARE\WinFsp` key — the key an ARM64-native installer is
likely to write. Cross-compiling from x64 never hits this (the x64 MSI populates
WOW6432Node). **Building natively on ARM64 probably does.**

Resolving it is itself a test result. Record which key the ARM64 MSI actually
wrote:

```powershell
Get-ItemProperty 'HKLM:\SOFTWARE\WinFsp'             -Name InstallDir -EA SilentlyContinue
Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -Name InstallDir -EA SilentlyContinue

# If only the native key exists, mirror it so winfsp-sys' build.rs finds it:
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
.\target\release\carbon-phoenix.exe list-disks
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
cargo test -p phoenix-core -p phoenix-capture -p phoenix-restore `
           -p phoenix-clone -p phoenix-mount -p phoenix-vss
```

**Pass criterion: 111/111 green, identical to x64.** Any delta is a real bug.
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
.\target\release\carbon-phoenix.exe list-disks
```

Confirm the internal drive enumerates and that its **sector size, bus type, and
model** are right. This is the first real exercise of `get_sector_size` and the
drive-type/USB-bridge naming on ARM64. Cross-check against Phase 0's `Get-Disk`
output — **if `list-disks` and `Get-Disk` disagree about the sector size, stop.**
Everything downstream trusts that number.

### The GUI — the renderer already bit us once

```powershell
.\target\release\carbon-phoenix-gui.exe          # UAC prompt expected: it requires admin
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
> Both are fixed; full story in [WINDOWS-ARM64.md](WINDOWS-ARM64.md) → "The OpenGL
> problem". **x64 renders. ARM64 render is still unwitnessed** — that is what this
> phase now exists to answer.

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

### ⚠️ Run it STAGED. A single one-shot run has hard-hung Windows — twice.

Not a specific test: **cumulative VHD attach/detach churn** inside one uptime
exhausts the storage stack (the crashes correlated with Windows handing out
`\Device\HarddiskVolume1065`). Every binary passes cleanly on its own after a
reboot. So do **not** use `scripts\run-system-tests.ps1` here — it is a single
cargo invocation. Drive one cargo process per test binary, with a settle between:

```powershell
foreach ($b in @('resize_roundtrip','sector_4kn','backup_restore_roundtrip','clone',
                 'fat_family','gpt_identity','partial_mbr','partial_clone','mount','vss',
                 'bitlocker','winfsp_mount')) {
    Add-Content arm64-t2.log "START $b $(Get-Date -Format o)"
    cargo test -p phoenix-systests --features winfsp --test $b -- --ignored --test-threads=1
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
- **`bitlocker.rs` needs a Pro/Enterprise SKU** (the BitLocker cmdlets). It uses a
  password protector on a data volume — no TPM or policy change needed. On Home,
  it can't run; say so rather than reporting it as a failure.
- **`sector_4kn.rs` is not hardware-dependent** — it synthesizes a true 4Kn VHDX
  via `CreateVirtualDisk` (core Win32, every SKU). It should pass regardless of
  what Phase 0 said about the internal drive.
- **`winfsp_mount.rs` is the first-ever run of `winfsp-a64.dll`.** The crate's
  runtime DLL selection *is* ARM64-correct (it resolves via the registry and
  picks `winfsp-a64.dll` on aarch64, with a documented fallback for exactly the
  native-key case that bites the build script). Expected to work — but expected
  is not observed. This is a headline result either way.

---

## Phase 6 — VSS (the highest-risk *runtime* path on this machine)

VSS is **not** `vssadmin`. `phoenix-vss` shells out to **PowerShell** and calls
the CIM method `Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create`.
Both PowerShell and WMI are native on ARM64, so the mechanism is expected to
work — but it is **PowerShell-startup-latency sensitive**, and that latency is far
more noticeable on a low-powered ARM laptop than on a desktop.

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
.\target\release\carbon-phoenix.exe mount <backup.phnx> --partitions 2
```

Confirm: the drive letter appears, files read back correctly, **free disk space
does not drop by the size of the backup** (if it does, you are on the degraded
non-winfsp fallback — see Phase 2), and the letter disappears on unmount.

**Restore**: take the Phase 7 backup back onto the USB stick **through the GUI**,
exercising the layout editor (drag to resize a partition), then confirm the
fixture and run `chkdsk`. This exercises the restore path and the ARM64 GUI's
pointer math in one shot.

---

## What to report back

Per phase: **pass / fail / skipped-and-why**, plus the **exact error text** on any
failure. Then, specifically:

1. **The Phase 0 machine characterization**, verbatim. Especially
   `LogicalSectorSize`.
2. **Which WinFsp registry key the ARM64 MSI wrote**, and whether the build
   panicked before you mirrored it.
3. **T1 count** — anything other than 111/111 is the headline.
4. **Whether the GUI renders** on the wgpu/DX12 backend, with screenshots. (The
   glow/OpenGL backend is already known dead here — that finding is closed.)
5. **Whether `winfsp-a64.dll` mounts** — first run ever.
6. **Throughput numbers.** Every `ConsoleProgress`-wrapped operation prints
   elapsed + MB/s, and one CSV row per operation lands in `target/perf-log.csv`
   (`timestamp,label,elapsed_s,bytes,mb_per_s,workers`; override with
   `PHOENIX_PERF_LOG`, and note `cargo clean` wipes the default). **Send the CSV.**
   Real ARM64-on-UFS numbers are new data — the worker defaults were tuned on an
   x64 desktop, and this is the machine that can tell us they're wrong.

If everything passes, say so plainly — and then
[WINDOWS-ARM64.md](WINDOWS-ARM64.md) and [TESTING.md](TESTING.md) both need their
"never run on ARM64" caveats deleted, because they will no longer be true.
