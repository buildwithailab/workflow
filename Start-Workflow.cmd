@echo off
setlocal
cd /d "%~dp0"
if exist "Workflow.exe" (
  start "" "%~dp0Workflow.exe"
  exit /b 0
)
if exist "target\release\workflow.exe" (
  start "" "%~dp0target\release\workflow.exe"
  exit /b 0
)
if exist "target\x86_64-pc-windows-msvc\release\workflow.exe" (
  start "" "%~dp0target\x86_64-pc-windows-msvc\release\workflow.exe"
  exit /b 0
)
echo Workflow.exe was not found. Run Build.cmd first.
pause

