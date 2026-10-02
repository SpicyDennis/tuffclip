@echo off
setlocal
title Building Clipr
rem Works from wherever this folder lives (Desktop, Documents, ...).
set "ROOT=%~dp0"
set "OUT=%ROOT%build"
set "TOTAL=5"

call :now T0
echo.
echo === Clipr build ===
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

echo [3/%TOTAL%] Compiling Clipr ^(cargo tauri build^)...
echo   First build: several minutes. Later builds: usually under a minute.
echo   Watch the "Compiling ..." lines below. If they keep changing, it is working.
echo   The final "Compiling clipr" step can sit silent for a minute or two while
echo   the linker runs; that is normal.
echo.
call :now T1
pushd "%ROOT%src-tauri"
cargo tauri build --no-bundle
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

echo [4/%TOTAL%] Copying Clipr.exe to the build folder...
if not exist "%OUT%" mkdir "%OUT%"
copy /y "%ROOT%src-tauri\target\release\clipr.exe" "%OUT%\Clipr.exe" >nul
if errorlevel 1 (
  echo   ERROR: Could not copy Clipr.exe. Is Clipr still running? Quit it from the tray and retry.
  goto :fail
)
echo   OK.
echo.

echo [5/%TOTAL%] Looking for ffmpeg.exe...
rem Put ffmpeg.exe next to Clipr.exe so it works even if PATH changes.
if exist "%OUT%\ffmpeg.exe" (
  echo   Already in the build folder.
  goto :ffdone
)
for /f "delims=" %%F in ('where ffmpeg 2^>nul') do (
  copy /y "%%F" "%OUT%\ffmpeg.exe" >nul
  echo   Copied ffmpeg.exe from %%F
  goto :ffdone
)
echo   Note: ffmpeg.exe was not found on PATH. Drop ffmpeg.exe into the build folder,
echo         or set its path in Clipr's Settings.
:ffdone

call :elapsed T0 SPENT
echo.
echo === Done in %SPENT% ===
echo Output: "%OUT%\Clipr.exe"
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
