<#
.SYNOPSIS
    Packs the app-icon PNGs in assets/ into a Windows .ico.

.DESCRIPTION
    There is no ImageMagick on the dev box, so the ICO container is written by
    hand: an ICONDIR header, one ICONDIRENTRY per size, then the raw PNG blobs
    (Vista+ ICOs may store entries PNG-compressed rather than as BMP DIBs).

    Sizes with no PNG on disk are downscaled from -SourceSize with
    HighQualityBicubic. Entries larger than 256px cannot be expressed in the
    ICO directory (width/height are single bytes) and are skipped.

.EXAMPLE
    pwsh scripts/make-appicon.ps1 -Stem phoenix-appicon
#>
[CmdletBinding()]
param(
    # Basename of the asset family: <stem>-<N>px.png in, <stem>.ico out.
    [string] $Stem = 'phoenix-appicon',

    # Icon sizes to embed. 48 is what the shell uses for "medium icons".
    [int[]] $Sizes = @(16, 32, 48, 64, 80, 96, 128, 256),

    # PNG to downscale from when a size has no hand-authored file.
    [int] $SourceSize = 512
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$assets = Join-Path (Split-Path -Parent $PSScriptRoot) 'assets'
$out = Join-Path $assets "$Stem.ico"

# Render a size to an in-memory PNG blob, preferring a hand-authored file.
function Get-PngBytes {
    param([int] $Size)

    $exact = Join-Path $assets "$Stem-${Size}px.png"
    if (Test-Path -LiteralPath $exact) {
        Write-Host ("  {0,3}px  <- {1}" -f $Size, (Split-Path -Leaf $exact))
        return [System.IO.File]::ReadAllBytes($exact)
    }

    $sourcePath = Join-Path $assets "$Stem-${SourceSize}px.png"
    if (-not (Test-Path -LiteralPath $sourcePath)) {
        throw "no $Size px PNG and no ${SourceSize}px master to downscale from ($sourcePath)"
    }

    Write-Host ("  {0,3}px  <- downscaled from {1}px" -f $Size, $SourceSize)
    $source = [System.Drawing.Image]::FromFile($sourcePath)
    try {
        $bmp = New-Object System.Drawing.Bitmap $Size, $Size
        try {
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            try {
                $g.CompositingMode = 'SourceCopy'
                $g.InterpolationMode = 'HighQualityBicubic'
                $g.PixelOffsetMode = 'HighQuality'
                $g.SmoothingMode = 'HighQuality'
                $g.DrawImage($source, 0, 0, $Size, $Size)
            } finally { $g.Dispose() }

            $ms = New-Object System.IO.MemoryStream
            try {
                $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
                return $ms.ToArray()
            } finally { $ms.Dispose() }
        } finally { $bmp.Dispose() }
    } finally { $source.Dispose() }
}

$embed = $Sizes | Sort-Object -Unique | Where-Object { $_ -le 256 }
foreach ($skipped in ($Sizes | Where-Object { $_ -gt 256 })) {
    Write-Warning "skipping ${skipped}px: ICO entries cannot exceed 256px"
}

Write-Host "packing $Stem.ico"

# Cast each blob back to byte[]: PowerShell unrolls an array returned from a
# function, so it arrives here as an object[] of individual bytes.
$images = New-Object 'System.Collections.Generic.List[byte[]]'
foreach ($size in $embed) {
    [byte[]] $bytes = Get-PngBytes -Size $size
    $images.Add($bytes)
}

$stream = [System.IO.File]::Create($out)
$writer = New-Object System.IO.BinaryWriter $stream
try {
    # ICONDIR: reserved, type (1 = icon), image count.
    $writer.Write([uint16] 0)
    $writer.Write([uint16] 1)
    $writer.Write([uint16] $images.Count)

    # Blobs begin after the directory; each ICONDIRENTRY is 16 bytes.
    $offset = 6 + (16 * $images.Count)
    for ($i = 0; $i -lt $images.Count; $i++) {
        $size = $embed[$i]
        $bytes = $images[$i]

        # A 256px edge is encoded as 0 — the field is one byte wide.
        $edge = if ($size -eq 256) { 0 } else { $size }
        $writer.Write([byte] $edge)          # width
        $writer.Write([byte] $edge)          # height
        $writer.Write([byte] 0)              # palette entries (0 = truecolor)
        $writer.Write([byte] 0)              # reserved
        $writer.Write([uint16] 1)            # colour planes
        $writer.Write([uint16] 32)           # bits per pixel
        $writer.Write([uint32] $bytes.Length)
        $writer.Write([uint32] $offset)
        $offset += $bytes.Length
    }

    foreach ($bytes in $images) { $writer.Write($bytes) }
} finally {
    $writer.Dispose()
    $stream.Dispose()
}

Write-Host ("wrote {0} ({1:N0} bytes, {2} entries)" -f $out, (Get-Item $out).Length, $images.Count)
