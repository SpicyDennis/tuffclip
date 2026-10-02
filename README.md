# Clipr

A lightweight replay-buffer clipper for Windows. No accounts, no ads, no telemetry, no background services.

## How it stays light

The expensive work never touches the CPU or system RAM:

```
Game window ──gfxcapture (Windows Graphics Capture, frames stay in VRAM)──► scale_d3d11 (GPU, optional)
(or monitor ──ddagrab)  ──► NVENC / AMF / QSV (dedicated encoder chip) ──► 2 s .ts segments (ring buffer)
Hotkey      ──► newest segments stitched with -c copy (no re-encode) ──► clip.mp4
```

This is the same pipeline ShadowPlay and SteelSeries Moments use, so the in-game cost is in the same range (typically 1 to 5% FPS, mostly from the capture copy). Around it:

- **Rust core** (~0% CPU idle). Once a second it looks for your listed games; that's all it does until you press the hotkey.
- **No UI while you play.** Closing the window destroys the webview completely; Clipr lives in the tray. The UI only exists when you open it.
- **Games mode** runs nothing at all until one of your listed games is running, and stops when it exits. The last buffer stays saveable for a few minutes afterwards (Settings > Capture).
- **ffmpeg is tied to Clipr with a Job Object**, so it can never be left running orphaned, even after a crash.
- **System audio** is captured with WASAPI loopback and paced in 10 ms chunks (silence-filled), so ffmpeg never stalls.

## Requirements

1. **Windows 10 or 11** with an NVIDIA, AMD, or Intel GPU.
2. **FFmpeg 8.0 or newer, full build** (needs `gfxcapture` and `ddagrab`, and `scale_d3d11` if you want to record below native resolution). Clipr offers to download one for you the first time you open it (button in the strip under the top bar, or Settings > Advanced); it is stored in `%LOCALAPPDATA%\Clipr\ffmpeg\`. Or get your own from https://www.gyan.dev/ffmpeg/builds/ (the "full" build) and put `ffmpeg.exe` on your PATH, or set its full path in Settings.
3. **Rust** from https://rustup.rs.
4. **Tauri CLI**: `cargo install tauri-cli --version "^2" --locked`

WebView2 is already on Windows 10/11. No Node.js or npm needed; the UI is plain HTML/CSS/JS.

## Build and run

Double-click **`build.bat`** in the project folder. It installs the Tauri CLI if needed, compiles a release build, and creates a `build` folder right next to `src-tauri` and `ui`:

```
Clipr\
  build\
    Clipr.exe     <- run this
  src-tauri\
  ui\
  build.bat
```

`Clipr.exe` is self-contained (the UI is embedded), so you can move it anywhere or make a shortcut to it. FFmpeg is not part of the build; it lives in Clipr's data folder. Only one Clipr runs at a time: starting the exe again just opens the existing window.

For development with live reload instead: `cd src-tauri` then `cargo tauri dev`.

## Using it

1. Open **Settings**. Choose *Only my games* or *Always record a monitor*.
2. Add games: start the game, click Refresh, pick it from the list, Add. Or type the exe name.
3. Pick your encoder (NVIDIA / AMD / Intel), frame rate, bitrate, clip length, and hotkey (you can add a second one). Settings save as you change them.
4. Play. Press the hotkey (default **Alt+F10**) to save the last N seconds. You'll hear a beep.
5. In **Library**, raw and exported clips are separate tabs. Filter by game, group by game with the checkbox, play clips, drag the amber handles (or press **I** / **O**) to trim, pick a file type, codec and target size, and Export. Click the open clip again to close it.

Files go to `Videos\Clipr\Raw\<Game>\` and `Videos\Clipr\Exports\<Game>\`.
Settings and `ffmpeg.log` live in `%LOCALAPPDATA%\Clipr\`.

The tray icon gets a red dot while recording (amber while a closed game's buffer is still saveable); hover it for what is being recorded. If several of your games are running, Clipr records the one you've clipped most; switch from the top bar.

### Export modes

| Choice | What happens | Speed |
|---|---|---|
| Original quality, same codec | Stream copy, no quality loss. Cuts snap to the nearest keyframe (≤ 2 s). | Instant |
| Original quality, other codec | GPU re-encode (H.264 ⇄ HEVC) at a quality-matched bitrate; HEVC needs about 35% less. Audio is copied. | Fast |
| 10/25/50/100 MB or custom | GPU encode at a computed bitrate; re-done once if it overshoots. | Fast |
| + "Exact size" | x264 / x265 two-pass on the CPU. Lands closest to the target. | Slower |
| `.webm` | Always re-encoded as VP9 + Opus on the CPU. | Slow |
| `.gif` | Looping, no sound. Frame rate (10-30 fps) and size (240p-720p) are chosen next to the Export button; up to 60 s. | Medium |

"Auto" resolution drops to 1080p/720p/480p when the size budget is too small to look good. File type (`.mp4`, `.mkv`, `.mov`, `.webm`, `.gif`) and codec are chosen per export, next to the Export button. H.263 is not offered: it is a 1990s codec that only supports a handful of fixed frame sizes, compresses far worse than H.264 and has no GPU encoder.

Clipr remembers which raw clips you've exported; Settings > Storage can delete the ones that already have a trimmed version.

## Tuning

- **Zero SSD writes:** set the buffer folder to a RAM disk (e.g. ImDisk). At 30 Mbps the buffer is about 4 MB/s and only ~(clip length + 6 s) of video.
- **More FPS headroom:** lower the frame rate to 60, or record at 1080p on a 1440p monitor (needs FFmpeg 8's `scale_d3d11`).
- **A/V out of sync:** adjust *Audio delay* in Settings (positive = audio later).

## Limitations / notes

- By default only the **game's own window** is recorded, so windows in front of it never show. A few games (some exclusive-fullscreen ones) can come out black this way; switch Settings > Capture > Capture method to *Whole display* for those.
- Clip length is accurate to about ±2 s (segment granularity).
- Switching your default audio device while recording keeps capturing the old device until recording restarts (change any setting or restart the game).
- HEVC clips play in the built-in player only if Microsoft's HEVC Video Extensions are installed. H.264 always plays.
- Microphone capture isn't included yet. It would go in `audio.rs` as a second cpal stream mixed into the same pacer.

## Code map

```
src-tauri/src/
  main.rs      Tauri app, commands, single instance, window events, global hotkeys
  tray.rs      tray icon (recording dot) and tooltip
  engine.rs    1 Hz watcher: which game is up, keep ffmpeg running, hold the buffer after a game closes, save clips
  recorder.rs  builds the ffmpeg capture command; stitches segments into clips
  audio.rs     WASAPI loopback -> paced f32 stream to ffmpeg stdin
  export.rs    trim / codec / target-size export with progress
  library.rs   lists clips by game folder, remembers which raws were exported, storage totals
  win.rs       DXGI monitors, game windows, memory info, job object
  config.rs    settings (JSON)
  ff.rs        ffmpeg helpers and capability check
ui/            index.html, style.css, app.js (no framework, no build step)
```
