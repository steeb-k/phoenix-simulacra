# Build Carbon Phoenix for all tier-1 Windows targets (x64 + ARM64).
# Requires: Rust stable, Visual Studio Build Tools with Desktop C++ and both
# x64 and ARM64 MSVC toolchains (see docs/WINDOWS-ARM64.md).
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$targets = @(
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc"
)

foreach ($t in $targets) {
    # rustup logs info to stderr; with $ErrorActionPreference Stop that becomes a terminating error.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    rustup target add $t 2>&1 | Out-Null
    $ErrorActionPreference = $prev
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $t failed with exit code $LASTEXITCODE"
    }
}

foreach ($t in $targets) {
    Write-Host "Building release for $t ..."
    cargo build --release --workspace --target $t
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $outDir = Join-Path "dist" $t
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    $src = Join-Path "target" (Join-Path $t "release")
    Copy-Item (Join-Path $src "carbon-phoenix.exe") $outDir -Force
    Copy-Item (Join-Path $src "carbon-phoenix-gui.exe") $outDir -Force
}

Write-Host "Done. Artifacts:"
foreach ($t in $targets) {
    Write-Host "  dist\$t\carbon-phoenix.exe"
    Write-Host "  dist\$t\carbon-phoenix-gui.exe"
}
