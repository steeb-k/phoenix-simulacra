# Publish a built installer to the PUBLIC binaries repo as a GitHub Release.
#
# This is the counterpart to build-installer.ps1: build first (which produces a
# signed dist/Simulacra-Setup-<ver>.exe), then run this to tag and upload it —
# plus a .sha256 sidecar — to steeb-k/phoenix-simulacra-binaries, which is where
# the app's "Check for updates" flow looks. Kept separate from the build so a
# build never publishes as a side effect.
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

# --- 3. SHA-256 sidecar (sha256sum -c format: '<hash>  <filename>') ------------
$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
$shaPath = "$setup.sha256"
# ASCII, no trailing newline, two spaces before the name (coreutils convention).
[System.IO.File]::WriteAllText($shaPath, "$hash  $setupName")
Write-Host "== SHA-256: $hash"

# --- 4. gh auth preflight ------------------------------------------------------
& gh auth status 2>$null
if ($LASTEXITCODE -ne 0) {
    throw "GitHub CLI is not authenticated. Run 'gh auth login' (needs push access to $Target)."
}

# --- 5. Create or update the release ------------------------------------------
$notes = "Automated release of Phoenix Simulacra $version.`n`nSHA-256 (Simulacra-Setup-$version.exe): $hash"

& gh release view $tag -R $Target 1>$null 2>$null
$exists = ($LASTEXITCODE -eq 0)

if ($exists) {
    if (-not $Force) {
        throw "release $tag already exists on $Target. Re-run with -Force to re-upload its assets."
    }
    Write-Host "== $tag exists; re-uploading assets (--clobber)" -ForegroundColor Yellow
    & gh release upload $tag $setup $shaPath -R $Target --clobber
    if ($LASTEXITCODE -ne 0) { throw "gh release upload failed ($LASTEXITCODE)" }
} else {
    $ghArgs = @(
        "release", "create", $tag, $setup, $shaPath,
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
