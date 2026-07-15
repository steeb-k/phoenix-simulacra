# Build the Phoenix Simulacra installer (Simulacra-Setup-<ver>.exe).
#
# Pipeline:
#   1. Build the dual-arch bundle (scripts/build-release.ps1 -> dist/simulacra/).
#   2. Read the app version from the workspace Cargo.toml.
#   3. Ensure installer/build/winfsp.msi exists and matches the pinned SHA-256
#      (downloads it from winfsp.dev's GitHub release on first run / mismatch).
#   3b. Code-sign the bundle exes (before ISCC embeds them) with Azure Trusted
#      Signing -- automatic when scripts/artifact-signing-metadata.json exists;
#      pass -NoSign to skip (dev builds). See scripts/sign-artifacts.ps1.
#   4. Compile installer/simulacra.iss with Inno Setup (ISCC), version 6.6.0+.
#   5. Code-sign the installer itself.
#
# Output: dist/Simulacra-Setup-<ver>.exe
#
# Prereqs: everything build-release.ps1 needs, plus Inno Setup 6.6.0+
# (winget install JRSoftware.InnoSetup). 6.6.0 is required for the installer's
# native dark mode (WizardStyle=... dynamic) and PNG wizard image. Signing also
# needs the Windows SDK signtool, the Trusted Signing client tools
# (winget install Microsoft.Azure.TrustedSigningClientTools), and an Azure login.
param(
    [switch]$NoSign
)
$ErrorActionPreference = "Stop"
$repo = Join-Path $PSScriptRoot ".."
Set-Location $repo

# Sign when the metadata file is present unless the caller opted out.
$signMeta = Join-Path $PSScriptRoot "artifact-signing-metadata.json"
$doSign = (-not $NoSign) -and (Test-Path $signMeta)
$signScript = Join-Path $PSScriptRoot "sign-artifacts.ps1"

# --- Pinned WinFsp MSI (2.1 ABI, matching winfsp-sys 0.12.1+winfsp-2.1) --------
$WinFspUrl    = "https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi"
$WinFspSha256 = "073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A"

# --- 1. Build the bundle -------------------------------------------------------
Write-Host "== Building release bundle (dist/simulacra) ..." -ForegroundColor Cyan
& (Join-Path $PSScriptRoot "build-release.ps1")
if ($LASTEXITCODE -ne 0) { throw "build-release.ps1 failed ($LASTEXITCODE)" }

# --- 2. App version from Cargo.toml -------------------------------------------
$cargo = Get-Content (Join-Path $repo "Cargo.toml") -Raw
$m = [regex]::Match($cargo, '(?ms)^\s*\[workspace\.package\].*?^\s*version\s*=\s*"([^"]+)"')
if (-not $m.Success) { throw "could not read [workspace.package] version from Cargo.toml" }
$version = $m.Groups[1].Value
Write-Host "== App version: $version"

# --- 3. Ensure the pinned WinFsp MSI ------------------------------------------
$buildDir = Join-Path $repo "installer\build"
New-Item -ItemType Directory -Force -Path $buildDir | Out-Null
$msi = Join-Path $buildDir "winfsp.msi"

function Test-Msi {
    if (-not (Test-Path $msi)) { return $false }
    return ((Get-FileHash -Algorithm SHA256 $msi).Hash -eq $WinFspSha256)
}

if (Test-Msi) {
    Write-Host "== WinFsp MSI present and verified (SHA-256 match)."
} else {
    Write-Host "== Downloading WinFsp MSI: $WinFspUrl"
    Invoke-WebRequest -Uri $WinFspUrl -OutFile $msi -UseBasicParsing
    $got = (Get-FileHash -Algorithm SHA256 $msi).Hash
    if ($got -ne $WinFspSha256) {
        Remove-Item $msi -Force
        throw "WinFsp MSI SHA-256 mismatch!`n  expected $WinFspSha256`n  got      $got"
    }
    Write-Host "== WinFsp MSI downloaded and verified."
}

# --- 3b. Sign the bundle exes (before ISCC embeds them) -----------------------
$bundleExes = Get-ChildItem (Join-Path $repo "dist\simulacra") -Filter *.exe | Select-Object -ExpandProperty FullName
if ($doSign) {
    & $signScript @bundleExes
    if ($LASTEXITCODE -ne 0) { throw "signing the bundle exes failed ($LASTEXITCODE)" }
} else {
    $why = if ($NoSign) { "-NoSign" } else { "no artifact-signing-metadata.json" }
    Write-Host "== Skipping code signing ($why). Artifacts will be UNSIGNED." -ForegroundColor Yellow
}

# --- 4. Locate Inno Setup 6.6.0+ ----------------------------------------------
# Inno may be a per-machine (Program Files) or per-user (LocalAppData\Programs)
# install, and it isn't added to PATH by default.
$iscc = $null
$cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
if ($cmd) { $iscc = $cmd.Source }
if (-not $iscc) {
    foreach ($base in @(
        "${env:ProgramFiles(x86)}\Inno Setup 6",
        "$env:ProgramFiles\Inno Setup 6",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6")) {
        $c = Join-Path $base "ISCC.exe"
        if (Test-Path $c) { $iscc = $c; break }
    }
}
if (-not $iscc) {
    throw "Inno Setup compiler (ISCC.exe) not found. Install Inno Setup 6.6.0+:`n  winget install JRSoftware.InnoSetup"
}
# ISCC.exe's file version is unreliable (often 0.0.0.0), so this gate is
# best-effort: only hard-fail when we can positively read a version < 6.6.0.
# Otherwise proceed -- ISCC errors clearly on the 6.6.0 directives if too old.
$isccVer = (Get-Item $iscc).VersionInfo.ProductVersion
Write-Host "== Using ISCC: $iscc (reported version '$isccVer')"
$parsed = $null
$verStr = ($isccVer -split '[^0-9.]')[0]
if ($verStr -and [version]::TryParse($verStr, [ref]$parsed) -and $parsed.Major -gt 0) {
    if ($parsed -lt [version]"6.6.0") {
        throw "Inno Setup $isccVer is too old; 6.6.0+ required (native dark mode + PNG wizard image).`n  winget upgrade JRSoftware.InnoSetup"
    }
} else {
    Write-Host "   (could not determine ISCC version; assuming 6.6.0+)" -ForegroundColor DarkGray
}

# --- 5. Compile ----------------------------------------------------------------
$iss = Join-Path $repo "installer\simulacra.iss"
Write-Host "== Compiling installer ..." -ForegroundColor Cyan
& $iscc "/DAppVersion=$version" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

$out = Join-Path $repo "dist\Simulacra-Setup-$version.exe"
if (-not (Test-Path $out)) {
    throw "ISCC reported success but expected output not found: $out"
}

# --- 6. Sign the installer -----------------------------------------------------
if ($doSign) {
    & $signScript $out
    if ($LASTEXITCODE -ne 0) { throw "signing the installer failed ($LASTEXITCODE)" }
}

Write-Host ""
Write-Host "Done. Installer: $out $(if ($doSign) { '(signed)' } else { '(UNSIGNED)' })" -ForegroundColor Green
