# Run the Tier-2 system tests STAGED -- one cargo process per test binary, with a
# settle between, and a breadcrumb log that survives a hard hang.
#
# ## Why this exists alongside run-system-tests.ps1
#
# `run-system-tests.ps1` is a SINGLE cargo invocation. On a low-powered machine a
# single one-shot run has hard-hung Windows -- twice. It is not any one test: it is
# cumulative VHD attach/detach churn inside one uptime exhausting the storage
# stack (the crashes correlated with Windows handing out
# \Device\HarddiskVolume1065). Every binary passes cleanly on its own after a
# reboot. So: one process per binary, and let each detach finish before the next.
#
# The breadcrumb log is the point of the whole thing. A hard hang loses whatever
# is sitting in a stdout pipe; an Add-Content that already reached disk survives.
# That log is the only reason the x64 hang was ever localized.
#
# ## Usage (ELEVATED shell)
#
#     .\scripts\arm64-dev-env.ps1          # ARM64 only: puts MSVC on PATH
#     .\scripts\run-system-tests-staged.ps1
#
#     .\scripts\run-system-tests-staged.ps1 -Workers 2      # low-RAM machines
#     .\scripts\run-system-tests-staged.ps1 -Tests vss,mount
#     .\scripts\run-system-tests-staged.ps1 -Winfsp         # adds winfsp_mount
#
# T3 (destructive, real disks) is NOT here -- see real_disk.rs / real_clone.rs and
# their PHOENIX_T3_* opt-ins.

[CmdletBinding()]
param(
    # Default sweep. Order is cheapest-and-most-foundational first: if
    # resize_roundtrip is red, nothing after it means anything.
    [string[]]$Tests = @(
        'resize_roundtrip', 'sector_4kn', 'backup_restore_roundtrip', 'clone',
        'fat_family', 'refs_family', 'gpt_identity', 'partial_mbr', 'partial_clone',
        'mount', 'vhdx_container', 'vss', 'bitlocker'
    ),
    # Adds winfsp_mount. Needs the `winfsp` feature, which needs WinFsp installed
    # AND libclang at BUILD time (winfsp-sys runs bindgen). Without this flag
    # winfsp_mount is #![cfg(feature = "winfsp")]'d out to an empty binary, so
    # running it would silently report zero tests rather than fail.
    [switch]$Winfsp,
    # Clamped 1..=64 by the engine. In-flight buffers cap around 128 MiB at the
    # default min(cores, 8) -- too much for a sub-4 GB machine.
    [int]$Workers = 0,
    [string]$LogFile = "t2-staged.log",
    # Seconds between binaries. VHD detach is asynchronous; this is the settle.
    [int]$SettleSeconds = 5
)

$ErrorActionPreference = "Stop"

$identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)) {
    Write-Error "This script must run from an elevated (Administrator) PowerShell."
    exit 1
}

$feature = @()
if ($Winfsp) {
    $feature = @('--features', 'winfsp')
    $Tests = $Tests + 'winfsp_mount'
}

if ($Workers -gt 0) {
    $env:PHOENIX_WORKERS = "$Workers"
    Write-Host "PHOENIX_WORKERS = $Workers" -ForegroundColor Cyan
}

Add-Content $LogFile "===== T2 staged run $(Get-Date -Format o) ====="
Add-Content $LogFile "tests: $($Tests -join ',')  workers: $(if ($Workers) { $Workers } else { 'default' })  winfsp: $($Winfsp.IsPresent)"

# cargo writes its normal progress to stderr, and under ErrorActionPreference=Stop
# PowerShell 5.1 turns any native-command stderr line into a TERMINATING error --
# so a healthy build would abort the loop. Success is tracked via $LASTEXITCODE
# below, so drop back to Continue for the actual test runs.
$ErrorActionPreference = "Continue"

$results = [ordered]@{}
foreach ($t in $Tests) {
    Write-Host "`n=== $t ===" -ForegroundColor Cyan
    Add-Content $LogFile "START $t $(Get-Date -Format o)"

    cargo test -p phoenix-systests @feature --test $t -- --ignored --test-threads=1 --nocapture
    $code = $LASTEXITCODE

    Add-Content $LogFile "END   $t exit=$code $(Get-Date -Format o)"
    $results[$t] = $code
    if ($code -ne 0) { Write-Host "$t FAILED (exit $code)" -ForegroundColor Red }

    # Let the VHD detach land before the next binary attaches more.
    Start-Sleep -Seconds $SettleSeconds
}

Write-Host "`n===== summary =====" -ForegroundColor Cyan
foreach ($k in $results.Keys) {
    $code = $results[$k]
    $tag  = if ($code -eq 0) { "PASS" } else { "FAIL ($code)" }
    $col  = if ($code -eq 0) { "Green" } else { "Red" }
    Write-Host ("  {0,-26} {1}" -f $k, $tag) -ForegroundColor $col
}
Write-Host "`nbreadcrumbs: $LogFile"

$failed = @($results.Values | Where-Object { $_ -ne 0 }).Count
if ($failed -gt 0) {
    Write-Host "`n$failed binary/binaries failed. A failure is a RESULT -- capture the exact" -ForegroundColor Yellow
    Write-Host "error text and report it. Do not relax or skip the test to make it green." -ForegroundColor Yellow
    Write-Host "Known exception: vss.rs :: locked_backup_blocks_writers_for_duration is" -ForegroundColor Yellow
    Write-Host "flaky (~1-in-3, races a writer against the capture window). Re-run before" -ForegroundColor Yellow
    Write-Host "believing it. It is the ONLY test with a standing excuse." -ForegroundColor Yellow
}
exit $failed
