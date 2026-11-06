#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('EditorLarge', 'EditorLong', 'Terminal')]
    [string]$Case,

    [string]$Executable
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) {
    throw 'Run this performance scenario on Windows.'
}

$repo = Split-Path -Parent $PSScriptRoot
if (-not $Executable) {
    $Executable = Join-Path $repo 'target/release/rfnedit.exe'
}
$source = Get-Item -LiteralPath $Executable
if ($source.PSIsContainer -or $source.Extension -ne '.exe') {
    throw 'Executable must be a Release build of rfnedit.exe.'
}

$runId = '{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), [guid]::NewGuid().ToString('N').Substring(0, 8)
$runDirectory = Join-Path $repo "target/editor-perf/ui/$Case-$runId"
$localData = Join-Path $runDirectory 'local-data'
$profile = Join-Path $localData 'RFN Edit'
[void][System.IO.Directory]::CreateDirectory($profile)
$app = Join-Path $runDirectory 'rfnedit.exe'
Copy-Item -LiteralPath $source.FullName -Destination $app

$settings = @('RFN-EDIT-SETTINGS 1', 'zoom.default: 120', 'work.autosave: 0')
$utf8 = [System.Text.UTF8Encoding]::new($false)
$inputFile = $null
if ($Case -eq 'Terminal') {
    $fixture = Join-Path $runDirectory 'rfn-terminal-perf-fixture.exe'
    & rustc --edition 2024 -O (Join-Path $PSScriptRoot 'performance/terminal_fixture.rs') -o $fixture
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not compile the Terminal output fixture.'
    }
    # The isolated session opens the fixture on startup, before any UI automation.
    $session = @(
        'RFN-EDIT-SESSION 1', 'layout: P 0', 'focused: 0', 'tree: 0',
        'pane: 0 120', 'tab: 1 0 0 0', 'shell: RFN Perf Fixture',
        ('shell-command: "{0}"' -f $fixture)
    )
    [System.IO.File]::WriteAllText(
        (Join-Path $profile 'session.rfnsession'),
        ($session -join "`n") + "`n",
        $utf8
    )
} else {
    & (Join-Path $PSScriptRoot 'Generate-EditorPerfFixtures.ps1')
    $name = if ($Case -eq 'EditorLarge') { 'many-paragraphs.md' } else { 'long-paragraph.md' }
    $inputFile = Join-Path $runDirectory $name
    Copy-Item -LiteralPath (Join-Path $repo "target/editor-perf/fixtures/$name") -Destination $inputFile
}
[System.IO.File]::WriteAllText(
    (Join-Path $profile 'settings.rfnsettings'),
    ($settings -join "`n") + "`n",
    $utf8
)

$launch = [System.Diagnostics.ProcessStartInfo]::new($app)
$launch.UseShellExecute = $false
$launch.WorkingDirectory = $runDirectory
$launch.Environment['LOCALAPPDATA'] = $localData
$launch.ArgumentList.Add('--diagnostics=render,terminal,perf')
if ($inputFile) {
    $launch.ArgumentList.Add('--')
    $launch.ArgumentList.Add($inputFile)
}
$process = [System.Diagnostics.Process]::Start($launch)
[pscustomobject]@{
    Case = $Case
    ProcessId = $process.Id
    Executable = $app
    RunDirectory = $runDirectory
    Logs = Join-Path $runDirectory 'diag'
    InputFile = $inputFile
}
