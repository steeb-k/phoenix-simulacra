# Publish built artifacts to the PUBLIC binaries repo as a GitHub Release.
#
# This is the counterpart to build-installer.ps1: build first (which produces a
# signed dist/Simulacra-Setup-<ver>.exe plus dist/simulacra/ and the verified
# WinFsp MSI), then run this to tag and upload the release assets to
# steeb-k/phoenix-simulacra-binaries, which is where the app's "Check for
# updates" flow looks. Kept separate from the build so a build never publishes
# as a side effect. A full release carries four assets:
#   - Simulacra-Setup-<ver>.exe        the installer (what auto-update fetches)
#   - Simulacra-Bundle-<ver>.zip       portable bundle: the 5 exes + WinFsp MSI
#                                      + portable.marker (disables auto-update)
#   - a .sha256 sidecar for each of the two above
#
# -ZipOnly publishes ONLY the ZIP + its sidecar: no installer, nothing signed.
# It needs just dist/simulacra/ (scripts/build-release.ps1) plus the staged
# WinFsp MSI, so a portable build can reach PE boot scripts without the whole
# sign-and-package pipeline. CAVEAT: the in-app updater looks for a
# `Simulacra-Setup-*.exe` asset in /releases/latest and reports "release has no
# installer asset" without one — a silent auto-check swallows that (installed
# users simply never update), but a manual "Check for updates" surfaces the
# error. (Portable users are unaffected: their check stops at the version and
# never looks for the asset.) Pair with -PreRelease to keep such a release out
# of /releases/latest entirely, leaving the in-app updater pointed at the last
# full release.
#
# The app queries /releases/latest, which ignores drafts and pre-releases, so a
# real update must be a FULL release. Use -Draft to stage one for review first.
#
# Prereqs: the GitHub CLI (`gh`) authenticated with push access to the binaries
# repo (`gh auth login`).
#
# Usage:
#   pwsh scripts/build-installer.ps1
#   pwsh scripts/publish-release.ps1                       # create release v<ver>
#   pwsh scripts/publish-release.ps1 -Draft                # stage as a draft
#   pwsh scripts/publish-release.ps1 -Force                # re-upload assets to an existing v<ver>
#   pwsh scripts/build-release.ps1                         # ZIP-only flow: build the bundle exes...
#   pwsh scripts/publish-release.ps1 -ZipOnly -PreRelease  # ...then publish just the ZIP
param(
    [switch]$Draft,
    [switch]$Force,
    # Publish only the portable ZIP (+ sidecar): no installer, nothing signed.
    [switch]$ZipOnly,
    # Mark the release as a pre-release, so /releases/latest ignores it.
    [switch]$PreRelease
)
$ErrorActionPreference = "Stop"
$repo = Join-Path $PSScriptRoot ".."
Set-Location $repo

# Where the built binaries live (public; separate from this private code repo).
$Target = "steeb-k/phoenix-simulacra-binaries"

# --- 1. App version from Cargo.toml (same read as build-installer.ps1) ---------
$cargo = Get-Content (Join-Path $repo "Cargo.toml") -Raw
$m = [regex]::Match($cargo, '(?ms)^\s*\[workspace\.package\].*?^\s*version\s*=\s*"([^"]+)"')
if (-not $m.Success) { throw "could not read [workspace.package] version from Cargo.toml" }
$version = $m.Groups[1].Value
$tag = "v$version"
Write-Host "== Publishing $tag to $Target" -ForegroundColor Cyan

# --- 2. Locate the built installer (not needed for -ZipOnly) ------------------
$setupName = "Simulacra-Setup-$version.exe"
$setup = Join-Path $repo "dist\$setupName"
if (-not $ZipOnly -and -not (Test-Path $setup)) {
    throw "installer not found: $setup`n  Run scripts/build-installer.ps1 first."
}

# --- 2b. Assemble the portable ZIP bundle -------------------------------------
# A no-installer alternative for advanced/portable use: the five bundle exes
# (both arches + launcher) plus the WinFsp MSI to install by hand. The exes come
# from build-release.ps1 and the verified MSI from build-installer.ps1 (which
# runs the former); either way both must already be present here. A -ZipOnly run
# needs only build-release.ps1, so its MSI must be staged from an earlier
# build-installer.ps1 run.
$bundleDir = Join-Path $repo "dist\simulacra"
$msi = Join-Path $repo "installer\build\winfsp.msi"
if (-not (Test-Path $bundleDir)) {
    throw "bundle folder not found: $bundleDir`n  Run scripts/build-release.ps1 (or build-installer.ps1) first."
}
if (-not (Test-Path $msi)) {
    throw "WinFsp MSI not found: $msi`n  Run scripts/build-installer.ps1 once (it downloads + verifies it)."
}
$exes = Get-ChildItem $bundleDir -Filter *.exe
if ($exes.Count -ne 5) {
    throw "expected 5 bundle exes in $bundleDir, found $($exes.Count). Re-run scripts/build-release.ps1."
}

$zipName = "Simulacra-Bundle-$version.zip"
$zip = Join-Path $repo "dist\$zipName"
# Stage under a versioned folder so the archive extracts into one tidy directory.
$stageRoot = Join-Path $repo "dist\zip-stage"
$stage = Join-Path $stageRoot "Simulacra-$version"
if (Test-Path $stageRoot) { Remove-Item -Recurse -Force $stageRoot }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
$exes | Copy-Item -Destination $stage
Copy-Item $msi -Destination $stage
# The ZIP and the installer ship the SAME exes, so this marker is the only thing
# that tells a running app it's the portable copy (phoenix-gui/src/updater.rs,
# `is_portable`). Without it the bundle would auto-download and silently run the
# installer on close -- which would install a SECOND, installed copy into Program
# Files and leave the extracted folder untouched, in the one environment (WinPE,
# off a USB stick) least able to afford the download. Keep the name in step with
# `PORTABLE_MARKER`; deleting it from an extracted bundle re-arms auto-update.
Set-Content -Path (Join-Path $stage "portable.marker") -Encoding ASCII -Value @(
    "This file marks the folder beside it as a PORTABLE Phoenix Simulacra bundle."
    ""
    "While it is here, the app never checks for, downloads, or installs updates on"
    "its own -- the About page's 'Check for updates' button reports new versions but"
    "downloads nothing. Pick new versions up from https://kznjk.com/."
    ""
    "Deleting this file turns automatic updates back on, which will install a"
    "separate, non-portable copy into Program Files."
)
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $stageRoot
Write-Host "== Built ZIP: $zip"

# --- 3. SHA-256 sidecars (sha256sum -c format: '<hash>  <filename>') -----------
# ASCII, no trailing newline, two spaces before the name (coreutils convention).
if (-not $ZipOnly) {
    $hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
    $shaPath = "$setup.sha256"
    [System.IO.File]::WriteAllText($shaPath, "$hash  $setupName")
    Write-Host "== SHA-256 (installer): $hash"
}

$zipHash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
$zipShaPath = "$zip.sha256"
[System.IO.File]::WriteAllText($zipShaPath, "$zipHash  $zipName")
Write-Host "== SHA-256 (zip):       $zipHash"

# --- 4. gh auth preflight ------------------------------------------------------
& gh auth status 2>$null
if ($LASTEXITCODE -ne 0) {
    throw "GitHub CLI is not authenticated. Run 'gh auth login' (needs push access to $Target)."
}

# --- 5. Create or update the release ------------------------------------------
# Every release asset, in one list, so create and re-upload stay in sync.
if ($ZipOnly) {
    $notes = "Portable ZIP-only release of Phoenix Simulacra $version " +
             "(unsigned; no installer).`n`n" +
             "SHA-256 ($zipName): $zipHash"
    $assets = @($zip, $zipShaPath)
} else {
    $notes = "Automated release of Phoenix Simulacra $version.`n`n" +
             "SHA-256 ($setupName): $hash`n" +
             "SHA-256 ($zipName): $zipHash"
    $assets = @($setup, $shaPath, $zip, $zipShaPath)
}

& gh release view $tag -R $Target 1>$null 2>$null
$exists = ($LASTEXITCODE -eq 0)

if ($exists) {
    if (-not $Force) {
        throw "release $tag already exists on $Target. Re-run with -Force to re-upload its assets."
    }
    Write-Host "== $tag exists; re-uploading assets (--clobber)" -ForegroundColor Yellow
    & gh release upload $tag @assets -R $Target --clobber
    if ($LASTEXITCODE -ne 0) { throw "gh release upload failed ($LASTEXITCODE)" }
} else {
    $ghArgs = @(
        "release", "create", $tag) + $assets + @(
        "-R", $Target,
        "--title", "Phoenix Simulacra $version",
        "--notes", $notes
    )
    if ($Draft) { $ghArgs += "--draft" }
    if ($PreRelease) { $ghArgs += "--prerelease" }
    & gh @ghArgs
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed ($LASTEXITCODE)" }
}

# --- 6. Report -----------------------------------------------------------------
$url = (& gh release view $tag -R $Target --json url -q .url)
Write-Host ""
Write-Host "Published $tag$(if ($Draft) { ' (draft)' }): $url" -ForegroundColor Green
if ($Draft) {
    Write-Host "Draft releases are NOT returned by /releases/latest — publish it to trigger auto-updates." -ForegroundColor DarkGray
}
if ($PreRelease) {
    Write-Host "Pre-releases are NOT returned by /releases/latest — the in-app updater still sees the last full release." -ForegroundColor DarkGray
}
if ($ZipOnly -and -not ($Draft -or $PreRelease)) {
    Write-Host "ZIP-only: this release carries NO installer asset, so the in-app updater's manual check reports 'release has no installer asset' until a full release supersedes it." -ForegroundColor Yellow
}
