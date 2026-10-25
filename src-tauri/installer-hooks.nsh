; OICPP IDE installer hooks
; 文件关联由 tauri.conf.json 的 bundle.fileAssociations 生成；
; 这里补充资源管理器右键菜单，并在卸载时清理。

!macro NSIS_HOOK_POSTINSTALL
  ; 文件右键：在 OICPP 中打开文件
  WriteRegStr SHCTX "Software\Classes\*\shell\OICPP_OpenFile" "" "在 OICPP 中打开文件"
  WriteRegStr SHCTX "Software\Classes\*\shell\OICPP_OpenFile" "Icon" '"$INSTDIR\${MAINBINARYNAME}.exe",0'
  StrCpy $R0 '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'
  WriteRegStr SHCTX "Software\Classes\*\shell\OICPP_OpenFile\command" "" $R0

  ; 文件夹右键：在 OICPP 中打开文件夹
  WriteRegStr SHCTX "Software\Classes\Directory\shell\OICPP_OpenFolder" "" "在 OICPP 中打开文件夹"
  WriteRegStr SHCTX "Software\Classes\Directory\shell\OICPP_OpenFolder" "Icon" '"$INSTDIR\${MAINBINARYNAME}.exe",0'
  StrCpy $R1 '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'
  WriteRegStr SHCTX "Software\Classes\Directory\shell\OICPP_OpenFolder\command" "" $R1

  ; 文件夹背景右键：在 OICPP 中打开文件夹
  WriteRegStr SHCTX "Software\Classes\Directory\Background\shell\OICPP_OpenFolder" "" "在 OICPP 中打开文件夹"
  WriteRegStr SHCTX "Software\Classes\Directory\Background\shell\OICPP_OpenFolder" "Icon" '"$INSTDIR\${MAINBINARYNAME}.exe",0'
  StrCpy $R2 '"$INSTDIR\${MAINBINARYNAME}.exe" "%V"'
  WriteRegStr SHCTX "Software\Classes\Directory\Background\shell\OICPP_OpenFolder\command" "" $R2
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DeleteRegKey SHCTX "Software\Classes\*\shell\OICPP_OpenFile"
  DeleteRegKey SHCTX "Software\Classes\Directory\shell\OICPP_OpenFolder"
  DeleteRegKey SHCTX "Software\Classes\Directory\Background\shell\OICPP_OpenFolder"
!macroend
