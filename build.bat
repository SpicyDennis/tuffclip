@echo off
setlocal
title Building TUFFClip
rem Works from wherever this folder lives (Desktop, Documents, ...).
set "ROOT=%~dp0"
set "OUT=%ROOT%build"
set "TOTAL=4"

call :now T0
echo.
echo === TUFFClip build ===
echo Started at %time:~0,8%. Each step prints when it starts and how long it took.
echo.

echo [1/%TOTAL%] Checking for Rust ^(cargo^)...
where cargo >nul 2>nul
if errorlevel 1 (
  echo   ERROR: Rust is not installed. Get it from https://rustup.rs then run this again.
  goto :fail
)
echo   OK.
echo.

echo [2/%TOTAL%] Checking for the Tauri CLI...
cargo tauri --version >nul 2>nul
if errorlevel 1 (
  echo   Not found, installing it. This only happens once and can take 5-10 minutes.
  echo   It is NOT stuck as long as "Compiling ..." lines keep scrolling by below.
  echo   A long pause on one line ^(e.g. a big crate^) is normal; give it a few minutes.
  echo.
  call :now T1
  cargo install tauri-cli --version "^2" --locked
  if errorlevel 1 (
    echo.
    echo   ERROR: Tauri CLI install failed. The error is in the output above.
    goto :fail
  )
  call :elapsed T1 SPENT
  echo.
  echo   Tauri CLI installed in %SPENT%.
) else (
  echo   Already installed.
)
echo.

echo [3/%TOTAL%] Compiling TUFFClip ^(cargo tauri build^)...
echo   First build: several minutes. Later builds: usually under a minute.
echo   Watch the "Compiling ..." lines below. If they keep changing, it is working.
echo   The last step ^(the tuffclip crate: full optimisation + linking^) can sit on one
echo   line for several minutes. A "still working" line with the elapsed time is
echo   printed every 15 seconds while it runs, so you can tell it is not stuck.
echo.
call :now T1
pushd "%ROOT%src-tauri"
powershell -nologo -noprofile -executionpolicy bypass -file "%ROOT%build-heartbeat.ps1" cargo tauri build --no-bundle
if errorlevel 1 (
  popd
  echo.
  echo   ERROR: Compile failed. The first "error" in the output above is the cause.
  goto :fail
)
popd
call :elapsed T1 SPENT
echo.
echo   Compiled in %SPENT%.
echo.

rem The version comes from Cargo.toml (the single source of truth); the exe is named TUFFClip v0.0.0.exe.
set "VER="
for /f "tokens=2 delims==" %%V in ('findstr /b /c:"version = " "%ROOT%src-tauri\Cargo.toml"') do if not defined VER set "VER=%%~V"
set "VER=%VER: =%"
set "VER=%VER:"=%"
if not defined VER (
  echo   ERROR: Could not read the version from src-tauri\Cargo.toml.
  goto :fail
)
set "EXE=TUFFClip v%VER%.exe"
echo [4/%TOTAL%] Copying %EXE% to the build folder...
if not exist "%OUT%" mkdir "%OUT%"
del /q "%OUT%\TUFFClip*.exe" >nul 2>nul
if exist "%OUT%\TUFFClip*.exe" (
  echo   ERROR: Could not delete the old exe in the build folder. Is TUFFClip still running? Quit it from the tray and retry.
  goto :fail
)
copy /y "%ROOT%src-tauri\target\release\tuffclip.exe" "%OUT%\%EXE%" >nul
if errorlevel 1 (
  echo   ERROR: Could not copy %EXE%. Is TUFFClip still running? Quit it from the tray and retry.
  goto :fail
)
echo   OK.
echo.

call :elapsed T0 SPENT
echo.
echo === Done in %SPENT% ===
echo Output: "%OUT%\%EXE%"
echo FFmpeg is not part of the build. TUFFClip offers to download it into its own data
echo folder the first time you open it, so the exe can live anywhere.
explorer "%OUT%"
pause
exit /b 0

:fail
call :elapsed T0 SPENT
echo.
echo === BUILD FAILED after %SPENT% ===
echo Scroll up for the error message.
pause
exit /b 1

rem ---- helpers ----
rem :now VAR      stores seconds since midnight in VAR
:now
for /f %%S in ('powershell -nologo -noprofile -command "[int](Get-Date).TimeOfDay.TotalSeconds"') do set "%~1=%%S"
exit /b 0

rem :elapsed STARTVAR OUTVAR   stores "Xm Ys" since STARTVAR in OUTVAR
:elapsed
call :now _NOW
set /a _D=_NOW-%~1
if %_D% lss 0 set /a _D+=86400
set /a _M=_D/60, _S=_D%%60
set "%~2=%_M%m %_S%s"
exit /b 0
