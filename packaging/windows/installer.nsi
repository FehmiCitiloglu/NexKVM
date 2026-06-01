; NSIS installer for coklu desktop daemon (minimal MVP packaging slice).
; Builds a per-user installer that lays down the binary and Start Menu shortcut.

!include "MUI2.nsh"

!define APP_NAME "coklu"
!define COMPANY "coklu contributors"
!define EXE_NAME "coklu.exe"
!define ICON_PATH "..\\..\\packaging\\windows\\coklu.ico"

Name "${APP_NAME}"
OutFile "..\\..\\target\\package\\coklu-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\\${APP_NAME}"
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
  File "..\\..\\target\\release\\${EXE_NAME}"

  CreateDirectory "$SMPROGRAMS\\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\\${APP_NAME}\\${APP_NAME}.lnk" "$INSTDIR\\${EXE_NAME}" "" "$INSTDIR\\${EXE_NAME}"
  CreateShortcut "$SMPROGRAMS\\${APP_NAME}\\Uninstall ${APP_NAME}.lnk" "$INSTDIR\\Uninstall.exe"

  WriteUninstaller "$INSTDIR\\Uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\\${EXE_NAME}"
  Delete "$INSTDIR\\Uninstall.exe"
  RMDir /r "$SMPROGRAMS\\${APP_NAME}"
  RMDir "$INSTDIR"
SectionEnd
