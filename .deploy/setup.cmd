@echo off
REM RustDesk build machine - one click setup.
REM Installs the offline build environment and registers this machine
REM as a build node of the admin panel.
REM Edit deploy-config.txt before running.

cd /d "%~dp0"
powershell -ExecutionPolicy Bypass -File "%~dp0setup.ps1"
if errorlevel 1 (
  echo.
  echo Setup failed.
  pause
  exit /b 1
)
echo.
echo Done. You can close this window.
pause
