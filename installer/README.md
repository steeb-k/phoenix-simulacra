# Installer

Builds `Simulacra-Setup-<ver>.exe` — the single Windows installer for Phoenix
Simulacra. It lays down the dual-arch bundle (x64 + ARM64 binaries and the
arch-selecting launcher), creates Start Menu entries and an optional desktop
icon, and silently installs the bundled **WinFsp** driver so mounting backups
works out of the box.

## Build

```powershell
# One-time: install the compiler (6.6.0+ required)
winget install JRSoftware.InnoSetup

# Build bundle + fetch/verify WinFsp MSI + compile installer
pwsh scripts\build-installer.ps1
# -> dist\Simulacra-Setup-<ver>.exe
```

`build-installer.ps1` runs `build-release.ps1` first, then downloads the pinned
WinFsp MSI to `installer\build\winfsp.msi` (verified against a pinned SHA-256;
not committed), reads the version from the workspace `Cargo.toml`, and compiles
`simulacra.iss`.

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

A marker (`HKLM\Software\Phoenix Simulacra\InstalledWinFsp`) records whether the
installer installed WinFsp, so the uninstaller only offers to remove it when we
put it there (never a WinFsp shared with rclone / sshfs-win).

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
