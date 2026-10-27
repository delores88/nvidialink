; Only the result delivery process is stopped. Spark jobs and native laptop
; workers continue, and durable verified-chunk checkpoints resume after setup.
;
; RTXBridge was DGXBridge. An install from before the rename is upgraded in
; place, in its existing folder, so agent configurations that name that
; folder's sparkdev.exe, and a taskbar pin, keep working. Its old Apps entry,
; shortcuts and startup value are retired only when they belong to that folder.
!define OLD_PRODUCTKEY "Software\DeloresAI\DGXBridge"
!define OLD_UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\DGXBridge"
!define RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"

!macro RemoveOwnedLegacyShortcut shortcut
  !insertmacro IsShortcutTarget "${shortcut}" "$INSTDIR\${MAINBINARYNAME}.exe"
  Pop $4
  ${If} $4 == 1
    Delete "${shortcut}"
  ${EndIf}
!macroend

!macro RemoveOwnedResultStartup name
  ReadRegStr $5 HKCU "${RUNKEY}" "${name}"
  ${If} $5 == '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
    DeleteRegValue HKCU "${RUNKEY}" "${name}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToLog 'taskkill /F /IM sparkdev-return.exe'
  ; The in-app updater passes the existing folder with /D=. A setup started by
  ; hand defaults to a new RTXBridge folder; reuse the earlier one instead.
  ReadRegStr $0 HKCU "${OLD_PRODUCTKEY}" ""
  ${If} $0 != ""
  ${AndIf} $INSTDIR == "$LOCALAPPDATA\${PRODUCTNAME}"
  ${AndIf} ${FileExists} "$0\${MAINBINARYNAME}.exe"
    StrCpy $INSTDIR $0
    SetOutPath $INSTDIR
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Write the current package version on every install/upgrade, then read back.
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  ReadRegStr $0 HKCU "${UNINSTKEY}" "DisplayVersion"
  ${If} $0 != "${VERSION}"
    MessageBox MB_ICONSTOP "Windows could not record ${PRODUCTNAME} ${VERSION}. Run this installer again as your ordinary Windows user."
    Abort
  ${EndIf}
  ReadRegStr $0 HKCU "${OLD_PRODUCTKEY}" ""
  ${If} $0 == $INSTDIR
    ReadRegStr $1 HKCU "${OLD_UNINSTKEY}" "UninstallString"
    ${If} $1 == '$\"$INSTDIR\uninstall.exe$\"'
      DeleteRegKey HKCU "${OLD_UNINSTKEY}"
    ${EndIf}
    DeleteRegKey HKCU "${OLD_PRODUCTKEY}"
    !insertmacro RemoveOwnedLegacyShortcut "$SMPROGRAMS\DeloresAI\DGXBridge.lnk"
    !insertmacro RemoveOwnedLegacyShortcut "$SMPROGRAMS\DGXBridge.lnk"
    !insertmacro RemoveOwnedLegacyShortcut "$DESKTOP\DGXBridge.lnk"
  ${EndIf}
  ; Only this ordinary installer writes HKCU: a packaged CLI's registry can
  ; be virtualized. The dispatcher also resumes physically verified custom
  ; states recorded under the owner's profile, using explicit --state-dir.
  !insertmacro RemoveOwnedResultStartup "DGXBridge Results"
  WriteRegStr HKCU "${RUNKEY}" "RTXBridge Results" '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
  Exec '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog 'taskkill /F /IM sparkdev-return.exe'
  !insertmacro RemoveOwnedResultStartup "RTXBridge Results"
  !insertmacro RemoveOwnedResultStartup "DGXBridge Results"
  ; Keep owner-state registrations and receipts for an in-place reinstall.
!macroend
