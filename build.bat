@echo off
setlocal
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
taskkill /f /im ffscreencast.exe >nul 2>&1
cargo build --release
if errorlevel 1 pause
copy /y target\release\ffscreencast.exe .
if errorlevel 1 pause
echo.
echo Build complete: ffscreencast.exe
pause
