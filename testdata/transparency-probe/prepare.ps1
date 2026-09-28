$ErrorActionPreference = 'Stop'
$editorRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$cargoRoot = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$source = @(Get-ChildItem (Join-Path $cargoRoot 'registry/src') -Directory | ForEach-Object {
    Join-Path $_.FullName 'i-slint-backend-winit-1.17.1'
} | Where-Object { Test-Path $_ })
if ($source.Count -ne 1) { throw 'Expected exactly one cached Slint 1.17.1 winit backend.' }
$vendor = Join-Path $editorRoot 'target/transparency-probe/backend-winit'
if (-not (Test-Path $vendor)) {
    New-Item -ItemType Directory -Force -Path $vendor | Out-Null
    Copy-Item -Path (Join-Path $source[0] '*') -Destination $vendor -Recurse
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'sw_layered.rs') -Destination (Join-Path $vendor 'renderer/sw.rs')
# Patch resolution is confined to this command. The editor and global Cargo cache are unchanged.
$patch = 'patch.crates-io.i-slint-backend-winit.path="' + $vendor.Replace('\', '/') + '"'
& cargo build --offline --release --manifest-path (Join-Path $PSScriptRoot 'Cargo.toml') --target-dir (Join-Path $editorRoot 'target') --config $patch
if ($LASTEXITCODE -ne 0) { throw "Probe build failed: $LASTEXITCODE" }
