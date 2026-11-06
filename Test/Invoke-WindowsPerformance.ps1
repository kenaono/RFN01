#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) {
    throw 'Run this performance suite on Windows.'
}

$repo = Split-Path -Parent $PSScriptRoot
$results = Join-Path $repo 'target/editor-perf/results'
[void][System.IO.Directory]::CreateDirectory($results)
$runId = '{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), [guid]::NewGuid().ToString('N').Substring(0, 8)
$log = Join-Path $results "windows-$runId.log"

& (Join-Path $PSScriptRoot 'Generate-EditorPerfFixtures.ps1')
Push-Location -LiteralPath $repo
try {
    $header = @("Windows $([Environment]::OSVersion.Version), $([Environment]::ProcessorCount) logical processors")
    $git = Get-Command git -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($git) {
        $safeDirectory = 'safe.directory=' + $repo.Replace('\', '/')
        $revision = & $git.Source -c $safeDirectory rev-parse --verify HEAD 2>$null
        if ($LASTEXITCODE -eq 0) {
            $changed = & $git.Source -c $safeDirectory status --porcelain 2>$null
            $tree = if ($LASTEXITCODE -ne 0) { 'unknown' } elseif ($changed) { 'dirty' } else { 'clean' }
            $header += "revision=$revision tree=$tree"
        }
    }
    foreach ($name in 'many-paragraphs.md', 'long-paragraph.md') {
        $fixture = Join-Path $repo "target/editor-perf/fixtures/$name"
        $hash = (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash
        $header += "fixture=$name sha256=$hash"
    }
    Set-Content -LiteralPath $log -Value $header -Encoding utf8
    & cargo test --release --locked --offline windows_performance -- --ignored --nocapture --test-threads=1 2>&1 |
        ForEach-Object { $_.ToString() } |
        Tee-Object -FilePath $log -Append
    $result = $LASTEXITCODE
} finally {
    Pop-Location
}

Write-Host "Results: $log"
if ($result -ne 0) {
    exit $result
}

$cases = @(
    'editor_many_paragraphs', 'editor_long_paragraph', 'terminal_plain_band',
    'terminal_mixed_band', 'terminal_mixed_viewport'
)
foreach ($case in $cases) {
    if (-not (Select-String -LiteralPath $log -SimpleMatch "perf case=$case metric=" -Quiet)) {
        throw "The test run did not report samples for $case."
    }
}
