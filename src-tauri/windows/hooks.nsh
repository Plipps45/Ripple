!macro RIPPLE_QUIT_RUNNING
  ${If} ${FileExists} "$INSTDIR\${MAINBINARYNAME}.exe"
    nsExec::Exec '"$INSTDIR\${MAINBINARYNAME}.exe" --quit'
    Pop $0
    StrCpy $1 0
    ${Do}
      nsExec::Exec '"$SYSDIR\cmd.exe" /c tasklist /FI "IMAGENAME eq ${MAINBINARYNAME}.exe" /NH | "$SYSDIR\find.exe" /I "${MAINBINARYNAME}.exe"'
      Pop $0
      ${IfThen} $0 != 0 ${|} ${ExitDo} ${|}
      Sleep 200
      IntOp $1 $1 + 1
    ${LoopUntil} $1 >= 25
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro RIPPLE_QUIT_RUNNING
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro RIPPLE_QUIT_RUNNING
!macroend

!macro NSIS_HOOK_POSTINSTALL
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  nsExec::Exec '"$SYSDIR\ie4uinit.exe" -show'
!macroend
