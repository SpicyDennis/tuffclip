# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Clipr is a Windows-only replay-buffer clipper built with Tauri 2 (Rust core + plain HTML/CSS/JS UI in `ui/`, no Node/npm, no frontend build step). All capture/encode work is delegated to an external **FFmpeg 8+ full build** (needs `ddagrab` and `gfxcapture`, plus `scale_d3d11` for downscaling) running on the GPU (NVENC/AMF/QSV). The app is not a git repo and has no tests or linter configured.

## Working on changes

For every batch of requested changes: load the **suite-design** skill first (UI rules, and its "Versioning" section), follow it, and bump the version before finishing. The version lives in `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json` (keep them equal; `Cargo.lock` follows on the next build). If a change produces a UI pattern the skill doesn't cover yet, add it to the skill.

## Commands

Run from `src-tauri/` unless noted:

- Dev with live reload: `cargo tauri dev`
- Release build: `build.bat` in the repo root (runs `cargo tauri build --no-bundle`, copies `target\release\clipr.exe` to `build\Clipr.exe`; it does not bundle ffmpeg, the app downloads it into the data dir on first run). Needs `cargo install tauri-cli --version "^2" --locked`.
- Type-check only: `cargo check`

`README.md` notes the code was originally written without compiling on Windows, so expect occasional `windows`-crate (0.58) signature errors.

UI follows the suite-design skill (Slate & Tally). theme.css / menu.js are the shared theme; don't edit them per app. File menu and Settings follow the skill's fixed order. One deliberate deviation: settings autosave (no draft/Save bar), per the owner's request.

## Recent behaviour worth knowing

- Games mode defaults to `capture_method: window`: ffmpeg's `gfxcapture` (Windows Graphics Capture) records only the game's HWND, so windows in front never show; resizes are letterboxed (`resize_mode=scale_aspect`) instead of restarting, and `fps=` pads the variable frame rate. `capture_method: display` is the old path: ddagrab cropped to the window's client rect (`recorder::crop_for`), where a changed rect must hold still 3 s before ffmpeg restarts (`Engine::resolve_crop`). Desktop mode is always ddagrab.
- Several listed games running: `Engine::update_tracked` picks the one with the most raw clips and then sticks to it (switching restarts the buffer); the top-bar selector calls `set_target`.
- When the game closes, `Engine::keep_or_discard` stops ffmpeg but keeps the buffer (`Held`: RAM `Arc<RamBuf>` or the segment dir) for `hold_minutes`; `save_clip_inner` falls back to it. A new recording drops it.
- `exported.json` (data dir) maps raw clip -> its export files (`library::record_export`); `Clip.exported` and the Storage cleanup use it. Renames/deletes must keep it in sync.
- Export file type/codec are per-export (UI + `ExportRequest`), not settings. `gentle_save` (alias of old `export_gentle`) only affects saving raw clips.
- Window X / minimize: `close_to_tray` (default on; off = X calls `app.exit`) and `minimize_to_tray`. `hide_window`/minimize use `destroy()` so they never trigger the quit path. `tauri-plugin-single-instance` must stay the first plugin.
- `buffer_in_ram`: ffmpeg writes MPEG-TS to stdout, `RamBuf` cuts it at video-PID (256) keyframes; saving pipes the chunks to ffmpeg stdin. Disk mode is the segment ring as before.
- Crash handling is retry-with-backoff forever (`Engine::note_crash`), plus a stall watchdog; the watcher tick is wrapped in `catch_unwind`.
- `.gif` export (`export.rs`): single ffmpeg pass with palettegen/paletteuse, no audio, own fps/size selects (`#gifFps`, `#gifRes`); exported gifs are not listed in the library. Settings codec defaults to HEVC (existing settings keep their value). Export progress shows percent + time left only (no speed). Games get their display name from the game list; adding a running app uses the optional "Name to show" field, not the window title.
- Exports always run at idle priority with few threads. `.webm` always re-encodes (VP9 + Opus); "original quality" with a different codec is a quality-matched GPU re-encode (`HEVC_RATIO` 0.65, duplicated in `ui/app.js`), audio copied.
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
- **Hotkeys** (default Alt+F10, plus an optional `hotkey2`) are registered via `tauri-plugin-global-shortcut` (`set_hotkeys`); `save_config` re-registers them and rolls back to the old ones on failure. `tray.rs` owns the tray icon (red dot while recording) and tooltip, updated from `Engine::update_tray`.
