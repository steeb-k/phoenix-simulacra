# Build Phoenix Simulacra and assemble the shippable bundle: one folder holding
# both architectures plus a single arch-selecting launcher.
#
#   dist\simulacra\
#     simulacra-launcher.exe   x64 shim; picks + launches the right GUI at run time
#     simulacra.exe            x64 GUI
#     simulacra-cli.exe        x64 CLI
#     simulacra-arm.exe        ARM64 GUI
#     simulacra-cli-arm.exe    ARM64 CLI
#
# The launcher is x64-only on purpose: Windows-on-ARM64 runs it under emulation,
# so one binary covers both machines (see phoenix-launcher/src/main.rs).
#
# Requires: Rust stable, Visual Studio Build Tools with Desktop C++ and both the
# x64 and ARM64 MSVC toolchains (see docs/WINDOWS-ARM64.md), plus LLVM/libclang
# (for the winfsp bindgen step) and WinFsp installed (for its SDK). The shipped
# GUI/CLI binaries delay-load winfsp-<arch>.dll and locate it via the registry at
# run time; the app installer must bundle+install WinFsp (https://winfsp.dev).
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$x64  = "x86_64-pc-windows-msvc"
$arm  = "aarch64-pc-windows-msvc"
$targets = @($x64, $arm)

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
    Write-Host "Building GUI + CLI release for $t (with winfsp zero-space mount) ..."
    # Build the two shipped binaries with the winfsp feature. Not all workspace
    # crates define that feature, so build per-binary rather than --workspace.
    cargo build --release -p phoenix-cli --features winfsp --target $t
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --release -p phoenix-gui --features winfsp --target $t
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

# The launcher ships as a single x64 binary (emulated on ARM64). It has no
# winfsp dependency, so no feature flag.
Write-Host "Building launcher release for $x64 ..."
cargo build --release -p phoenix-launcher --target $x64
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Assemble the bundle. Wipe it first so it never accumulates stale binaries.
$bundle = Join-Path "dist" "simulacra"
if (Test-Path $bundle) { Remove-Item -Recurse -Force $bundle }
New-Item -ItemType Directory -Force -Path $bundle | Out-Null

$x64Rel = Join-Path "target" (Join-Path $x64 "release")
$armRel = Join-Path "target" (Join-Path $arm "release")

# source path -> bundled name. The cargo bin names (simulacra = CLI,
# simulacra-gui = GUI) are remapped to the shipped scheme here.
$layout = @(
    @{ Src = (Join-Path $x64Rel "simulacra-launcher.exe"); Dst = "simulacra-launcher.exe" }
    @{ Src = (Join-Path $x64Rel "simulacra-gui.exe");      Dst = "simulacra.exe" }
    @{ Src = (Join-Path $x64Rel "simulacra.exe");          Dst = "simulacra-cli.exe" }
    @{ Src = (Join-Path $armRel "simulacra-gui.exe");      Dst = "simulacra-arm.exe" }
    @{ Src = (Join-Path $armRel "simulacra.exe");          Dst = "simulacra-cli-arm.exe" }
)

foreach ($item in $layout) {
    if (-not (Test-Path $item.Src)) { throw "expected build output missing: $($item.Src)" }
    Copy-Item $item.Src (Join-Path $bundle $item.Dst) -Force
}

Write-Host ""
Write-Host "Done. Bundle: $bundle"
foreach ($item in $layout) {
    Write-Host ("  {0}" -f (Join-Path $bundle $item.Dst))
}
