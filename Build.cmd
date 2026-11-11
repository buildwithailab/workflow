@echo off
setlocal
cd /d "%~dp0"
set "CARGO_TARGET_DIR=%~dp0target"
powershell.exe -NoProfile -File "%~dp0scripts\build-windows.ps1"
if errorlevel 1 (
  echo Build failed.
  pause
  exit /b 1
)
copy /y "target\x86_64-pc-windows-msvc\release\workflow.exe" "Workflow.exe" >nul
if errorlevel 1 (
  echo Could not update Workflow.exe. Close Workflow and try again.
  pause
  exit /b 1
)
echo Workflow is ready. Run Start-Workflow.cmd to open it.
pause

