# Installer

Builds `Simulacra-Setup-<ver>.exe` — the single Windows installer for Phoenix
Simulacra. It lays down the dual-arch bundle (x64 + ARM64 binaries and the
arch-selecting launcher), creates Start Menu entries and an optional desktop
icon, silently installs the bundled **WinFsp** driver so mounting backups works
out of the box, and lays down a private copy of **QEMU** so booting backups as
VMs does too.

## Build

```powershell
# One-time: install the compiler (6.6.0+ required)
winget install JRSoftware.InnoSetup

# Build bundle + fetch/verify WinFsp MSI + compile installer
pwsh scripts\build-installer.ps1
# -> dist\Simulacra-Setup-<ver>.exe
```

`build-installer.ps1` runs `build-release.ps1` first, then stages the two
third-party payloads under `installer\build\` — the WinFsp MSI and the curated
QEMU tree — each downloaded once and verified against a pinned SHA-256 (neither
is committed). It then reads the version from the workspace `Cargo.toml` and
compiles `simulacra.iss`.

**Inno Setup 6.6.0+ is required** — 6.6.0 for the wizard's native dark mode
(`WizardStyle=modern dynamic`, which follows the Windows light/dark setting) and
6.5.2 for the PNG wizard image (`assets\wizardImage.png`, 410x797).

## What it installs

- `%ProgramFiles%\Phoenix Simulacra\` — the five bundle exes.
- Start Menu: **Phoenix Simulacra** and **Phoenix Simulacra (Debug)** (the debug
  entry passes `--debug` to enable the runtime console), plus an uninstall entry.
- Optional desktop icon (checked by default) -> `simulacra-launcher.exe`.
- **WinFsp** (checked by default; hidden if already installed) via
  `msiexec /i winfsp.msi /qn`. On ARM64 the installer mirrors WinFsp's
  `InstallDir` into the `WOW6432Node` registry view so the app can locate the
  DLL (winfsp-sys reads that literal path).
- **QEMU** (checked by default; x64 only) into `%ProgramFiles%\Phoenix
  Simulacra\qemu\` — a **private** copy, deliberately not `Program Files\qemu`,
  so a QEMU the user installed themselves is never touched, overwritten or
  version-clashed with. The app looks there first and the location is
  changeable in its UI. Removed with the app on uninstall, since Inno tracks
  the files it laid down; no registry marker is needed.

A marker (`HKLM\Software\Phoenix Simulacra\InstalledWinFsp`) records whether the
installer installed WinFsp, so the uninstaller only offers to remove it when we
put it there (never a WinFsp shared with rclone / sshfs-win).

## The QEMU payload

Upstream ships one installer carrying 55 system emulators and firmware for all
of them — 1.17 GB installed. We use exactly one target, so
`scripts\build-qemu-payload.ps1` extracts the upstream installer and prunes it
to **223 MB** (84 MB compressed). NSIS silent installs take default component
selection and QEMU's script exposes no component flags, so pruning is the only
way to avoid shipping a gigabyte of emulators that never run.

That script is **not** part of a release build. It is run by hand when moving to
a new QEMU build, and its output is uploaded to the binaries repo;
`build-installer.ps1` then downloads that zip against a pinned SHA-256. A
release build therefore never depends on the upstream URL still resolving.

Because pruning is our decision rather than upstream's, the script verifies what
it produced: the emulator reports the expected version, still accepts
`-display gtk,clipboard=on`, `qemu-img` runs, and — the check that actually
catches over-pruning — a paused machine starts with the **exact device set the
app emits** (q35, virtio-vga, e1000e, virtio-serial, qemu-vdagent, both pflash
units). A missing option ROM or firmware blob fails the build instead of a
user's first boot.

**The pinned build must be QEMU 11.1 or newer**, or host↔guest clipboard
silently disappears. Beware the version string: Windows builds are branch
snapshots that don't track upstream tags, and the pinned one reports `11.0.50`,
which *is* the 11.1 development tree. See `docs/VIRTUALIZATION.md`.

### QEMU licensing

QEMU is **GPLv2**. Shipping its binaries alongside this application is
redistribution, and the obligation that comes with it is *corresponding source*
for the exact build. The pinned payload is QEMU commit `54e84cdc7a`; its source
must be linked from the release notes:

```
https://github.com/qemu/qemu/archive/54e84cdc7a.tar.gz
```

QEMU runs as a separate process driven over a command line, so it is not linked
into the application and does not affect this project's own licensing.

## Uninstall options

The uninstaller shows two opt-in, default-off checkboxes:

- **Remove settings** — deletes `%LOCALAPPDATA%\PhoenixSimulacra`.
- **Remove WinFsp** — `msiexec /x` the WinFsp we installed (enabled only if the
  marker is present).

## Silent install (auto-updater)

```
Simulacra-Setup-<ver>.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
```

`UsePreviousTasks=yes` preserves the earlier desktop-icon / WinFsp choices, and
the WinFsp task is gated on "not already installed", so silent updates don't
re-install WinFsp. A silent uninstall (`/VERYSILENT`) leaves both removal
options off (settings and WinFsp are kept).

## WinFsp licensing

WinFsp is redistributed under **GPLv3 with the FLOSS exception**, which permits
bundling it with this project. The pinned release is WinFsp 2.1.25156
(`ProductCode {C79D9B29-3AF0-45B3-9DB9-226F3C2D2204}`). Its license text ships
with WinFsp's own MSI install.
