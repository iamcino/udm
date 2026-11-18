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
; (expects ..\..\target\x86_64-pc-windows-msvc\release\udm.exe, udm-cli.exe and udm-host.exe,
;  or set QUIET_EXE_DIR / see build.ps1 which stages dist\ first)

Unicode true
!define APPNAME        "udm"
!ifndef VERSION
!define VERSION        "0.2.4"
!endif
!define PUBLISHER      "Cino"
!define EXE            "udm.exe"
!define CLIEXE         "udm-cli.exe"
!define HOSTEXE        "udm-host.exe"

; Update trust pin: the installer never generates or fetches a pin.
; packaging/windows/build.ps1 reads the single source of truth
; (crates/core/src/updater.rs, PINNED_SPKI_SHA256_HEX - the SPKI
; SHA-256 of the code-signing key) and passes it as /DUDM_SPKI_PIN=... .
; A direct makensis run without that define fails here on purpose; no
; literal pin lives in this file.
!ifndef UDM_SPKI_PIN
!error "UDM_SPKI_PIN is required: build via packaging/windows/build.ps1, which reads it from crates/core/src/updater.rs"
!endif

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
VIAddVersionKey "LegalCopyright" "Copyright (c) 2026 Cino - MIT OR Apache-2.0"

Section "udm" SecMain
    SectionIn RO
    SetOutPath "$INSTDIR"

    ; If an older copy is running, ask politely instead of failing midway.
    File /oname=${EXE}        "..\..\dist\${EXE}"
    File /oname=${CLIEXE}     "..\..\dist\${CLIEXE}"
    File /oname=${HOSTEXE}    "..\..\dist\${HOSTEXE}"
    File /oname=LICENSE.txt   "..\..\LICENSE"
    File /oname=PRIVACY.md    "..\..\docs\PRIVACY.md"
    File /oname=CINO-UI-LICENSE.txt "..\..\crates\gui\vendor\cino-ui\LICENSE"
    File /oname=BE-VIETNAM-PRO-OFL.txt "..\..\crates\gui\vendor\cino-ui\ui\fonts\OFL.txt"

    ; Browser extension bundles (Settings > Browser points here): one
    ; self-contained folder per browser, staged by build.ps1 into dist.
    SetOutPath "$INSTDIR\extension\chrome"
    File /r "..\..\dist\extension\chrome\*.*"
    SetOutPath "$INSTDIR\extension\firefox"
    File /r "..\..\dist\extension\firefox\*.*"
    ; Firefox installs a file, not a folder: the bundled extension zip
    ; (the Firefox guide in Settings > Browser points at it).
    SetOutPath "$INSTDIR\extension"
    File "..\..\dist\extension\udm-extension-firefox-${VERSION}.zip"
    SetOutPath "$INSTDIR"

    ; First-trust record: the installer ships the same signing-key pin
    ; the binary was built with, so `udm-cli update pin` output can be
    ; compared against the installed copy. The pin is public.
    FileOpen $0 "$INSTDIR\update-pin.txt" w
    FileWrite $0 "signing_key_pin_spki_sha256=${UDM_SPKI_PIN}$\r$\n"
    FileClose $0

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

    ; Browser handoff sweep (item 5): unregister both browsers through
    ; the installed host while it still exists. That deletes exactly
    ; this build's host key, strips the browser's entries from
    ; <data>\handoff\callers.json (per-user data root, and the portable
    ; data folder when the host runs from one), and removes
    ; udm-host.caller when it names that browser. The two DeleteRegKey
    ; lines are the guarantee behind it: they remove the keys even if
    ; the host binary is missing or its unregister fails. Each key is
    ; the leaf host key itself - never a parent key, never another
    ; product's key. The journal, udm.redb and downloads are user data
    ; and stay behind, like the rest of %LOCALAPPDATA%\udm.
    IfFileExists "$INSTDIR\${HOSTEXE}" host_exists host_missing
    host_exists:
    ExecWait '"$INSTDIR\${HOSTEXE}" --unregister chrome'
    ExecWait '"$INSTDIR\${HOSTEXE}" --unregister firefox'
    host_missing:
    DeleteRegKey HKCU "Software\Google\Chrome\NativeMessagingHosts\pw.cino.udm.handoff.chrome"
    DeleteRegKey HKCU "Software\Mozilla\NativeMessagingHosts\pw.cino.udm.handoff.firefox"

    Delete "$INSTDIR\${EXE}"
    Delete "$INSTDIR\${CLIEXE}"
    Delete "$INSTDIR\${HOSTEXE}"
    Delete "$INSTDIR\udm-host.caller"
    Delete "$INSTDIR\LICENSE.txt"
    Delete "$INSTDIR\PRIVACY.md"
    Delete "$INSTDIR\CINO-UI-LICENSE.txt"
    Delete "$INSTDIR\BE-VIETNAM-PRO-OFL.txt"
    RMDir /r "$INSTDIR\extension"
    Delete "$INSTDIR\update-pin.txt"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    ; NOTE: user data (%LOCALAPPDATA%\udm) is deliberately left behind.
    ; Uninstalling the program must not delete your downloads or database.
SectionEnd
