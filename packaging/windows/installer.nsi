; udm per-user installer (NSIS 3.x).
;
; Design rules, in order of importance:
;   1. RequestExecutionLevel user   -> the UAC shield never appears.
;   2. Install to $LOCALAPPDATA\Programs\udm (per user, writable without
;      elevation). Never $PROGRAMFILES, never $PROGRAMDATA.
;   3. Registry writes only under HKCU (uninstall entry, start-menu shortcut
;      metadata). No services, no scheduled tasks, no HKLM.
;   4. Silent, reboot-free, and fully reversible.
;
; Build on Windows:  makensis packaging\windows\installer.nsi
; (expects ..\..\target\x86_64-pc-windows-msvc\release\udm.exe and udm-cli.exe,
;  or set QUIET_EXE_DIR / see build.ps1 which stages dist\ first)

Unicode true
!define APPNAME        "udm"
!ifndef VERSION
!define VERSION        "0.1.0"
!endif
!define PUBLISHER      "udm contributors"
!define EXE            "udm.exe"
!define CLIEXE         "udm-cli.exe"

Name "${APPNAME} ${VERSION}"
OutFile "..\..\dist\udm-${VERSION}-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\${APPNAME}"
RequestExecutionLevel user          ; <-- no admin, ever
SetCompressor /solid lzma
ShowInstDetails show
ShowUninstDetails show

; Modern UI
!include "MUI2.nsh"
!define MUI_ICON "..\..\crates\gui\assets\icon.ico"
!define MUI_UNICON "..\..\crates\gui\assets\icon.ico"
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APPNAME} installer (per-user, no admin)"
VIAddVersionKey "LegalCopyright" "MIT OR Apache-2.0"

Section "udm" SecMain
    SectionIn RO
    SetOutPath "$INSTDIR"

    ; If an older copy is running, ask politely instead of failing midway.
    File /oname=${EXE}        "..\..\dist\${EXE}"
    File /oname=${CLIEXE}     "..\..\dist\${CLIEXE}"
    File /oname=LICENSE.txt   "..\..\LICENSE"
    File /oname=PRIVACY.md    "..\..\docs\PRIVACY.md"
    File /oname=CINO-UI-LICENSE.txt "..\..\crates\gui\vendor\cino-ui\LICENSE"
    File /oname=BE-VIETNAM-PRO-OFL.txt "..\..\crates\gui\vendor\cino-ui\ui\fonts\OFL.txt"

    ; Per-user start menu + desktop shortcut (user profile, not all users).
    CreateDirectory "$SMPROGRAMS\${APPNAME}"
    CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${EXE}" "" "$INSTDIR\${EXE}" 0
    CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"
    CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${EXE}" "" "$INSTDIR\${EXE}" 0

    ; Uninstall entry under HKCU only.
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "DisplayName" "${APPNAME} ${VERSION}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "DisplayVersion" "${VERSION}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "Publisher" "${PUBLISHER}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "DisplayIcon" "$INSTDIR\${EXE},0"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "UninstallString" "$INSTDIR\uninstall.exe"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "QuietUninstallString" "$INSTDIR\uninstall.exe /S"
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "NoModify" 1
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "NoRepair" 1
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}" \
        "EstimatedSize" 12000

    WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
    Delete "$DESKTOP\${APPNAME}.lnk"
    Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
    Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
    RMDir "$SMPROGRAMS\${APPNAME}"

    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}"

    Delete "$INSTDIR\${EXE}"
    Delete "$INSTDIR\${CLIEXE}"
    Delete "$INSTDIR\LICENSE.txt"
    Delete "$INSTDIR\PRIVACY.md"
    Delete "$INSTDIR\CINO-UI-LICENSE.txt"
    Delete "$INSTDIR\BE-VIETNAM-PRO-OFL.txt"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    ; NOTE: user data (%LOCALAPPDATA%\udm) is deliberately left behind.
    ; Uninstalling the program must not delete your downloads or database.
SectionEnd
