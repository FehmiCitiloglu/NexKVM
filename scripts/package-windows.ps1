Param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

New-Item -ItemType Directory -Path target/package -Force | Out-Null

$logoPng = Join-Path $root "packaging/assets/coklu-logo.png"
$iconIco = Join-Path $root "packaging/windows/coklu.ico"

if (-not (Test-Path $logoPng)) {
  throw "Logo not found at $logoPng"
}

if (-not (Get-Command magick -ErrorAction SilentlyContinue)) {
 echo "magicki not found nstalling imagemagick"
  winget install ImageMagick.ImageMagick
 # throw "ImageMagick 'magick' not found. Install it (e.g. choco install imagemagick -y or winget install ImageMagick.ImageMagick)."
}

Write-Host "Generating installer icon from logo..."
magick convert $logoPng -background none -define icon:auto-resize=256,128,64,48,32,16 $iconIco

Write-Host "Building release binary..."
cargo build -p coklu --release

if (-not (Get-Command makensis -ErrorAction SilentlyContinue)) {
 echo "makensis not found. Installing NSIS"
  winget install NSIS.NSIS
  #throw "makensis not found. Install NSIS (e.g. choco install nsis -y or winget install NSIS.NSIS)."
}

Write-Host "Building NSIS installer..."
makensis packaging/windows/installer.nsi | Out-Host

Write-Host "Windows package ready: target/package/coklu-windows-x64-setup.exe"
