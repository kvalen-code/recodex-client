; ReCodex 安装包。
;
; 不复用上游的 CodexPlusPlus.nsi:那份装两个 exe(含已被 slim fork 从工作区
; 移除的管理工具),并且写死了 Codex++ 品牌与注册表键。ReCodex 只有一个 exe。
;
; 注册表键刻意用 Software\ReCodex 而不是上游的 Software\Codex++ ——
; 两者可以并存,卸载 ReCodex 不会动到用户自己装的 Codex++。

!include "MUI2.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef ROOT
  !define ROOT "..\..\.."
!endif

Name "ReCodex"
OutFile "${ROOT}\dist\windows\ReCodex-${VERSION}-windows-x64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\ReCodex"
InstallDirRegKey HKCU "Software\ReCodex" "InstallDir"
RequestExecutionLevel user
Unicode true

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "ReCodex"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "ReCodex"
VIAddVersionKey "LegalCopyright" "AGPL-3.0-only"

!define MUI_ICON "${ROOT}\assets\images\codex-plus-plus.ico"
!define MUI_UNICON "${ROOT}\assets\images\codex-plus-plus.ico"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

Section "Install"
  ; 覆盖安装前必须先停掉正在跑的实例,否则 File 写不进去、装完还是旧版
  nsExec::ExecToLog 'taskkill /IM codex-plus-plus.exe /F'

  SetOutPath "$INSTDIR"
  File "${ROOT}\dist\windows\app\codex-plus-plus.exe"

  CreateShortcut "$DESKTOP\ReCodex.lnk" "$INSTDIR\codex-plus-plus.exe" "" "$INSTDIR\codex-plus-plus.exe"
  CreateDirectory "$SMPROGRAMS\ReCodex"
  CreateShortcut "$SMPROGRAMS\ReCodex\ReCodex.lnk" "$INSTDIR\codex-plus-plus.exe" "" "$INSTDIR\codex-plus-plus.exe"
  CreateShortcut "$SMPROGRAMS\ReCodex\卸载 ReCodex.lnk" "$INSTDIR\uninstall.exe" "" "$INSTDIR\codex-plus-plus.exe"

  WriteRegStr HKCU "Software\ReCodex" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "DisplayName" "ReCodex"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "DisplayIcon" "$INSTDIR\codex-plus-plus.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "UninstallString" "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "Publisher" "ReCodex"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex" "NoRepair" 1

  ; 归属导入:安装包在下发时带着「来自哪个站点」的线索(文件名 @域名、浏览器写的
  ; 下载来源、签名后追加的标签)。把安装包自己的路径交给启动器读一次,写进
  ; %USERPROFILE%\.codex\recodex\api-base —— 代理站用户装完登录就直接打开自己站点的
  ; 授权页,不会看到平台主站。主站直下的包没有线索,这一步静默无事。
  nsExec::ExecToLog '"$INSTDIR\codex-plus-plus.exe" --import-installer-tag "$EXEPATH"'
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  nsExec::ExecToLog 'taskkill /IM codex-plus-plus.exe /F'

  Delete "$INSTDIR\codex-plus-plus.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$DESKTOP\ReCodex.lnk"
  Delete "$SMPROGRAMS\ReCodex\ReCodex.lnk"
  Delete "$SMPROGRAMS\ReCodex\卸载 ReCodex.lnk"
  RMDir "$SMPROGRAMS\ReCodex"

  DeleteRegKey HKCU "Software\ReCodex"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex"
SectionEnd
