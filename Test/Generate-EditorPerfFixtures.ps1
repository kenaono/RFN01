#requires -Version 7.0
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$fixtureDirectory = Join-Path $root 'target/editor-perf/fixtures'
[void][IO.Directory]::CreateDirectory($fixtureDirectory)

# Keep the fixture bytes identical across runs and locale settings.
$utf8 = [Text.UTF8Encoding]::new($false)
$culture = [Globalization.CultureInfo]::InvariantCulture
$paragraphs = @(
    '夕方の駅では案内板の時刻が更新された。窓口の係員は雨で遅れた便を確かめ、次の列車を待つ人に乗換口を案内した。The departure board showed Platform 3 and a delay of 12 minutes.',
    '調査チームは観測値と現地の記録を照合した。数字が一致しない箇所には印を付け、測定器の調整を担当者に依頼した。The review notes describe sensor A-17, the 08:30 reading, and the follow-up check.',
    '港の倉庫には翌朝に積み込む荷物が並んだ。担当者は木箱の番号を名簿と見比べ、荷崩れしないように奥から順番を組み直した。Cargo sheet B lists the east gate, the driver, and the return route.',
    '会議では計画の変更とその影響を順番に確認した。前回の議事録を読み返し、判断が分かれた項目は資料を追加してから再検討することにした。The next review covers Option A, the schedule, and the budget.',
    '川沿いの道を歩くと、古い水門の向こうに低い雲が見えた。橋の上で地図を広げると、対岸の道は一つ先の交差点で街道に合流していた。The map marks a 4.5 km detour and two accessible crossings.',
    '作業を終えた技師は設定を保存し、点検表に結果を記録した。翌日の担当者が同じ手順を再現できるよう、使った条件と確認した範囲を書き添えた。Run R-42 used a local copy and the default view settings.'
)

function Write-EditorPerfFixture {
    param(
        [string]$Name,
        [string]$Heading,
        [int]$TargetBytes,
        [int]$MinimumFirstParagraphBytes = 0
    )

    $headingLine = "# $Heading`n`n"
    $firstParagraph = "RFN_PERF_EDIT_HERE $($paragraphs[0])"
    $content = [Text.StringBuilder]::new($headingLine)
    [void]$content.Append($firstParagraph)
    $firstParagraphBytes = $utf8.GetByteCount($firstParagraph)
    for ($index = 1; $firstParagraphBytes -lt $MinimumFirstParagraphBytes; $index++) {
        $number = $index.ToString('D4', $culture)
        $segment = " Record $number. $($paragraphs[$index % $paragraphs.Count])"
        [void]$content.Append($segment)
        $firstParagraphBytes += $utf8.GetByteCount($segment)
    }
    [void]$content.Append("`n`n")

    $bytes = $utf8.GetByteCount($headingLine) + $firstParagraphBytes + 2
    for ($index = 1; $bytes -lt $TargetBytes; $index++) {
        $number = $index.ToString('D4', $culture)
        $paragraph = "Note $number. $($paragraphs[$index % $paragraphs.Count])`n`n"
        [void]$content.Append($paragraph)
        $bytes += $utf8.GetByteCount($paragraph)
    }

    $path = Join-Path $fixtureDirectory $Name
    $text = $content.ToString()
    if (-not [IO.File]::Exists($path) -or [IO.File]::ReadAllText($path, $utf8) -cne $text) {
        # A parallel runner may open the fixture while this one generates it.
        $temporary = Join-Path $fixtureDirectory ([guid]::NewGuid().ToString('N') + '.tmp')
        try {
            [IO.File]::WriteAllText($temporary, $text, $utf8)
            [IO.File]::Move($temporary, $path, $true)
        } finally {
            [IO.File]::Delete($temporary)
        }
    }
    '{0} ({1} bytes)' -f $path, $bytes
}

Write-EditorPerfFixture -Name 'many-paragraphs.md' -Heading 'Many paragraphs' -TargetBytes 2MB
Write-EditorPerfFixture -Name 'long-paragraph.md' -Heading 'Long paragraph' -TargetBytes 200KB -MinimumFirstParagraphBytes 24KB
