# Genera assets\app.ico (16..256 px, PNG dentro l'ICO) con System.Drawing.
Add-Type -AssemblyName System.Drawing
$out = Join-Path $PSScriptRoot 'app.ico'

function Draw([int]$s) {
    $bmp = New-Object Drawing.Bitmap $s, $s, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([Drawing.Color]::Transparent)
    $k = $s / 256.0

    # quadrato arrotondato con sfumatura viola
    $r = 56 * $k; $m = 8 * $k; $w = $s - 2 * $m
    $path = New-Object Drawing.Drawing2D.GraphicsPath
    $path.AddArc($m, $m, 2 * $r, 2 * $r, 180, 90)
    $path.AddArc($m + $w - 2 * $r, $m, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($m + $w - 2 * $r, $m + $w - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $path.AddArc($m, $m + $w - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure()
    $grad = New-Object Drawing.Drawing2D.LinearGradientBrush ([Drawing.PointF]::new(0, 0)), ([Drawing.PointF]::new($s, $s)),
        ([Drawing.Color]::FromArgb(255, 0x9A, 0x8C, 0xFF)), ([Drawing.Color]::FromArgb(255, 0x4B, 0x3A, 0xD8))
    $g.FillPath($grad, $path)

    # triangolo play
    $white = New-Object Drawing.SolidBrush ([Drawing.Color]::White)
    $tri = [Drawing.PointF[]]@(
        [Drawing.PointF]::new(92 * $k, 70 * $k),
        [Drawing.PointF]::new(92 * $k, 190 * $k),
        [Drawing.PointF]::new(190 * $k, 130 * $k))
    $g.FillPolygon($white, $tri)

    # scintilla in alto a destra (il "neural")
    if ($s -ge 32) {
        $cx = 196 * $k; $cy = 62 * $k; $a = 34 * $k; $b = 9 * $k
        $star = [Drawing.PointF[]]@(
            [Drawing.PointF]::new($cx, $cy - $a), [Drawing.PointF]::new($cx + $b, $cy - $b),
            [Drawing.PointF]::new($cx + $a, $cy), [Drawing.PointF]::new($cx + $b, $cy + $b),
            [Drawing.PointF]::new($cx, $cy + $a), [Drawing.PointF]::new($cx - $b, $cy + $b),
            [Drawing.PointF]::new($cx - $a, $cy), [Drawing.PointF]::new($cx - $b, $cy - $b))
        $g.FillPolygon($white, $star)
    }
    $g.Dispose()
    $ms = New-Object IO.MemoryStream
    $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}

$sizes = 16, 24, 32, 48, 64, 128, 256
$images = New-Object System.Collections.Generic.List[byte[]]
foreach ($s in $sizes) { $images.Add([byte[]](Draw $s)) }
$fs = [IO.File]::Create($out)
$bw = New-Object IO.BinaryWriter $fs
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $len = $images[$i].Length
    $bw.Write([byte]($s % 256)); $bw.Write([byte]($s % 256)); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32); $bw.Write([UInt32]$len); $bw.Write([UInt32]$offset)
    $offset += $len
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Close()
"scritto $out"

