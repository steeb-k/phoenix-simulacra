<#
.SYNOPSIS
  Tier-3 real-hardware disk-to-disk clone matrix (phoenix-systests/tests/real_clone.rs).

.DESCRIPTION
  Drives every clone mode the Clone page can reach, between two REAL disks:
  full clone, table-style switch (MBR -> GPT), clone matching the source style,
  clone with grow, clone with shrink, partial clone into a live target, and a
  clone back onto removable media.

  *** BOTH DISKS ARE COMPLETELY WIPED. ***

  Both are individually validated by the RealDisk safety gate: the boot and
  system disks are refused outright, sizes must fall inside the window, and a
  NON-REMOVABLE disk additionally requires -AllowFixed plus an exact serial pin.
  The source is gated as hard as the target, because these tests write their own
  fixture onto it before cloning it.

  Runs ONE TEST PER CARGO PROCESS with a settle in between. That is not
  fussiness: a single long unbroken run of VHD/disk churn has bugchecked the dev
  box (docs/TESTING.md). A breadcrumb is appended to a file before and after each
  test, because a hard hang loses buffered stdout but not a completed write — if
  the machine does go down, the breadcrumb names the test that took it.

.EXAMPLE
  # Source: disk 2, a 32 GB USB flash stick.  Target: disk 3, a 4 TB external HDD.
  .\scripts\run-real-clone.ps1 -SourceDisk 2 -TargetDisk 3 -AllowFixed `
      -TargetSerial "<exact-serial>" -MaxGB 4200

.EXAMPLE
  # See exactly what would run, and which disks it resolved, without touching them.
  .\scripts\run-real-clone.ps1 -SourceDisk 2 -TargetDisk 3 -AllowFixed `
      -TargetSerial "<exact-serial>" -MaxGB 4200 -WhatIf
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [int]    $SourceDisk,
    [Parameter(Mandatory)] [int]    $TargetDisk,
    [string] $SourceSerial,
    [string] $TargetSerial,
    # Required when the TARGET (or source) is non-removable — e.g. an external HDD.
    [switch] $AllowFixed,
    [double] $MinGB = 16,
    [double] $MaxGB = 4200,
    [string] $Test,
    [switch] $WhatIf
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
        ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this from an ELEVATED shell (raw disk handles, diskpart, formatting)."
}

if ($SourceDisk -eq $TargetDisk) {
    throw "SourceDisk and TargetDisk are the same disk ($SourceDisk). That would clone a disk over itself."
}

# --- Show the operator exactly which physical devices these numbers mean. ------
# Disk numbers move. Serial numbers don't. Anyone about to wipe two drives should
# see the model and serial of both before agreeing to it.
Write-Host "`n=== Disks this run will WIPE ===" -ForegroundColor Yellow
$resolved = foreach ($n in @($SourceDisk, $TargetDisk)) {
    $d  = Get-Disk -Number $n
    $dd = Get-CimInstance Win32_DiskDrive -Filter "Index=$n"
    [pscustomobject]@{
        Role      = if ($n -eq $SourceDisk) { 'SOURCE' } else { 'TARGET' }
        Disk      = $n
        Model     = $d.FriendlyName
        SizeGB    = [math]::Round($d.Size / 1e9, 1)
        Bus       = $d.BusType
        Media     = $dd.MediaType
        Serial    = $d.SerialNumber
        Style     = $d.PartitionStyle
        IsBoot    = $d.IsBoot
        IsSystem  = $d.IsSystem
    }
}
$resolved | Format-Table -AutoSize | Out-String | Write-Host

foreach ($r in $resolved) {
    if ($r.IsBoot -or $r.IsSystem) {
        throw "REFUSING: disk $($r.Disk) ($($r.Role)) is the boot/system disk."
    }
}

# --- Env opt-in consumed by RealDisk::acquire / acquire_source -----------------
$env:PHOENIX_T3_SRC_DISK = "$SourceDisk"
$env:PHOENIX_T3_DISK     = "$TargetDisk"
$env:PHOENIX_T3_MIN_GB   = "$MinGB"
$env:PHOENIX_T3_MAX_GB   = "$MaxGB"
if ($SourceSerial) { $env:PHOENIX_T3_SRC_SERIAL = $SourceSerial }
if ($TargetSerial) { $env:PHOENIX_T3_SERIAL     = $TargetSerial }
if ($AllowFixed)   { $env:PHOENIX_T3_ALLOW_FIXED = "1" }

# pwsh rewrites PSModulePath; the harness shells Windows PowerShell 5.1 for the
# storage cmdlets, which then can't find its own modules. Put it back.
$env:PSModulePath = "$env:ProgramFiles\WindowsPowerShell\Modules;$env:SystemRoot\system32\WindowsPowerShell\v1.0\Modules"

$tests = @(
    'real_clone_full_mbr_source_to_gpt_target',
    'real_clone_full_match_source_keeps_mbr',
    'real_clone_grow_ntfs_into_larger_slot',
    'real_clone_shrink_ntfs_into_smaller_slot',
    'real_partial_clone_preserves_the_sibling',
    'real_clone_back_from_hdd_to_flash'
)
if ($Test) { $tests = @($Test) }

if ($WhatIf) {
    Write-Host "WhatIf: would run these tests, one cargo process each:" -ForegroundColor Cyan
    $tests | ForEach-Object { Write-Host "  $_" }
    Write-Host "`nNothing was written." -ForegroundColor Green
    return
}

Write-Host "BOTH disks above will be ERASED. Ctrl-C now if that is not what you want." -ForegroundColor Red
$answer = Read-Host "Type the TARGET disk number ($TargetDisk) to proceed"
if ($answer.Trim() -ne "$TargetDisk") { throw "Aborted (got '$answer')." }

$logdir = Join-Path $repo "target\t3-clone"
New-Item -ItemType Directory -Force -Path $logdir | Out-Null
$progress = Join-Path $logdir "progress.txt"
"=== real_clone run $(Get-Date -Format o) : src=$SourceDisk tgt=$TargetDisk ===" |
    Out-File $progress -Encoding utf8

$failed = @()
foreach ($t in $tests) {
    Write-Host "`n--- $t ---" -ForegroundColor Cyan
    "START  $t  $(Get-Date -Format HH:mm:ss)" | Out-File $progress -Append -Encoding utf8

    cargo test -p phoenix-systests --test real_clone -- `
        --ignored --test-threads=1 --nocapture --exact $t 2>&1 |
        Tee-Object -FilePath (Join-Path $logdir "$t.txt")

    $code = $LASTEXITCODE
    "FINISH $t  exit=$code  $(Get-Date -Format HH:mm:ss)" | Out-File $progress -Append -Encoding utf8
    if ($code -ne 0) {
        $failed += $t
        Write-Host "FAILED: $t" -ForegroundColor Red
    }
    # Detach/dismount is asynchronous. Give the storage stack a moment before the
    # next test starts hammering the same two disks.
    Start-Sleep -Seconds 8
}

"=== completed $(Get-Date -Format o) ===" | Out-File $progress -Append -Encoding utf8

Write-Host "`n=== Summary ===" -ForegroundColor Yellow
Write-Host "logs: $logdir"
if ($failed.Count -eq 0) {
    Write-Host "All $($tests.Count) clone scenarios PASSED." -ForegroundColor Green
} else {
    Write-Host "FAILED ($($failed.Count)/$($tests.Count)):" -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    exit 1
}
