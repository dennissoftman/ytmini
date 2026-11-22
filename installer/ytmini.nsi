; ytmini's Windows installer (NSIS 3, built on macOS by scripts/build_windows.sh).
; Per-user install: no admin prompt, goes to %LOCALAPPDATA%\Programs\ytmini.
; Run again over an existing install and it offers to update it in place
; (settings and sign-in live in %APPDATA%\ytmini and are left alone).
; Defines passed in: VERSION (0.1.0), BUILD (git hash), STAGE (folder with
; ytmini.exe, libmpv-2.dll, license files), ROOT (repo), OUTFILE.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
ManifestDPIAware true

!define APP "ytmini"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"

Name "${APP}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP}"
BrandingText "${APP} ${VERSION} (${BUILD})"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=0 "ProductName" "${APP}"
VIAddVersionKey /LANG=0 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "FileDescription" "${APP} setup"
VIAddVersionKey /LANG=0 "LegalCopyright" "The ytmini authors"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "nsDialogs.nsh"

Var OldVersion   ; installed version, empty on a fresh install
Var WelcomeTitle
Var WelcomeText
Var HadShortcut

!define MUI_ICON "${ROOT}\assets\icon.ico"
!define MUI_UNICON "${ROOT}\assets\icon.ico"
!define MUI_ABORTWARNING

!define MUI_WELCOMEPAGE_TITLE "$WelcomeTitle"
!define MUI_WELCOMEPAGE_TEXT "$WelcomeText"
!define MUI_PAGE_CUSTOMFUNCTION_SHOW WelcomeShow
!insertmacro MUI_PAGE_WELCOME

!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipOnUpdate
!insertmacro MUI_PAGE_DIRECTORY

!insertmacro MUI_PAGE_INSTFILES

!define MUI_FINISHPAGE_RUN "$INSTDIR\ytmini.exe"
; the "show readme" box, used for the desktop shortcut
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_TEXT "$(DesktopShortcut)"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION MakeDesktopShortcut
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishShow
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; the system UI language picks one of these (English otherwise)
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "German"
!insertmacro MUI_LANGUAGE "Spanish"
!insertmacro MUI_LANGUAGE "French"
!insertmacro MUI_LANGUAGE "Italian"
!insertmacro MUI_LANGUAGE "Japanese"
!insertmacro MUI_LANGUAGE "Korean"
!insertmacro MUI_LANGUAGE "Polish"
!insertmacro MUI_LANGUAGE "PortugueseBR"
!insertmacro MUI_LANGUAGE "Russian"
!insertmacro MUI_LANGUAGE "Turkish"
!insertmacro MUI_LANGUAGE "Ukrainian"
!insertmacro MUI_LANGUAGE "SimpChinese"

!include "${ROOT}\installer\strings.nsh"

; ---------------------------------------------------------------- install

Function .onInit
    ReadRegStr $OldVersion HKCU "${UNINST_KEY}" "DisplayVersion"
    ReadRegStr $0 HKCU "${UNINST_KEY}" "InstallLocation"
    ${If} $OldVersion != ""
    ${AndIf} $0 != ""
        StrCpy $INSTDIR $0
        StrCpy $WelcomeTitle "$(UpdateTitle)"
        StrCpy $WelcomeText "$(UpdateText)"
    ${Else}
        StrCpy $OldVersion ""
        StrCpy $WelcomeTitle "$(MUI_TEXT_WELCOME_INFO_TITLE)"
        StrCpy $WelcomeText "$(MUI_TEXT_WELCOME_INFO_TEXT)"
    ${EndIf}
    StrCpy $HadShortcut 0
    ${If} ${FileExists} "$DESKTOP\${APP}.lnk"
        StrCpy $HadShortcut 1
    ${EndIf}
FunctionEnd

Function WelcomeShow
    ${If} $OldVersion != ""
        GetDlgItem $0 $HWNDPARENT 1
        SendMessage $0 ${WM_SETTEXT} 0 "STR:$(UpdateButton)"
    ${EndIf}
FunctionEnd

Function SkipOnUpdate
    ${If} $OldVersion != ""
        Abort
    ${EndIf}
FunctionEnd

Function FinishShow
    ; updating someone who never wanted a desktop shortcut: don't push one
    ${If} $OldVersion != ""
    ${AndIf} $HadShortcut == 0
        ${NSD_Uncheck} $mui.FinishPage.ShowReadme
    ${EndIf}
FunctionEnd

Function MakeDesktopShortcut
    CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\ytmini.exe"
FunctionEnd

; ytmini.exe can't be replaced while it runs: offer to close it.
!macro CloseApp un
Function ${un}CloseApp
    retry:
    ${IfNot} ${FileExists} "$INSTDIR\ytmini.exe"
        Return
    ${EndIf}
    ClearErrors
    FileOpen $0 "$INSTDIR\ytmini.exe" a
    ${IfNot} ${Errors}
        FileClose $0
        Return
    ${EndIf}
    MessageBox MB_OKCANCEL|MB_ICONINFORMATION "$(AppRunning)" /SD IDOK IDOK close
    Abort
    close:
    nsExec::Exec 'taskkill /IM ytmini.exe'
    Pop $0
    Sleep 2000
    ClearErrors
    FileOpen $0 "$INSTDIR\ytmini.exe" a
    ${IfNot} ${Errors}
        FileClose $0
        Return
    ${EndIf}
    nsExec::Exec 'taskkill /F /IM ytmini.exe'
    Pop $0
    Sleep 1000
    ClearErrors
    FileOpen $0 "$INSTDIR\ytmini.exe" a
    ${IfNot} ${Errors}
        FileClose $0
        Return
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "$(StillRunning)" /SD IDCANCEL IDRETRY retry
    Abort
FunctionEnd
!macroend
!insertmacro CloseApp ""
!insertmacro CloseApp "un."

Section "ytmini" Main
    SectionIn RO
    Call CloseApp
    SetOutPath "$INSTDIR"
    File "${STAGE}\ytmini.exe"
    File "${STAGE}\libmpv-2.dll"
    File "${STAGE}\LICENSE.md"
    File "${STAGE}\NOTICE.md"
    File "${STAGE}\GPL-3.0.txt"
    File "${STAGE}\libmpv-source.txt"
    WriteUninstaller "$INSTDIR\uninstall.exe"

    CreateShortcut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\ytmini.exe"
    ; keep an existing desktop shortcut pointing at this install
    ${If} $HadShortcut == 1
        CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\ytmini.exe"
    ${EndIf}

    WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP}"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION} (${BUILD})"
    WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "The ytmini authors"
    WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\ytmini.exe"
    WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
    SectionGetSize ${Main} $0
    WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" $0
SectionEnd

; ---------------------------------------------------------------- uninstall

Section "Uninstall"
    Call un.CloseApp
    Delete "$INSTDIR\ytmini.exe"
    Delete "$INSTDIR\libmpv-2.dll"
    Delete "$INSTDIR\LICENSE.md"
    Delete "$INSTDIR\NOTICE.md"
    Delete "$INSTDIR\GPL-3.0.txt"
    Delete "$INSTDIR\libmpv-source.txt"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"
    Delete "$SMPROGRAMS\${APP}.lnk"
    Delete "$DESKTOP\${APP}.lnk"
    DeleteRegKey HKCU "${UNINST_KEY}"
    RMDir /r "$LOCALAPPDATA\${APP}" ; thumbnails and other caches

    ; sign-in cookies, settings, yt-dlp/deno: kept unless asked (silent: kept)
    ${If} ${FileExists} "$APPDATA\${APP}\*.*"
        MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "$(RemoveData)" /SD IDNO IDNO keep
        RMDir /r "$APPDATA\${APP}"
        keep:
    ${EndIf}
SectionEnd
