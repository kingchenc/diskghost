; The setup installs the `diskghost` CLI next to the app (bundle.externalBin)
; and puts that folder on the user's PATH. The app edits HKCU\Environment
; itself (src/path_env.rs): NSIS strings would cut a long PATH short.

!macro NSIS_HOOK_POSTINSTALL
  ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --add-to-path'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --remove-from-path'
!macroend
