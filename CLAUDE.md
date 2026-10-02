# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Clipr is a Windows-only replay-buffer clipper built with Tauri 2 (Rust core + plain HTML/CSS/JS UI in `ui/`, no Node/npm, no frontend build step). All capture/encode work is delegated to an external **FFmpeg 8+ full build** (needs `ddagrab`, plus `scale_d3d11` for downscaling) running on the GPU (NVENC/AMF/QSV). The app is not a git repo and has no tests or linter configured.

## Commands

Run from `src-tauri/` unless noted:

- Dev with live reload: `cargo tauri dev`
- Release build: `build.bat` in the repo root (runs `cargo tauri build --no-bundle`, copies `target\release\clipr.exe` to `build\Clipr.exe`, and copies `ffmpeg.exe` from PATH if present). Needs `cargo install tauri-cli --version "^2" --locked`.
- Type-check only: `cargo check`

`README.md` notes the code was originally written without compiling on Windows, so expect occasional `windows`-crate (0.58) signature errors.

UI follows the suite-design skill (Slate & Tally). theme.css / menu.js are the shared theme; don't edit them per app. File menu and Settings follow the skill's fixed order. One deliberate deviation: settings autosave (no draft/Save bar), per the owner's request.

## Recent behaviour worth knowing

- Windowed (non-fullscreen) games are captured by cropping ddagrab to the window's client rect (`recorder::crop_for`); a changed rect must hold still 3 s before ffmpeg restarts (`Engine::resolve_crop`), since a restart empties the buffer.
- `buffer_in_ram`: ffmpeg writes MPEG-TS to stdout, `RamBuf` cuts it at video-PID (256) keyframes; saving pipes the chunks to ffmpeg stdin. Disk mode is the segment ring as before.
- Crash handling is retry-with-backoff forever (`Engine::note_crash`), plus a stall watchdog; the watcher tick is wrapped in `catch_unwind`.
- Exports run at idle priority with few threads; `export_gentle` also adds `-readrate` while a game is recording. WebM always re-encodes (VP9 + Opus).
- `fps: 0` means "native": the monitor refresh rate from `EnumDisplaySettingsW`.

## Architecture

The key design goal is near-zero idle cost; several choices follow from it:

- **No webview while gaming.** `main.rs::show_main` creates the `main` window on demand (in a spawned thread, to avoid a Windows deadlock) and it is destroyed on close. `windows: []` in `tauri.conf.json` is intentional. `RunEvent::ExitRequested` is prevented so the app lives in the tray; only the tray "Quit" (`app.exit(0)`) exits, which triggers `Engine::shutdown`.
- **`Engine` (`engine.rs`)** is shared as `Arc<Engine>` Tauri state. `run_watcher` is a 1 Hz loop that checks the foreground process (`win.rs`) against the configured mode (`CaptureMode`: games-only vs. always-record a monitor), and starts/stops/restarts the recorder. Changing settings calls `eng.config_changed()`; anything in `RecordSpec` that differs forces an ffmpeg restart. Status is pushed to the UI via Tauri events.
- **`recorder.rs`** builds and supervises the single long-lived ffmpeg process: ddagrab -> optional scale_d3d11 -> HW encoder -> 2 s MPEG-TS segments in a rotating ring (`SEG_SECS`). Saving a clip stitches the newest segments with `-c copy` (no re-encode), so clip length is only accurate to about ±2 s.
- **`audio.rs`** captures WASAPI loopback via `cpal` and feeds ffmpeg's stdin as paced f32 chunks (10 ms, silence-filled) so ffmpeg never stalls. Audio device is fixed at recorder start.
- **`win.rs`** holds the raw `windows`-crate code: DXGI monitor/adapter enumeration, foreground process lookup, window list, and the Job Object that ties ffmpeg's lifetime to Clipr (no orphaned ffmpeg after a crash).
- **`export.rs`** does trim / target-size export with progress events (`export-progress`): stream copy for "original", GPU encode at a computed bitrate (retried once on overshoot), or x264 two-pass for "exact size".
- **`library.rs`** lists clips from per-game folders: `<clips_dir>\Raw\<Game>\` and `<clips_dir>\Exports\<Game>\` (default `Videos\Clipr`). `config.rs` persists JSON settings and `ffmpeg.log` in `%LOCALAPPDATA%\Clipr\`. `ff.rs` wraps ffmpeg invocation (always `CREATE_NO_WINDOW`) and the capability check.
- **IPC surface** is the `#[tauri::command]` list in `main.rs`; the UI (`ui/app.js`) calls them through `withGlobalTauri` (`window.__TAURI__`). Adding a command means registering it in `generate_handler!` and calling it from `app.js`.
- **Security constraints:** the CSP and `assetProtocol` in `tauri.conf.json` control video playback; `main.rs` adds the clips dir to the asset scope at startup and whenever it changes. File-deleting commands must go through `inside_clips_dir` (canonicalized path must be under the clips dir).
- **Hotkey** (default Alt+F10) is registered via `tauri-plugin-global-shortcut`; `save_config` re-registers it and rolls back to the old one on failure.
