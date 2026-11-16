@echo off
cd /d "%~dp0"
cargo build --release --locked
if errorlevel 1 goto error
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\install-build.ps1"
if errorlevel 1 goto error
exit /b 0
:error
pause
exit /b 1
