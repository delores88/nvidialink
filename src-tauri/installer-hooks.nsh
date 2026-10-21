; Only the result delivery process is stopped. Spark jobs and native laptop
; workers continue, and durable verified-chunk checkpoints resume after setup.
!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToLog 'taskkill /F /IM sparkdev-return.exe'
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Write the current package version on every install/upgrade, then read back.
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  ReadRegStr $0 HKCU "${UNINSTKEY}" "DisplayVersion"
  ${If} $0 != "${VERSION}"
    MessageBox MB_ICONSTOP "Windows could not record DGXBridge ${VERSION}. Run this installer again as your ordinary Windows user."
    Abort
  ${EndIf}
  ; Only this ordinary installer writes HKCU: a packaged CLI's registry can
  ; be virtualized. The dispatcher also resumes physically verified custom
  ; states recorded under the owner's profile, using explicit --state-dir.
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DGXBridge Results" '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
  Exec '$\"$INSTDIR\bin\sparkdev-return.exe$\" --resume-autostart --state-dir $\"$LOCALAPPDATA\SparkDevBridge$\"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog 'taskkill /F /IM sparkdev-return.exe'
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DGXBridge Results"
  ; Keep owner-state registrations and receipts for an in-place reinstall.
!macroend
