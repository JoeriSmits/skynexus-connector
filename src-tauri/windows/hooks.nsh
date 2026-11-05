; NSIS hooks for the SkyNexus Connector installer (bundle.windows.nsis.installerHooks).

!macro NSIS_HOOK_PREUNINSTALL
  ; Take the "Start with MSFS" entry out of every MSFS exe.xml while the exe is
  ; still here, so MSFS does not try to start a program that is gone. The
  ; exe does only that with this flag and exits; it changes nothing else.
  ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --remove-msfs-entry'
!macroend
