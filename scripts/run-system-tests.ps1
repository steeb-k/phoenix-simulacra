# Run the Tier-2 system tests (phoenix-systests) on the local machine.
#
# These tests create, attach, format, back up, restore, clone, and mount REAL
# virtual disks (VHDX) via diskpart. They need an elevated shell and they must
# run single-threaded (diskpart / disk-index discovery is process-global).
#
# Usage (from an elevated PowerShell):
#   .\scripts\run-system-tests.ps1
#   .\scripts\run-system-tests.ps1 -Test ntfs_backup_restore_roundtrip_same_size

[CmdletBinding()]
param(
    [string]$Test = "",
    # Extra args passed through to the test binary after `--`.
    [string[]]$Extra = @()
)

$ErrorActionPreference = "Stop"

# --- Elevation check ---------------------------------------------------------
$identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) {
    Write-Error "This script must run from an elevated (Administrator) PowerShell."
    exit 1
}

# --- Clean up any VHDs leaked by a previous crashed run ----------------------
$scratch = Join-Path $env:TEMP "phoenix-systests"
if (Test-Path $scratch) {
    Write-Host "Detaching/cleaning leaked VHDs in $scratch ..." -ForegroundColor Yellow
    Get-ChildItem -Path $scratch -Filter *.vhdx -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            $img = Get-DiskImage -ImagePath $_.FullName -ErrorAction SilentlyContinue
            if ($img -and $img.Attached) {
                Dismount-DiskImage -ImagePath $_.FullName -ErrorAction SilentlyContinue | Out-Null
            }
        } catch {}
        Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue
    }
}

# --- Run ---------------------------------------------------------------------
# Build with the winfsp feature so the suite exercises the shipping zero-space
# mount path (needs libclang + WinFsp installed, same as build-release.ps1).
if (-not $env:LIBCLANG_PATH) {
    $llvm = "C:\Program Files\LLVM\bin"
    if (Test-Path (Join-Path $llvm "libclang.dll")) { $env:LIBCLANG_PATH = $llvm }
}
$cargoArgs = @("test", "-p", "phoenix-systests", "--features", "winfsp")
if ($Test -ne "") { $cargoArgs += $Test }
$cargoArgs += @("--", "--ignored", "--test-threads=1", "--nocapture")
$cargoArgs += $Extra

Write-Host "cargo $($cargoArgs -join ' ')" -ForegroundColor Cyan
& cargo @cargoArgs
exit $LASTEXITCODE
