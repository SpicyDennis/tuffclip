# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

TUFFClip is a Windows-only replay-buffer clipper built with Tauri 2 (Rust core + plain HTML/CSS/JS UI in `ui/`, no Node/npm, no frontend build step). All capture/encode work is delegated to an external **FFmpeg 8+ full build** (needs `ddagrab` and `gfxcapture`, plus `scale_d3d11` for downscaling) running on the GPU (NVENC/AMF/QSV). The app is not a git repo and has no tests or linter configured.

## Working on changes

For every batch of requested changes: load the **suite-design** skill first (UI rules, and its "Versioning" section), follow it, and bump the version before finishing. The version lives in `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json` (keep them equal; `Cargo.lock` follows on the next build). If a change produces a UI pattern the skill doesn't cover yet, add it to the skill.

## Commands

Run from `src-tauri/` unless noted:

- Dev with live reload: `cargo tauri dev`
- Release build: `build.bat` in the repo root (runs `cargo tauri build --no-bundle`, copies `target\release\tuffclip.exe` to `build\TUFFClip.exe`; the cargo step goes through `build-heartbeat.ps1`, which prints "still working" every 15 s because the LTO link is silent for minutes; it does not bundle ffmpeg, the app downloads it into the data dir on first run). Needs `cargo install tauri-cli --version "^2" --locked`.
- Type-check only: `cargo check`

`README.md` notes the code was originally written without compiling on Windows, so expect occasional `windows`-crate (0.58) signature errors.

UI follows the suite-design skill (Slate & Tally). theme.css / menu.js are the shared theme; don't edit them per app. File menu and Settings follow the skill's fixed order. Deliberate deviations, per the owner's requests: settings autosave (no draft/Save bar); no File menu (menu.js is no longer loaded; Ctrl+, / Ctrl+Q / Esc still work, Open data folder lives in Settings > Advanced); no Back button in Settings (tab or Esc leaves); the top bar holds quick settings (clip length, resolution, frame rate) in the middle and a reminder of the clip shortcut at the far right.

## Recent behaviour worth knowing

- Distribution: `build\TUFFClip.exe` is meant to be handed out alone. `.cargo/config.toml` links the MSVC CRT statically (`+crt-static`) so no VC++ Redistributable is needed; WebView2 is the only external runtime (preinstalled on current Windows 10/11). Don't add files that must sit next to the exe.

- Games mode defaults to `capture_method: window`: ffmpeg's `gfxcapture` (Windows Graphics Capture) records only the game's HWND, so windows in front never show; resizes are letterboxed (`resize_mode=scale_aspect`) instead of restarting, and `fps=` pads the variable frame rate. `capture_method: display` is the old path: ddagrab cropped to the window's client rect (`recorder::crop_for`), where a changed rect must hold still 3 s before ffmpeg restarts (`Engine::resolve_crop`). Desktop mode is always ddagrab.
- Several listed games running: `Engine::update_tracked` picks the one with the most raw clips and then sticks to it (switching restarts the buffer); the top-bar selector calls `set_target`.
- When the game closes, `Engine::keep_or_discard` stops ffmpeg but keeps the buffer (`Held`: RAM `Arc<RamBuf>` or the segment dir) for `hold_minutes`; `save_clip_inner` falls back to it. A new recording drops it.
- `exported.json` (data dir) maps raw clip -> its export files (`library::record_export`); `Clip.exported` and the Storage cleanup use it. Renames/deletes must keep it in sync.
- Export file type/codec are per-export (UI + `ExportRequest`), not settings. `gentle_save` (alias of old `export_gentle`) only affects saving raw clips.
- Window X / minimize: `close_to_tray` (default on; off = X calls `app.exit`) and `minimize_to_tray`. `hide_window`/minimize use `destroy()` so they never trigger the quit path. `tauri-plugin-single-instance` must stay the first plugin.
- `buffer_in_ram`: ffmpeg writes MPEG-TS to stdout, `RamBuf` cuts it at video-PID (256) keyframes; saving pipes the chunks to ffmpeg stdin. Disk mode is the segment ring as before.
- Crash handling is retry-with-backoff forever (`Engine::note_crash`), plus a stall watchdog; the watcher tick is wrapped in `catch_unwind`.
- `.gif` export (`export.rs`): single ffmpeg pass with palettegen/paletteuse, no audio, own fps/size selects (`#gifFps`, `#gifRes`); exported gifs are not listed in the library. Settings codec defaults to HEVC (existing settings keep their value). Export progress shows percent + time left only (no speed). Games get their display name from the game list; adding a running app uses the optional "Name to show" field, not the window title.
- `.png` export (`export::export_frame`) is a screenshot: the frame under the playhead, decoded raw from the clip with one ffmpeg call, no filters, lossless. Picking `.png` hides the mode/size/bitrate/codec/resolution/low-impact controls and the button reads "Save frame". They go in `Exports\<Game>\Screenshots\`. Like gifs, pngs aren't listed in the library and don't mark the raw clip as exported.
- Exports always run at idle priority with few threads. `.webm` always re-encodes (VP9 + Opus); "original quality" with a different codec is a quality-matched GPU re-encode (`HEVC_RATIO` 0.65, duplicated in `ui/app.js`), audio copied.
- `fps: 0` means "native": the monitor refresh rate from `EnumDisplaySettingsW`.

- Bitrate is automatic by default (`bitrate_auto`): `config::auto_bitrate` (output size x fps^0.75 x 0.2 bpp, HEVC x0.65) is applied in `RecordSpec::apply_auto_bitrate`; `autoRate` in `ui/app.js` mirrors it for display. The settings field is locked until "Set the bitrate myself" is ticked.
- Renaming a game in Settings > Games moves its `Raw\<Game>` / `Exports\<Game>` folders (`library::rename_game`, called from `apply_config`) and rewrites `exported.json`; file names don't change. Names apply on blur, not per keystroke. Games added get their exe name (editable afterwards).
- `show_main` retries (window still closing, WebView2 hiccup) so opening from the tray or a second launch can't silently do nothing; `memory_info` runs off the main thread so a busy recorder can't freeze the window.

- Self-heal (`start_self_heal` in `main.rs`): a thread pings the main thread every 10 s; if it hasn't answered for 90 s, TUFFClip relaunches itself (`--hidden --relaunch`, which waits 3 s so the single-instance hand-off doesn't hit the dying copy) and exits. Goal: a hung background instance must never stop you opening TUFFClip.

## Architecture

The key design goal is near-zero idle cost; several choices follow from it:

- **No webview while gaming.** `main.rs::show_main` creates the `main` window on demand (in a spawned thread, to avoid a Windows deadlock) and it is destroyed on close. `windows: []` in `tauri.conf.json` is intentional. `RunEvent::ExitRequested` is prevented so the app lives in the tray; only the tray "Quit" (`app.exit(0)`) exits, which triggers `Engine::shutdown`.
- **`Engine` (`engine.rs`)** is shared as `Arc<Engine>` Tauri state. `run_watcher` is a 1 Hz loop that checks the foreground process (`win.rs`) against the configured mode (`CaptureMode`: games-only vs. always-record a monitor), and starts/stops/restarts the recorder. Changing settings calls `eng.config_changed()`; anything in `RecordSpec` that differs forces an ffmpeg restart. Status is pushed to the UI via Tauri events.
- **`recorder.rs`** builds and supervises the single long-lived ffmpeg process: ddagrab -> optional scale_d3d11 -> HW encoder -> 2 s MPEG-TS segments in a rotating ring (`SEG_SECS`). Saving a clip stitches the newest segments with `-c copy` (no re-encode), so clip length is only accurate to about ±2 s.
- **`audio.rs`** captures WASAPI loopback via `cpal` and feeds ffmpeg's stdin as paced f32 chunks (10 ms, silence-filled) so ffmpeg never stalls. Audio device is fixed at recorder start.
- **`win.rs`** holds the raw `windows`-crate code: DXGI monitor/adapter enumeration, foreground process lookup, window list, and the Job Object that ties ffmpeg's lifetime to TUFFClip (no orphaned ffmpeg after a crash).
- **`export.rs`** does trim / target-size export with progress events (`export-progress`): stream copy for "original", GPU encode at a computed bitrate (retried once on overshoot), or x264 two-pass for "exact size".
- **`library.rs`** lists clips from per-game folders: `<clips_dir>\Raw\<Game>\` and `<clips_dir>\Exports\<Game>\` (default `Videos\TUFFClip`). `config.rs` persists JSON settings and `ffmpeg.log` in `%LOCALAPPDATA%\TUFFClip\`. `ff.rs` wraps ffmpeg invocation (always `CREATE_NO_WINDOW`) and the capability check.
- **IPC surface** is the `#[tauri::command]` list in `main.rs`; the UI (`ui/app.js`) calls them through `withGlobalTauri` (`window.__TAURI__`). Adding a command means registering it in `generate_handler!` and calling it from `app.js`.
- **Security constraints:** the CSP and `assetProtocol` in `tauri.conf.json` control video playback; `main.rs` adds the clips dir to the asset scope at startup and whenever it changes. File-deleting commands must go through `inside_clips_dir` (canonicalized path must be under the clips dir).
- **Hotkeys** (default Alt+F10, plus an optional `hotkey2`) are registered via `tauri-plugin-global-shortcut` (`set_hotkeys`); `save_config` re-registers them and rolls back to the old ones on failure. `tray.rs` owns the tray icon (red dot while recording) and tooltip, updated from `Engine::update_tray`.

- Renamed from Clipr to TUFFClip in 0.9.0: crate/exe `tuffclip`, data dir `%LOCALAPPDATA%\TUFFClip` (`config::data_dir` renames an old `Clipr` dir once), startup Run key `TUFFClip` (the old `Clipr` key is deleted on the next toggle). Existing `clips_dir` settings keep their saved path; only the default for new installs is `Videos\TUFFClip`. The logo is `src-tauri/icons/icon.png` (and `ui/logo.png` for the top bar); localStorage keys stay `clipr.*` so saved preferences survive.
