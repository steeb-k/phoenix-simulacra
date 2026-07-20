# Build the curated QEMU payload that the installer bundles.
#
# Upstream ships one installer carrying every emulator QEMU builds -- 55 system
# targets and firmware for all of them, 1.17 GB installed. We use exactly one
# (x86_64) and its firmware, so this script extracts the upstream installer and
# prunes it to ~220 MB. NSIS silent installs take default component selection
# and QEMU's script exposes no component flags, so pruning is the only way to
# avoid shipping a gigabyte of emulators for architectures we never launch.
#
# Output: dist/qemu-payload/ (the tree) + dist/<name>.zip, which is uploaded to
# the binaries repo. build-installer.ps1 then downloads THAT zip by pinned hash
# -- it never runs this script, so a release build never depends on the
# upstream installer still being at its URL.
#
# Run this only when moving to a new QEMU build, and TEST the result (see
# -Verify below) before uploading it.
#
# Prereqs: 7-Zip (winget install 7zip.7zip).
param(
    # Use an already-downloaded upstream installer instead of fetching it.
    [string]$Installer,
    # Skip the post-build checks (not recommended).
    [switch]$NoVerify
)
$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..")

# --- Pinned upstream build -----------------------------------------------------
# Weilnetz publishes master snapshots in the same listing as releases, so the
# version string does NOT track upstream tags: this build reports 11.0.50, which
# IS the 11.1 development tree. Clipboard needs 11.1+, so do not "upgrade" this
# pin to a lower-numbered stable release -- see docs/VIRTUALIZATION.md.
$QemuUrl     = "https://qemu.weilnetz.de/w64/2026/qemu-w64-setup-20260501.exe"
$QemuSha256  = "A8B29572AFB4C6AD024B7DE129C81033E9FD191B9E054E3A52EA0BED24AC19EF"
$QemuVersion = "11.0.50"          # as reported by --version
$QemuCommit  = "54e84cdc7a"       # for the GPLv2 corresponding-source link
$PayloadName = "qemu-x86_64-$QemuVersion-20260501-win64"

# What the guest recipe actually needs. Anything missing here is a build-time
# failure, not a runtime surprise.
$Required = @(
    "qemu-system-x86_64.exe",     # the emulator
    "qemu-img.exe",               # creates the qcow2 session overlay
    "share\edk2-x86_64-code.fd",  # UEFI code flash
    "share\edk2-i386-vars.fd",    # UEFI NVRAM template
    "share\bios-256k.bin",        # SeaBIOS, for MBR/BIOS guests
    "share\kvmvapic.bin",
    "share\vgabios-virtio.bin",   # virtio-vga, the video device we always use
    "share\efi-e1000e.rom"        # the NIC we always attach
)

function Get-SevenZip {
    $c = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    foreach ($p in @("$env:ProgramFiles\7-Zip\7z.exe", "${env:ProgramFiles(x86)}\7-Zip\7z.exe")) {
        if (Test-Path $p) { return $p }
    }
    throw "7-Zip not found. Install it:`n  winget install 7zip.7zip"
}

$sz = Get-SevenZip
$work = Join-Path $repo "dist\qemu-payload"
$stage = Join-Path $repo "dist\.qemu-extract"

# --- 1. Get the upstream installer --------------------------------------------
if (-not $Installer) {
    $Installer = Join-Path $repo "dist\.qemu-upstream.exe"
    if (Test-Path $Installer) {
        if ((Get-FileHash -Algorithm SHA256 $Installer).Hash -ne $QemuSha256) {
            Remove-Item $Installer -Force
        }
    }
    if (-not (Test-Path $Installer)) {
        Write-Host "== Downloading upstream QEMU installer (190 MB): $QemuUrl" -ForegroundColor Cyan
        Invoke-WebRequest -Uri $QemuUrl -OutFile $Installer -UseBasicParsing
    }
}
$got = (Get-FileHash -Algorithm SHA256 $Installer).Hash
if ($got -ne $QemuSha256) {
    throw "Upstream QEMU installer SHA-256 mismatch!`n  expected $QemuSha256`n  got      $got"
}
Write-Host "== Upstream installer verified ($QemuVersion)."

# --- 2. Extract ----------------------------------------------------------------
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
Write-Host "== Extracting ..." -ForegroundColor Cyan
& $sz x $Installer "-o$stage" -y | Out-Null
if ($LASTEXITCODE -ne 0) { throw "7z extraction failed ($LASTEXITCODE)" }
# NSIS bookkeeping, not part of the program.
foreach ($junk in @('$PLUGINSDIR', 'Uninstall.exe')) {
    $p = Join-Path $stage $junk
    if (Test-Path $p) { Remove-Item $p -Recurse -Force }
}

# --- 3. Prune ------------------------------------------------------------------
# Keep x86_64 and drop the other 54 system emulators, their firmware, and the
# GTK translations. `qemu-system-x86_64w.exe` goes too: it is the windowed
# variant, and we always spawn the console one with CREATE_NO_WINDOW.
$all = Get-ChildItem $stage -Recurse -File
$drop = $all | Where-Object {
    $_.Name -match '^qemu-system-(?!x86_64\.exe$)' -or
    ($_.FullName -match '\\share\\' -and $_.Name -match
        '^(edk2-(arm|aarch64|riscv|loongarch)|opensbi|u-boot|palcode|petalogix|hppa-|openbios|QEMU,|bamboo|canyonlands|s390|slof|skiboot|vof|npcm)') -or
    $_.FullName -match '\\share\\locale\\'
}
$before = ($all | Measure-Object Length -Sum).Sum
foreach ($f in $drop) { Remove-Item $f.FullName -Force }
# Sweep directories the prune emptied.
Get-ChildItem $stage -Recurse -Directory |
    Sort-Object { $_.FullName.Length } -Descending |
    Where-Object { -not (Get-ChildItem $_.FullName -Recurse -File) } |
    ForEach-Object { Remove-Item $_.FullName -Recurse -Force }

$kept = Get-ChildItem $stage -Recurse -File
$after = ($kept | Measure-Object Length -Sum).Sum
Write-Host ("== Pruned {0} files: {1:N0} MB -> {2:N0} MB" -f $drop.Count, ($before/1MB), ($after/1MB))

# --- 4. Check the payload is complete ------------------------------------------
$missing = $Required | Where-Object { -not (Test-Path (Join-Path $stage $_)) }
if ($missing) {
    throw "Curated payload is missing required files:`n  " + ($missing -join "`n  ")
}

if (-not $NoVerify) {
    $exe = Join-Path $stage "qemu-system-x86_64.exe"
    $ver = (& $exe --version 2>&1 | Select-Object -First 1)
    if ($ver -notmatch [regex]::Escape($QemuVersion)) {
        throw "Pruned QEMU reports '$ver', expected version $QemuVersion"
    }
    # The whole reason for this build: clipboard needs 11.1+, and a pruned tree
    # that cannot parse the option would silently ship without it.
    $probe = (& $exe -display "gtk,clipboard=on" -machine help 2>&1) -join "`n"
    if ($probe -match "is unexpected") {
        throw "Pruned QEMU rejects -display gtk,clipboard=on -- wrong build, clipboard would be lost"
    }
    # A pruned tree missing a DLL usually dies here rather than at --version.
    $img = & (Join-Path $stage "qemu-img.exe") --version 2>&1 | Select-Object -First 1
    if ($LASTEXITCODE -ne 0) { throw "qemu-img failed to run from the pruned tree: $img" }

    # The check that actually catches over-pruning: assemble the REAL device
    # set the app emits and start a paused machine. A missing option ROM,
    # firmware blob or device module fails here, at build time, instead of on
    # a user's first boot. TCG rather than WHPX so it needs no elevation --
    # this proves the devices exist, not that they run fast.
    $smoke = Join-Path $env:TEMP "qemu-payload-smoke"
    New-Item -ItemType Directory -Force $smoke | Out-Null
    Copy-Item (Join-Path $stage "share\edk2-x86_64-code.fd") "$smoke\code.fd" -Force
    Copy-Item (Join-Path $stage "share\edk2-i386-vars.fd")   "$smoke\vars.fd" -Force
    $smokeLog = "$smoke\stderr.txt"
    $smokeArgs = @(
        '-machine','q35','-accel','tcg','-cpu','Skylake-Client-v4','-m','1024',
        '-display','none','-S',
        '-vga','none','-device','virtio-vga',
        '-netdev','user,id=net0','-device','e1000e,netdev=net0,id=nic0',
        '-device','virtio-serial-pci',
        '-chardev','qemu-vdagent,id=vdagent0,name=vdagent,clipboard=on',
        '-device','virtserialport,chardev=vdagent0,name=com.redhat.spice.0',
        '-drive',"if=pflash,format=raw,file=$smoke\code.fd",
        '-drive',"if=pflash,format=raw,file=$smoke\vars.fd"
    )
    $proc = Start-Process -FilePath $exe -ArgumentList $smokeArgs -PassThru -NoNewWindow `
        -RedirectStandardError $smokeLog
    Start-Sleep -Seconds 4
    if ($proc.HasExited) {
        $err = (Get-Content $smokeLog -ErrorAction SilentlyContinue) -join "`n"
        throw "Pruned QEMU could not start the app's device set (exit $($proc.ExitCode)):`n$err"
    }
    $proc | Stop-Process -Force
    Remove-Item $smoke -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "== Verified: $ver, clipboard supported, qemu-img runs, full device set starts."
}

# --- 5. Publish ----------------------------------------------------------------
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
Move-Item $stage $work

$zip = Join-Path $repo "dist\$PayloadName.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Write-Host "== Compressing ..." -ForegroundColor Cyan
Compress-Archive -Path (Join-Path $work "*") -DestinationPath $zip -CompressionLevel Optimal
$zipHash = (Get-FileHash -Algorithm SHA256 $zip).Hash

Write-Host ""
Write-Host "Payload:  $work" -ForegroundColor Green
Write-Host ("Zip:      {0} ({1:N0} MB)" -f $zip, ((Get-Item $zip).Length/1MB)) -ForegroundColor Green
Write-Host "SHA-256:  $zipHash" -ForegroundColor Green
Write-Host ""
Write-Host "Next: upload the zip to the binaries repo, then update build-installer.ps1:"
Write-Host "  `$QemuPayloadSha256 = `"$zipHash`""
Write-Host ""
Write-Host "GPLv2: corresponding source for this build is QEMU commit $QemuCommit"
Write-Host "  https://github.com/qemu/qemu/archive/$QemuCommit.tar.gz"
