; NSIS installer for nexkvm desktop daemon (minimal MVP packaging slice).
; Builds a per-user installer for the GUI and daemon/CLI.

!include "MUI2.nsh"
!include "WinMessages.nsh"

!define APP_NAME "NexKVM"
!define COMPANY "nexkvm contributors"
!define CLI_EXE_NAME "nexkvm.exe"
!define GUI_EXE_NAME "nexkvm-gui.exe"
!define ICON_PATH "..\..\packaging\windows\nexkvm.ico"

Name "${APP_NAME}"
OutFile "..\..\target\package\nexkvm-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\nexkvm"
RequestExecutionLevel user

!define MUI_ICON "${ICON_PATH}"
!define MUI_UNICON "${ICON_PATH}"
Icon "${ICON_PATH}"
UninstallIcon "${ICON_PATH}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File "..\..\target\release\${CLI_EXE_NAME}"
  File "..\..\target\release\${GUI_EXE_NAME}"
  File "update-user-path.ps1"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${GUI_EXE_NAME}" "" "$INSTDIR\${GUI_EXE_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk" "$INSTDIR\Uninstall.exe"

  Call AddToUserPath
  WriteUninstaller "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
  Call un.RemoveFromUserPath
  Delete "$INSTDIR\${CLI_EXE_NAME}"
  Delete "$INSTDIR\${GUI_EXE_NAME}"
  Delete "$INSTDIR\update-user-path.ps1"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir /r "$SMPROGRAMS\${APP_NAME}"
  RMDir "$INSTDIR"
SectionEnd

Function AddToUserPath
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$INSTDIR\update-user-path.ps1" -Action Add -Directory "$INSTDIR"'
  Pop $0
  Pop $1
  StrCmp $0 "0" path_updated
  DetailPrint "Failed to add NexKVM to the user PATH: $1"
  Abort

path_updated:
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
FunctionEnd

Function un.RemoveFromUserPath
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$INSTDIR\update-user-path.ps1" -Action Remove -Directory "$INSTDIR"'
  Pop $0
  Pop $1
  StrCmp $0 "0" path_removed
  DetailPrint "Failed to remove NexKVM from the user PATH: $1"
  Return

path_removed:
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
FunctionEnd
