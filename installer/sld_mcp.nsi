; SolidWorks MCP installer (NSIS 3).
;
; Installs the sld_mcp SOLIDWORKS add-in and the mcp_server.exe MCP server,
; registers the add-in with SOLIDWORKS, and optionally registers the server
; with Claude Desktop and Codex. Built by `.\build.ps1 -Target installer`,
; which passes these defines:
;
;   VERSION     version shown to the user (from mcp_server/Cargo.toml)
;   ADDIN_DLL   Release sld_mcp.dll
;   SERVER_EXE  Release mcp_server.exe
;   OUTFILE     setup .exe to write
;   VCREDIST    (optional) vc_redist.x64.exe from the Visual Studio that built
;               the binaries; installed first, since both link the CRT
;               dynamically and need a runtime at least as new as the toolset
;
; The SOLIDWORKS add-in registration (HKLM\SOFTWARE\SolidWorks\Addins\{CLSID}
; plus HKCU\Software\SolidWorks\AddInsStartup\{CLSID}) isn't tied to a
; release, so the same DLL loads in SOLIDWORKS 2025, 2026 and later, provided
; it was built against the oldest release's type libraries (see typelibs\).
;
; Per-user parts (the add-in's load-at-startup flag, the Claude Desktop and
; Codex configs) go to the account that runs the installer. Run it from your
; own account and approve the UAC prompt; installing with a different admin
; account's credentials sets them up for that account instead.

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel admin

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef ADDIN_DLL | SERVER_EXE | OUTFILE
  !error "Build with .\build.ps1 -Target installer (ADDIN_DLL, SERVER_EXE and OUTFILE must be defined)."
!endif

!define APP_NAME "SolidWorks MCP"
!define APP_KEY "sld_mcp"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_KEY}"
; Lowest SOLIDWORKS release the add-in is built for, and how far ahead to
; look for installed releases.
!define SW_MIN_YEAR 2025
!define SW_MAX_YEAR 2035

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "Sections.nsh"
!include "FileFunc.nsh"

Name "${APP_NAME}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP_NAME}"
; Remembered for upgrades. Kept in the 32-bit registry view, which is where
; InstallDirRegKey looks from this 32-bit installer.
InstallDirRegKey HKLM "Software\${APP_KEY}" "InstallDir"
BrandingText "${APP_NAME} ${VERSION}"
ShowInstDetails show
ShowUninstDetails show

Var SwReleases    ; installed SOLIDWORKS releases found, e.g. "2025, 2026"

; ------------------------------------------------------------------ pages ---

!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC

!define MUI_WELCOMEPAGE_TEXT "This installs the SolidWorks MCP add-in for SOLIDWORKS ${SW_MIN_YEAR} and newer, and the MCP server that lets Claude Desktop or Codex drive SOLIDWORKS through it.$\r$\n$\r$\nClose SOLIDWORKS, Claude Desktop and Codex before continuing.$\r$\n$\r$\n$_CLICK"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_TITLE "SolidWorks MCP is installed"
!define MUI_FINISHPAGE_TEXT "Start (or restart) SOLIDWORKS: the add-in loads automatically and appears under Tools > Add-ins as sld_mcp.$\r$\n$\r$\nFully quit and reopen Claude Desktop (including its tray icon) or Codex so they pick up the new server. It's listed as $\"solidworks$\".$\r$\n$\r$\nTo register the server with a client later, run configure-clients.ps1 from the install folder."
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ----------------------------------------------------------------- macros ---

; Waits until no process named IMAGE is running, asking the user to close it.
; Cancel aborts the (un)install. tasklist | find: find exits 0 on a match.
!macro WaitForProcessExit IMAGE MESSAGE
  ${Do}
    nsExec::ExecToStack '"$SYSDIR\cmd.exe" /c tasklist /NH /FI "IMAGENAME eq ${IMAGE}" | "$SYSDIR\find.exe" /I "${IMAGE}"'
    Pop $0
    Pop $1
    ${If} $0 != 0
      ${Break}
    ${EndIf}
    ${If} ${Cmd} `MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${MESSAGE}" /SD IDCANCEL IDCANCEL`
      Abort
    ${EndIf}
  ${Loop}
!macroend

!macro CloseRunningApps
  !insertmacro WaitForProcessExit "SLDWORKS.exe" "SOLIDWORKS is running and has the add-in loaded.$\r$\n$\r$\nSave your work, close SOLIDWORKS, then click Retry."
  !insertmacro WaitForProcessExit "mcp_server.exe" "The SolidWorks MCP server is running, started by an MCP client such as Claude Desktop or Codex.$\r$\n$\r$\nQuit that client (for Claude Desktop, also from its tray icon), then click Retry."
!macroend

; Runs configure-clients.ps1 ACTION CLIENT and leaves its exit code (or
; "error" if PowerShell couldn't start) in $0. Its output goes to the log.
!macro ConfigureClient ACTION CLIENT
  nsExec::ExecToLog '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$INSTDIR\configure-clients.ps1" -Action ${ACTION} -Client ${CLIENT} -ServerPath "$INSTDIR\mcp_server.exe"'
  Pop $0
!macroend

!macro AddToClient CLIENT LABEL
  DetailPrint "Registering the MCP server with ${LABEL}..."
  !insertmacro ConfigureClient Add ${CLIENT}
  ${If} $0 != 0
    DetailPrint "Couldn't register the MCP server with ${LABEL} (exit code $0)."
    MessageBox MB_OK|MB_ICONEXCLAMATION "Couldn't register the MCP server with ${LABEL}; the details are in the installer log.$\r$\n$\r$\nThe add-in is installed either way. To add the server to ${LABEL} by hand, point it at:$\r$\n$\r$\n$INSTDIR\mcp_server.exe" /SD IDOK
  ${EndIf}
!macroend

; --------------------------------------------------------------- sections ---

Section "-Prepare"
  !insertmacro CloseRunningApps

  ${If} $SwReleases != ""
    DetailPrint "Found SOLIDWORKS $SwReleases."
  ${Else}
    DetailPrint "No SOLIDWORKS ${SW_MIN_YEAR} or newer installation was found."
  ${EndIf}
SectionEnd

Section "SolidWorks add-in and MCP server" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "/oname=sld_mcp.dll" "${ADDIN_DLL}"
  File "/oname=mcp_server.exe" "${SERVER_EXE}"
  File "/oname=configure-clients.ps1" "${__FILEDIR__}\configure-clients.ps1"
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

!ifdef VCREDIST
Section "Microsoft Visual C++ runtime" SecVCRedist
  ; Both binaries need it. Installing over an equal or newer runtime is a
  ; quick no-op (exit code 1638).
  DetailPrint "Installing the Microsoft Visual C++ runtime..."
  InitPluginsDir
  File "/oname=$PLUGINSDIR\vc_redist.x64.exe" "${VCREDIST}"
  ExecWait '"$PLUGINSDIR\vc_redist.x64.exe" /install /quiet /norestart' $0
  ${If} $0 == 3010
    SetRebootFlag true
  ${ElseIf} $0 != 0
  ${AndIf} $0 != 1638
    DetailPrint "The Visual C++ runtime installer failed (exit code $0)."
  ${EndIf}
  Delete "$PLUGINSDIR\vc_redist.x64.exe"
SectionEnd
!endif

Section "-Register add-in"
  ; The installer is 32-bit, so go through Sysnative to reach the 64-bit
  ; regsvr32 (a 32-bit process can't load the 64-bit DLL). DllRegisterServer
  ; writes the COM class, the SOLIDWORKS add-in key, and turns on loading at
  ; startup for this user.
  DetailPrint "Registering the SOLIDWORKS add-in..."
  ExecWait '"$WINDIR\Sysnative\regsvr32.exe" /s "$INSTDIR\sld_mcp.dll"' $0
  ${If} $0 != 0
    DetailPrint "regsvr32 failed with exit code $0."
    MessageBox MB_OK|MB_ICONSTOP "Registering the SOLIDWORKS add-in failed (regsvr32 exit code $0).$\r$\n$\r$\nThis usually means sld_mcp.dll couldn't load, for example because the Microsoft Visual C++ runtime is missing or older than the one it was built with. Install the latest x64 runtime from https://aka.ms/vc14/vc_redist.x64.exe and run this installer again." /SD IDOK
    Abort
  ${EndIf}
SectionEnd

Section "Register with Claude Desktop" SecClaude
  !insertmacro AddToClient Claude "Claude Desktop"
SectionEnd

Section "Register with Codex" SecCodex
  !insertmacro AddToClient Codex "Codex"
SectionEnd

Section "-Finish"
  WriteRegStr HKLM "Software\${APP_KEY}" "InstallDir" "$INSTDIR"

  SetRegView 64
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" "$0"
  SetRegView 32
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore} "The sld_mcp add-in, registered to load at SOLIDWORKS startup, and mcp_server.exe."
  !ifdef VCREDIST
  !insertmacro MUI_DESCRIPTION_TEXT ${SecVCRedist} "Runtime both programs need. Skipped automatically if the same or a newer version is installed."
  !endif
  !insertmacro MUI_DESCRIPTION_TEXT ${SecClaude} "Adds the server to Claude Desktop's claude_desktop_config.json (Store and classic installs)."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCodex} "Adds the server to Codex's config.toml, shared by the Codex app, CLI and IDE extension."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; -------------------------------------------------------------- functions ---

; Sets $SwReleases to the installed SOLIDWORKS releases from SW_MIN_YEAR on:
; desktop SOLIDWORKS registers HKLM\SOFTWARE\SolidWorks\SOLIDWORKS <year>,
; 3DEXPERIENCE SOLIDWORKS (including for Makers) installs under
; Program Files\Dassault Systemes\SOLIDWORKS Apps <year>.
Function DetectSolidWorks
  StrCpy $SwReleases ""
  SetRegView 64
  ${For} $R0 ${SW_MIN_YEAR} ${SW_MAX_YEAR}
    StrCpy $R1 ""
    EnumRegKey $R1 HKLM "SOFTWARE\SolidWorks\SOLIDWORKS $R0" 0
    ${If} $R1 == ""
    ${AndIf} ${FileExists} "$PROGRAMFILES64\Dassault Systemes\SOLIDWORKS Apps $R0\SOLIDWORKS\*.*"
      StrCpy $R1 "found"
    ${EndIf}
    ${If} $R1 != ""
      ${If} $SwReleases == ""
        StrCpy $SwReleases "$R0"
      ${Else}
        StrCpy $SwReleases "$SwReleases, $R0"
      ${EndIf}
    ${EndIf}
  ${Next}
  SetRegView 32
FunctionEnd

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} needs 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}

  Call DetectSolidWorks
  ${If} $SwReleases == ""
    MessageBox MB_YESNO|MB_ICONEXCLAMATION "No installation of SOLIDWORKS ${SW_MIN_YEAR} or newer was found. The add-in only loads in SOLIDWORKS ${SW_MIN_YEAR} and later.$\r$\n$\r$\nInstall anyway?" /SD IDYES IDYES +2
    Abort
  ${EndIf}

  ; Preselect the clients that look installed. $APPDATA and $LOCALAPPDATA
  ; are this user's (SetShellVarContext current is the default).
  StrCpy $R0 ""
  ${If} ${FileExists} "$APPDATA\Claude\*.*"
    StrCpy $R0 "found"
  ${Else}
    FindFirst $R1 $R2 "$LOCALAPPDATA\Packages\Claude_*"
    ${If} $R2 != ""
      StrCpy $R0 "found"
    ${EndIf}
    FindClose $R1
  ${EndIf}
  ${If} $R0 == ""
    !insertmacro UnselectSection ${SecClaude}
  ${EndIf}

  ReadEnvStr $R0 "CODEX_HOME"
  ${If} $R0 == ""
  ${AndIfNot} ${FileExists} "$PROFILE\.codex\*.*"
    !insertmacro UnselectSection ${SecCodex}
  ${EndIf}
FunctionEnd

; ------------------------------------------------------------- uninstall ---

Section "Uninstall"
  !insertmacro CloseRunningApps

  ; Both are no-ops for a client the server was never added to.
  DetailPrint "Removing the MCP server from Claude Desktop and Codex..."
  !insertmacro ConfigureClient Remove Claude
  !insertmacro ConfigureClient Remove Codex

  DetailPrint "Unregistering the SOLIDWORKS add-in..."
  ExecWait '"$WINDIR\Sysnative\regsvr32.exe" /u /s "$INSTDIR\sld_mcp.dll"' $0

  Delete "$INSTDIR\sld_mcp.dll"
  Delete "$INSTDIR\mcp_server.exe"
  Delete "$INSTDIR\configure-clients.ps1"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\${APP_KEY}"
  SetRegView 64
  DeleteRegKey HKLM "${UNINST_KEY}"
  SetRegView 32
SectionEnd

Function un.onInit
  ${IfNot} ${RunningX64}
    Abort
  ${EndIf}
FunctionEnd
