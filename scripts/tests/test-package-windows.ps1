$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$packageScript = Get-Content -Raw (Join-Path $root "scripts/package-windows.ps1")
$installer = Get-Content -Raw (Join-Path $root "packaging/windows/installer.nsi")
$pathUpdater = Join-Path $root "packaging/windows/update-user-path.ps1"

function Assert-Match {
    param(
        [Parameter(Mandatory)]
        [string] $Text,
        [Parameter(Mandatory)]
        [string] $Pattern,
        [Parameter(Mandatory)]
        [string] $Message
    )

    if ($Text -notmatch $Pattern) {
        throw $Message
    }
}

function Assert-Contains {
    param(
        [Parameter(Mandatory)]
        [string] $Text,
        [Parameter(Mandatory)]
        [string] $Expected,
        [Parameter(Mandatory)]
        [string] $Message
    )

    if (-not $Text.Contains($Expected, [StringComparison]::Ordinal)) {
        throw $Message
    }
}

Assert-Match $packageScript `
    'cargo build --locked -p nexkvm -p nexkvm-gui --release' `
    "Windows packaging must build the daemon/CLI and GUI release binaries together."
Assert-Contains $installer `
    'File "..\..\target\release\${CLI_EXE_NAME}"' `
    "The installer must include the nexkvm CLI/daemon executable."
Assert-Contains $installer `
    'File "..\..\target\release\${GUI_EXE_NAME}"' `
    "The installer must include the NexKVM GUI executable."
Assert-Contains $installer `
    'CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${GUI_EXE_NAME}"' `
    "The Start Menu application shortcut must launch the GUI."
Assert-Contains $installer `
    'Call AddToUserPath' `
    "The installer must add its directory to the user PATH."
Assert-Contains $installer `
    'Call un.RemoveFromUserPath' `
    "The uninstaller must remove its directory from the user PATH."
Assert-Contains $installer `
    'Delete "$INSTDIR\${GUI_EXE_NAME}"' `
    "The uninstaller must remove the GUI executable."

$originalProcessPath = [Environment]::GetEnvironmentVariable("Path", "Process")
try {
    [Environment]::SetEnvironmentVariable("Path", "C:\Tools;C:\Other;C:\\NexKVM", "Process")
    & $pathUpdater -Action Add -Directory "C:\NexKVM" -Target Process
    & $pathUpdater -Action Add -Directory "c:\nexkvm\" -Target Process
    if ($env:Path -ne "C:\Tools;C:\Other;C:\NexKVM") {
        throw "PATH addition must be case-insensitive and idempotent; got '$env:Path'."
    }

    & $pathUpdater -Action Remove -Directory "C:\NEXKVM" -Target Process
    if ($env:Path -ne "C:\Tools;C:\Other") {
        throw "PATH removal must preserve unrelated entries; got '$env:Path'."
    }
} finally {
    [Environment]::SetEnvironmentVariable("Path", $originalProcessPath, "Process")
}

Write-Host "Windows package source checks passed."
