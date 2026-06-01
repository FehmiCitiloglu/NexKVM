Param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

New-Item -ItemType Directory -Path target/package -Force | Out-Null

Write-Host "Building release binary..."
cargo build -p coklu --release

if (-not (Get-Command makensis -ErrorAction SilentlyContinue)) {
  throw "makensis not found. Install NSIS (e.g. choco install nsis -y)."
}

Write-Host "Building NSIS installer..."
makensis packaging/windows/installer.nsi | Out-Host

Write-Host "Windows package ready: target/package/coklu-windows-x64-setup.exe"
