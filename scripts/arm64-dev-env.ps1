# Put the native ARM64 MSVC toolchain into the CURRENT PowerShell session.
#
# Why this script exists: `& vcvarsall.bat arm64` DOES NOT WORK from PowerShell.
# It spawns a child cmd.exe, sets the variables there, and they die with it. The
# MSVC environment is more than PATH -- it is INCLUDE, LIB, LIBPATH and a dozen
# VSCMD_* vars -- so it cannot be pinned into the user PATH either. VS ships the
# correct entry point for this, and this is it.
#
# Usage (a plain run is enough -- a .ps1 shares the calling process's environment,
# so the $env: changes below persist after it exits; no dot-sourcing needed):
#
#     .\scripts\arm64-dev-env.ps1
#     cargo test -p phoenix-core
#
# `cargo` and `rustup` themselves are on the user PATH already (rustup put them
# there). If `cargo` is not found, you are in a shell that was opened BEFORE
# rustup was installed -- a process inherits its parent's environment and never
# re-reads the registry. Open a new terminal.

[CmdletBinding()]
param(
    # Override if VS lives somewhere else. Build Tools is the default because
    # that is what the reference ARM64 box has; a full VS install works too.
    [string]$VsInstallPath = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
)

$ErrorActionPreference = "Stop"

$devShell = Join-Path $VsInstallPath "Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
if (-not (Test-Path $devShell)) {
    Write-Error @"
Visual Studio DevShell module not found at:
    $devShell

Install VS Build Tools with the ARM64 MSVC toolset and the Windows SDK:
    winget install --id Microsoft.VisualStudio.2022.BuildTools --override `
      "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools ``
       --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 ``
       --add Microsoft.VisualStudio.Component.Windows11SDK.22621"

Or pass -VsInstallPath if it is installed elsewhere.
"@
    exit 1
}

Import-Module $devShell
# -SkipAutomaticLocation keeps us in the repo instead of being dumped in VS's
# default source directory. host_arch=arm64 selects the NATIVE compiler, not the
# emulated x64 one.
Enter-VsDevShell -VsInstallPath $VsInstallPath `
                 -DevCmdArguments '-arch=arm64 -host_arch=arm64' `
                 -SkipAutomaticLocation | Out-Null

$cl    = (Get-Command cl    -ErrorAction SilentlyContinue).Source
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source

if (-not $cl) { Write-Error "cl.exe still not on PATH after entering the dev shell."; exit 1 }

Write-Host "ARM64 dev environment ready." -ForegroundColor Green
Write-Host "  cl    : $cl"
Write-Host "  cargo : $(if ($cargo) { $cargo } else { '<NOT FOUND -- open a new terminal, or install rustup>' })"
Write-Host "  target: $env:VSCMD_ARG_TGT_ARCH (host $env:VSCMD_ARG_HOST_ARCH)"
