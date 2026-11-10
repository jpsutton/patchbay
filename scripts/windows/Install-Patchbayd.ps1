<#
.SYNOPSIS
  Install or update patchbayd on Windows through winget, from the manifest Patchbay publishes on
  GitHub Pages (pb-20261110-14). Run it again later to update. Nothing is submitted to Microsoft.
.DESCRIPTION
  One-time, as administrator:   winget settings --enable LocalManifestFiles
  winget cannot replace a running exe, so stop patchbayd first, or pass -StopRunning.
  Your chats, sessions and token live in the data folder, not the install folder: they are kept.
  If the desktop app runs on this computer against the same data folder, update it to the same
  version: the app and the daemon must match.
#>
[CmdletBinding()]
param(
  [string]$ManifestUrl = 'https://jpsutton.github.io/patchbay/winget/jpsutton.patchbayd.yaml',
  [string]$ManifestPath,   # a manifest file on this computer: skips the download
  [switch]$StopRunning
)
$ErrorActionPreference = 'Stop'
$id = 'jpsutton.patchbayd'

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) { throw 'winget was not found (it ships with Windows 11 and current Windows 10 as App Installer).' }

$running = Get-Process patchbayd -ErrorAction SilentlyContinue
if ($running) {
  if (-not $StopRunning) { throw 'patchbayd is running. Stop it, or run this script again with -StopRunning.' }
  $running | Stop-Process -Force
}

if ($ManifestPath) {
  $manifest = (Resolve-Path -LiteralPath $ManifestPath).Path
} else {
  $manifest = Join-Path ([IO.Path]::GetTempPath()) "$id.yaml"
  Invoke-WebRequest -UseBasicParsing -Uri $ManifestUrl -OutFile $manifest
}

winget list --id $id --exact --accept-source-agreements *> $null
$verb = if ($LASTEXITCODE -eq 0) { 'upgrade' } else { 'install' }
winget $verb --manifest $manifest --accept-package-agreements --accept-source-agreements
if ($LASTEXITCODE -ne 0) {
  throw "winget $verb failed (exit $LASTEXITCODE). If it says local manifests are disabled, run once as administrator: winget settings --enable LocalManifestFiles"
}
Write-Host 'Done. Open a new terminal, then run: patchbayd --version'
