# Sign one or more PE files (exe) with Azure Trusted Signing.
#
# Uses signtool + the Trusted Signing dlib, driven by the metadata in
# scripts/artifact-signing-metadata.json (Endpoint / CodeSigningAccountName /
# CertificateProfileName). Authentication is via DefaultAzureCredential -- an
# `az login` session works, as do the AZURE_* service-principal env vars. The
# signing identity needs the Trusted Signing "Certificate Profile Signer" role.
#
# Prereqs:
#   - Windows SDK signing tools (signtool.exe)
#   - winget install Microsoft.Azure.TrustedSigningClientTools  (provides the dlib)
#
# Usage: pwsh scripts/sign-artifacts.ps1 file1.exe [file2.exe ...]
param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$Files
)
$ErrorActionPreference = "Stop"

$meta = Join-Path $PSScriptRoot "artifact-signing-metadata.json"
if (-not (Test-Path $meta)) { throw "signing metadata not found: $meta" }

# signtool.exe -- prefer PATH, else newest x64 from the Windows SDK.
$signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source
if (-not $signtool) {
    $signtool = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}
if (-not $signtool) { throw "signtool.exe not found -- install the Windows SDK signing tools." }

# Trusted Signing dlib from the winget client-tools package.
$dlib = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\MicrosoftArtifactSigningClientTools" -Recurse -Filter "Azure.CodeSigning.Dlib.dll" -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $dlib) {
    throw "Azure.CodeSigning.Dlib.dll not found. Install:`n  winget install Microsoft.Azure.TrustedSigningClientTools"
}

$paths = @($Files | ForEach-Object { (Resolve-Path $_).Path })
Write-Host ("== Signing {0} file(s) with Azure Trusted Signing ..." -f $paths.Count) -ForegroundColor Cyan
# signtool accepts multiple files in one call -> one auth round-trip.
& $signtool sign /v /fd SHA256 /tr "http://timestamp.acs.microsoft.com" /td SHA256 /dlib "$dlib" /dmdf "$meta" @paths
if ($LASTEXITCODE -ne 0) { throw "signtool sign failed ($LASTEXITCODE)" }
