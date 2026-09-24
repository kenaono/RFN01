# `ui/icon.ico`を作る（追加要件 C3、2026-09-24）。
#
# **このスクリプトはビルドの一部ではない。**絵を差し替えるときに手で1回動かし、
# 出来た`ui/icon.ico`をリポジトリへ入れる。ビルド側はその`.ico`を読むだけなので、
# 書き手の機械にPowerShellと.NETが無くてもビルドは通る。
#
# 絵は2種類を使い分ける。
#
# * **40px以上**は`ui/icon.png`をそのまま縮小する。細い線と丸い端の書き味が
#   そのまま出る大きさで、いまの絵の良さはここにある。
# * **32px以下**は簡略版を描く。元の絵は画の太さが16pxで1.06pxしかなく、
#   縮小すると`RE`が潰れる。簡略版は16単位の格子で、画を2px（16pxのとき）に
#   太らせ、文字を大きくし、Rの脚を長く、Eの3本の横棒の間隔を2pxに揃える。
#   下地（紫の角丸）は元の絵から文字だけを消して使うので、輪郭は変わらない。
#
# 使い方: `pwsh -File ui/make-icon.ps1`
param(
    [string]$Source = (Join-Path $PSScriptRoot 'icon.png'),
    [string]$Out = (Join-Path $PSScriptRoot 'icon.ico')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# 元の絵の色。下地の紫と、文字のアイボリー。
$purple = [System.Drawing.Color]::FromArgb(255, 87, 46, 157)
$ivory = [System.Drawing.Color]::FromArgb(255, 252, 249, 239)

# 簡略版を描く大きさと、元の絵を縮小する大きさ。**40pxが境目**。
$simplified = @(16, 20, 24, 32)
$scaled = @(40, 48, 64, 96, 128, 256)

# 変数名は`$Source`と別にする（PowerShellは大文字小文字を区別しないため、
# `$source`へ代入すると文字列のままに戻ってしまう）。
$art = [System.Drawing.Bitmap]::FromFile($Source)

# 文字を消した下地。文字の外接矩形（256pxで x39..216, y72..183）より少し広く塗る。
# 元の絵の下地は縁のアンチエイリアス以外は平らな紫なので、塗りつぶしても模様は消えない。
$flat = [System.Drawing.Bitmap]::new(256, 256)
$flatGraphics = [System.Drawing.Graphics]::FromImage($flat)
$flatGraphics.DrawImage($art, 0, 0)
$flatGraphics.FillRectangle([System.Drawing.SolidBrush]::new($purple), 32, 64, 192, 128)
$flatGraphics.Dispose()

function New-Background {
    param([int]$Size)
    $bmp = [System.Drawing.Bitmap]::new($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bmp)
    $graphics.Clear([System.Drawing.Color]::Transparent)
    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $graphics.DrawImage($flat, [System.Drawing.Rectangle]::new(0, 0, $Size, $Size))
    $graphics.Dispose()
    return $bmp
}

function New-Original {
    param([int]$Size)
    $bmp = [System.Drawing.Bitmap]::new($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bmp)
    $graphics.Clear([System.Drawing.Color]::Transparent)
    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $graphics.DrawImage($art, [System.Drawing.Rectangle]::new(0, 0, $Size, $Size))
    $graphics.Dispose()
    return $bmp
}

# 16単位の格子。単位は画面の画素そのもの（16pxのとき）。
function New-Simplified {
    param([int]$Size)
    $k = $Size / 16.0
    $stroke = 2.0 * $k      # 画の太さ
    $top = 3.0 * $k          # 文字の上端
    $height = 10.0 * $k      # 文字の高さ
    $hole = 2.0 * $k         # Rの椀の空き
    $barGap = 2.0 * $k       # Eの横棒の間隔
    $rLeft = 2.0 * $k
    $rWidth = 6.0 * $k
    $eLeft = 9.0 * $k
    $eWidth = 5.0 * $k

    $bmp = New-Background -Size $Size
    $graphics = [System.Drawing.Graphics]::FromImage($bmp)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $ink = [System.Drawing.SolidBrush]::new($ivory)

    # R: 縦棒、上の横棒、椀の右側、椀の下の横棒、脚。
    $graphics.FillRectangle($ink, [single]$rLeft, [single]$top, [single]$stroke, [single]$height)
    $graphics.FillRectangle($ink, [single]$rLeft, [single]$top, [single]$rWidth, [single]$stroke)
    $graphics.FillRectangle($ink, [single]($rLeft + $rWidth - $stroke), [single]$top, [single]$stroke, [single]($stroke + $hole))
    $graphics.FillRectangle($ink, [single]$rLeft, [single]($top + $stroke + $hole), [single]$rWidth, [single]$stroke)
    # 脚は椀の下辺から基線まで。上端を画2つぶん、下端をRの右端に合わせる。
    $legTop = $top + 2 * $stroke + $hole
    $leg = [System.Drawing.PointF[]]@(
        [System.Drawing.PointF]::new([single]($rLeft + $stroke), [single]$legTop),
        [System.Drawing.PointF]::new([single]($rLeft + 2 * $stroke), [single]$legTop),
        [System.Drawing.PointF]::new([single]($rLeft + $rWidth), [single]($top + $height)),
        [System.Drawing.PointF]::new([single]($rLeft + $rWidth - $stroke), [single]($top + $height))
    )
    $graphics.FillPolygon($ink, $leg)

    # E: 縦棒と3本の横棒。間隔は上の椀の下の横棒と同じ高さになる。
    $graphics.FillRectangle($ink, [single]$eLeft, [single]$top, [single]$stroke, [single]$height)
    $graphics.FillRectangle($ink, [single]$eLeft, [single]$top, [single]$eWidth, [single]$stroke)
    $graphics.FillRectangle($ink, [single]$eLeft, [single]($top + $stroke + $barGap), [single]$eWidth, [single]$stroke)
    $graphics.FillRectangle($ink, [single]$eLeft, [single]($top + $height - $stroke), [single]$eWidth, [single]$stroke)

    $ink.Dispose()
    $graphics.Dispose()
    return $bmp
}

# 32bppのBMP（DIB）として書く。小さなサイズはこの形が古い道具にも通る。
function Write-Dib {
    param([System.Drawing.Bitmap]$Bitmap, [System.IO.BinaryWriter]$Writer)
    $size = $Bitmap.Width
    $height = $Bitmap.Height
    $data = $Bitmap.LockBits(
        [System.Drawing.Rectangle]::new(0, 0, $size, $height),
        [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $buffer = New-Object byte[] ($data.Stride * $height)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buffer, 0, $buffer.Length)
    $Bitmap.UnlockBits($data)

    # BITMAPINFOHEADER。高さはXORとANDの2枚ぶんなので2倍。
    $Writer.Write([uint32]40)
    $Writer.Write([int32]$size)
    $Writer.Write([int32]($height * 2))
    $Writer.Write([uint16]1)
    $Writer.Write([uint16]32)
    $Writer.Write([uint32]0)
    $Writer.Write([uint32]($size * $size * 4))
    $Writer.Write([int32]0)
    $Writer.Write([int32]0)
    $Writer.Write([uint32]0)
    $Writer.Write([uint32]0)

    # 画素は下の行から。BGRAの並びはBitmapのまま使える。
    for ($y = $height - 1; $y -ge 0; $y--) {
        $Writer.Write($buffer, $y * $data.Stride, $size * 4)
    }

    # ANDマスク。32bppでは使われないので0でよい。1行は4バイトの倍数。
    $maskRow = [int][Math]::Ceiling($size / 8.0)
    $maskRow += (4 - ($maskRow % 4)) % 4
    $zeros = New-Object byte[] $maskRow
    for ($y = 0; $y -lt $height; $y++) {
        $Writer.Write($zeros)
    }
}

function Get-IconBytes {
    param([System.Drawing.Bitmap]$Bitmap, [bool]$AsPng)
    $stream = [System.IO.MemoryStream]::new()
    if ($AsPng) {
        $Bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    } else {
        $writer = [System.IO.BinaryWriter]::new($stream)
        Write-Dib -Bitmap $Bitmap -Writer $writer
        $writer.Flush()
        $writer.Dispose()
    }
    $bytes = $stream.ToArray()
    $stream.Dispose()
    # **`,`で包む。**そのまま返すとPowerShellが1要素ずつ並べ直し、
    # 呼び出し側で`Object[]`になり、`BinaryWriter.Write`が別の多重定義を選ぶ。
    return , ([byte[]]$bytes)
}

$images = @()
foreach ($size in $simplified) {
    $bitmap = New-Simplified -Size $size
    # 96px以上はPNGで入れる（Vista以降の普通の形）。
    $images += [pscustomobject]@{ Size = $size; Bytes = (Get-IconBytes -Bitmap $bitmap -AsPng $false) }
    $bitmap.Dispose()
}
foreach ($size in $scaled) {
    $bitmap = New-Original -Size $size
    $images += [pscustomobject]@{ Size = $size; Bytes = (Get-IconBytes -Bitmap $bitmap -AsPng ($size -ge 96)) }
    $bitmap.Dispose()
}
$images = $images | Sort-Object -Property Size

$stream = [System.IO.File]::Create($Out)
$writer = [System.IO.BinaryWriter]::new($stream)
$writer.Write([uint16]0)          # reserved
$writer.Write([uint16]1)          # type: icon
$writer.Write([uint16]$images.Count)

$offset = 6 + 16 * $images.Count
foreach ($image in $images) {
    $dimension = if ($image.Size -ge 256) { 0 } else { $image.Size }
    $writer.Write([byte]$dimension)
    $writer.Write([byte]$dimension)
    $writer.Write([byte]0)         # palette
    $writer.Write([byte]0)         # reserved
    $writer.Write([uint16]1)       # planes
    $writer.Write([uint16]32)      # bits per pixel
    $writer.Write([uint32]$image.Bytes.Length)
    $writer.Write([uint32]$offset)
    $offset += $image.Bytes.Length
}
foreach ($image in $images) {
    $writer.Write($image.Bytes)
}
$writer.Flush()
$writer.Dispose()
$stream.Dispose()

$flat.Dispose()
$art.Dispose()
Write-Output ("wrote {0}: {1} sizes, {2} bytes" -f $Out, $images.Count, (Get-Item -LiteralPath $Out).Length)
