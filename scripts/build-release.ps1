# Build Phoenix Simulacra for all tier-1 Windows targets (x64 + ARM64).
# Requires: Rust stable, Visual Studio Build Tools with Desktop C++ and both
# x64 and ARM64 MSVC toolchains (see docs/WINDOWS-ARM64.md), plus LLVM/libclang
# (for the winfsp bindgen step) and WinFsp installed (for its SDK). The shipped
# binaries delay-load winfsp-<arch>.dll and locate it via the registry at run
# time; the app installer must bundle+install WinFsp (https://winfsp.dev).
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$targets = @(
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc"
)

# The winfsp feature's bindgen needs libclang. Point at the standard LLVM path
# if the caller hasn't set LIBCLANG_PATH.
if (-not $env:LIBCLANG_PATH) {
    $llvm = "C:\Program Files\LLVM\bin"
    if (Test-Path (Join-Path $llvm "libclang.dll")) { $env:LIBCLANG_PATH = $llvm }
}

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
    Write-Host "Building release for $t (with winfsp zero-space mount) ..."
    # Build the two shipped binaries with the winfsp feature. Not all workspace
    # crates define that feature, so build per-binary rather than --workspace.
    cargo build --release -p phoenix-cli --features winfsp --target $t
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --release -p phoenix-gui --features winfsp --target $t
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $outDir = Join-Path "dist" $t
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    $src = Join-Path "target" (Join-Path $t "release")
    Copy-Item (Join-Path $src "simulacra.exe") $outDir -Force
    Copy-Item (Join-Path $src "simulacra-gui.exe") $outDir -Force
}

Write-Host "Done. Artifacts:"
foreach ($t in $targets) {
    Write-Host "  dist\$t\simulacra.exe"
    Write-Host "  dist\$t\simulacra-gui.exe"
}
