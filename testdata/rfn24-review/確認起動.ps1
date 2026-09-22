$projectDirectory = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$previousAppData = $env:LOCALAPPDATA
try {
    $env:LOCALAPPDATA = Join-Path $projectDirectory 'testdata/rfn24-appdata'
    Start-Process -FilePath (Join-Path $projectDirectory 'target/rfn24-review/editor_spike.exe') -ArgumentList '--diagnostics=perf', (Join-Path $PSScriptRoot 'Review.md') -WindowStyle Hidden
} finally {
    $env:LOCALAPPDATA = $previousAppData
}
