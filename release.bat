@echo off
setlocal
title Publishing TUFFClip
rem Publishes build\TUFFClip v0.0.0.exe as a GitHub release, so TUFFClip's "Update" button finds it.
rem Run build.bat first. Needs the GitHub CLI (gh), signed in. The repo must be public for the
rem update check to see releases.
set "ROOT=%~dp0"
set "VER="
for /f "tokens=2 delims==" %%V in ('findstr /b /c:"version = " "%ROOT%src-tauri\Cargo.toml"') do if not defined VER set "VER=%%~V"
set "VER=%VER: =%"
set "VER=%VER:"=%"
set "EXE=%ROOT%build\TUFFClip v%VER%.exe"
if not exist "%EXE%" (
  echo ERROR: "%EXE%" doesn't exist. Run build.bat first.
  goto :fail
)
where gh >nul 2>nul
if errorlevel 1 (
  echo ERROR: The GitHub CLI isn't installed. Get it from https://cli.github.com then run this again.
  goto :fail
)
echo This publishes TUFFClip v%VER% on GitHub. Everyone with update checks on will be offered it.
choice /m "Publish now"
if errorlevel 2 goto :eof
gh release create "v%VER%" "%EXE%" --repo SpicyDennis/tuffclip --title "TUFFClip v%VER%" --generate-notes
if errorlevel 1 goto :fail
echo.
echo Published v%VER%.
pause
exit /b 0

:fail
pause
exit /b 1
