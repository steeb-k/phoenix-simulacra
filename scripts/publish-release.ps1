# Publish a built installer to the PUBLIC binaries repo as a GitHub Release.
#
# This is the counterpart to build-installer.ps1: build first (which produces a
# signed dist/Simulacra-Setup-<ver>.exe plus dist/simulacra/ and the verified
# WinFsp MSI), then run this to tag and upload the release assets to
# steeb-k/phoenix-simulacra-binaries, which is where the app's "Check for
# updates" flow looks. Kept separate from the build so a build never publishes
# as a side effect. Each release carries four assets:
#   - Simulacra-Setup-<ver>.exe        the installer (what auto-update fetches)
#   - Simulacra-Bundle-<ver>.zip       portable bundle: the 5 exes + WinFsp MSI
#   - a .sha256 sidecar for each of the two above
#
# The app queries /releases/latest, which ignores drafts and pre-releases, so a
# real update must be a FULL release. Use -Draft to stage one for review first.
#
# Prereqs: the GitHub CLI (`gh`) authenticated with push access to the binaries
# repo (`gh auth login`).
#
# Usage:
#   pwsh scripts/build-installer.ps1
#   pwsh scripts/publish-release.ps1            # create release v<ver>
#   pwsh scripts/publish-release.ps1 -Draft     # stage as a draft
#   pwsh scripts/publish-release.ps1 -Force     # re-upload assets to an existing v<ver>
param(
    [switch]$Draft,
    [switch]$Force
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

# --- 2. Locate the built installer --------------------------------------------
$setupName = "Simulacra-Setup-$version.exe"
$setup = Join-Path $repo "dist\$setupName"
if (-not (Test-Path $setup)) {
    throw "installer not found: $setup`n  Run scripts/build-installer.ps1 first."
}

# --- 2b. Assemble the portable ZIP bundle -------------------------------------
# A no-installer alternative for advanced/portable use: the five bundle exes
# (both arches + launcher) plus the WinFsp MSI to install by hand. Both inputs
# are produced by build-installer.ps1 (bundle exes + downloaded/verified MSI), so
# they must already be present here.
$bundleDir = Join-Path $repo "dist\simulacra"
$msi = Join-Path $repo "installer\build\winfsp.msi"
if (-not (Test-Path $bundleDir)) {
    throw "bundle folder not found: $bundleDir`n  Run scripts/build-installer.ps1 first."
}
if (-not (Test-Path $msi)) {
    throw "WinFsp MSI not found: $msi`n  Run scripts/build-installer.ps1 first (it downloads + verifies it)."
}
$exes = Get-ChildItem $bundleDir -Filter *.exe
if ($exes.Count -ne 5) {
    throw "expected 5 bundle exes in $bundleDir, found $($exes.Count). Re-run scripts/build-installer.ps1."
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
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $stageRoot
Write-Host "== Built ZIP: $zip"

# --- 3. SHA-256 sidecars (sha256sum -c format: '<hash>  <filename>') -----------
# ASCII, no trailing newline, two spaces before the name (coreutils convention).
$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
$shaPath = "$setup.sha256"
[System.IO.File]::WriteAllText($shaPath, "$hash  $setupName")
Write-Host "== SHA-256 (installer): $hash"

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
$notes = "Automated release of Phoenix Simulacra $version.`n`n" +
         "SHA-256 ($setupName): $hash`n" +
         "SHA-256 ($zipName): $zipHash"

# Every release asset, in one list, so create and re-upload stay in sync.
$assets = @($setup, $shaPath, $zip, $zipShaPath)

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
