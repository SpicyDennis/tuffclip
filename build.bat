@echo off
setlocal
title Building Clipr
rem Works from wherever this folder lives (Desktop, Documents, ...).
set "ROOT=%~dp0"
set "OUT=%ROOT%build"

echo.
echo === Clipr build ===
echo.

where cargo >nul 2>nul
if errorlevel 1 (
  echo Rust is not installed. Get it from https://rustup.rs then run this again.
  goto :fail
)

cargo tauri --version >nul 2>nul
if errorlevel 1 (
  echo Installing the Tauri CLI. This only happens once and takes a few minutes...
  cargo install tauri-cli --version "^2" --locked
  if errorlevel 1 goto :fail
)

pushd "%ROOT%src-tauri"
echo Compiling. The first build takes a few minutes, later ones are much faster.
cargo tauri build --no-bundle
if errorlevel 1 (
  popd
  goto :fail
)
popd

if not exist "%OUT%" mkdir "%OUT%"
copy /y "%ROOT%src-tauri\target\release\clipr.exe" "%OUT%\Clipr.exe" >nul
if errorlevel 1 goto :fail

rem Put ffmpeg.exe next to Clipr.exe so it works even if PATH changes.
if not exist "%OUT%\ffmpeg.exe" (
  for /f "delims=" %%F in ('where ffmpeg 2^>nul') do (
    copy /y "%%F" "%OUT%\ffmpeg.exe" >nul
    echo Copied ffmpeg.exe from %%F
    goto :ffdone
  )
  echo Note: ffmpeg.exe was not found on PATH. Drop ffmpeg.exe into the build folder,
  echo       or set its path in Clipr's Settings.
)
:ffdone

echo.
echo Done: "%OUT%\Clipr.exe"
explorer "%OUT%"
pause
exit /b 0

:fail
echo.
echo Build failed. Scroll up for the error.
pause
exit /b 1
