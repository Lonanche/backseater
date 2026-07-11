param(
    [string]$Source = (Join-Path $PSScriptRoot "icon.png"),
    [string]$Out = (Join-Path $PSScriptRoot "icon.ico")
)
Add-Type -AssemblyName System.Drawing

$sizes = 16, 24, 32, 48, 64, 128, 256
$src = [System.Drawing.Image]::FromFile($Source)

# Render each size to an in-memory PNG (ICO supports PNG-compressed frames).
$frames = foreach ($s in $sizes) {
    $bmp = New-Object System.Drawing.Bitmap($s, $s)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.DrawImage($src, 0, 0, $s, $s)
    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    ,@{ Size = $s; Bytes = $ms.ToArray() }
}
$src.Dispose()

# Assemble the ICO: ICONDIR + one ICONDIRENTRY per frame + the PNG blobs.
$stream = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter($stream)
$w.Write([uint16]0)                # reserved
$w.Write([uint16]1)                # type: icon
$w.Write([uint16]$frames.Count)
$offset = 6 + 16 * $frames.Count
foreach ($f in $frames) {
    $dim = if ($f.Size -eq 256) { 0 } else { $f.Size }
    $w.Write([byte]$dim)           # width (0 = 256)
    $w.Write([byte]$dim)           # height
    $w.Write([byte]0)              # palette colors
    $w.Write([byte]0)              # reserved
    $w.Write([uint16]1)            # color planes
    $w.Write([uint16]32)           # bits per pixel
    $w.Write([uint32]$f.Bytes.Length)
    $w.Write([uint32]$offset)
    $offset += $f.Bytes.Length
}
foreach ($f in $frames) { $w.Write($f.Bytes) }
$w.Flush()
[System.IO.File]::WriteAllBytes($Out, $stream.ToArray())
$w.Dispose()
"wrote $Out ($((Get-Item $Out).Length) bytes, $($frames.Count) sizes)"
