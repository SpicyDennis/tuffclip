# Clipr

A lightweight replay-buffer clipper for Windows. No accounts, no ads, no telemetry, no background services.

## How it stays light

The expensive work never touches the CPU or system RAM:

```
Monitor ──ddagrab (DXGI, frames stay in VRAM)──► scale_d3d11 (GPU, optional)
        ──► NVENC / AMF / QSV (dedicated encoder chip) ──► 2 s .ts segments (ring buffer)
Hotkey  ──► newest segments stitched with -c copy (no re-encode) ──► clip.mp4
```

This is the same pipeline ShadowPlay and SteelSeries Moments use, so the in-game cost is in the same range (typically 1 to 5% FPS, mostly from the capture copy). Around it:

- **Rust core** (~0% CPU idle). Once a second it checks the foreground window; that's all it does until you press the hotkey.
- **No UI while you play.** Closing the window destroys the webview completely; Clipr lives in the tray. The UI only exists when you open it.
- **Games mode** runs nothing at all until one of your listed games is focused, and stops when it exits.
- **ffmpeg is tied to Clipr with a Job Object**, so it can never be left running orphaned, even after a crash.
- **System audio** is captured with WASAPI loopback and paced in 10 ms chunks (silence-filled), so ffmpeg never stalls.

## Requirements

1. **Windows 10 or 11** with an NVIDIA, AMD, or Intel GPU.
2. **FFmpeg 8.0 or newer, full build** (needs `ddagrab`, and `scale_d3d11` if you want to record below native resolution). Get one from https://www.gyan.dev/ffmpeg/builds/ (the "full" build) or https://github.com/BtbN/FFmpeg-Builds/releases. Put `ffmpeg.exe` on your PATH, or set its full path in Settings.
3. **Rust** from https://rustup.rs.
4. **Tauri CLI**: `cargo install tauri-cli --version "^2" --locked`

WebView2 is already on Windows 10/11. No Node.js or npm needed; the UI is plain HTML/CSS/JS.

## Build and run

Double-click **`build.bat`** in the project folder. It installs the Tauri CLI if needed, compiles a release build, and creates a `build` folder right next to `src-tauri` and `ui`:

```
Clipr\
  build\
    Clipr.exe     <- run this
    ffmpeg.exe    <- copied automatically if ffmpeg is on your PATH
  src-tauri\
  ui\
  build.bat
```

`Clipr.exe` is self-contained (the UI is embedded), so you can move the `build` folder anywhere or make a shortcut to it. If `ffmpeg.exe` sits next to it, Clipr finds it without any PATH setup.

For development with live reload instead: `cd src-tauri` then `cargo tauri dev`.

## Using it

1. Open **Settings**. Choose *Only my games* or *Always record a monitor*.
2. Add games: start the game, click Refresh, pick it from the list, Add. Or type the exe name.
3. Pick your encoder (NVIDIA / AMD / Intel), frame rate, bitrate, clip length, and hotkey. Save.
4. Play. Press the hotkey (default **Alt+F10**) to save the last N seconds. You'll hear a beep.
5. In **Library**, raw and exported clips are separate tabs. Filter or group by game, play clips, drag the amber handles (or press **I** / **O**) to trim, pick a target size, and Export.

Files go to `Videos\Clipr\Raw\<Game>\` and `Videos\Clipr\Exports\<Game>\`.
Settings and `ffmpeg.log` live in `%LOCALAPPDATA%\Clipr\`.

### Export modes

| Size choice | What happens | Speed |
|---|---|---|
| Original quality | Stream copy, no quality loss. Cuts snap to the nearest keyframe (≤ 2 s). | Instant |
| 10/25/50/100 MB or custom | GPU encode at a computed bitrate; re-done once if it overshoots. | Fast |
| + "Exact size" | x264 two-pass on the CPU. Lands closest to the target. | Slower |

"Auto" resolution drops to 1080p/720p/480p when the size budget is too small to look good.

## Tuning

- **Zero SSD writes:** set the buffer folder to a RAM disk (e.g. ImDisk). At 30 Mbps the buffer is about 4 MB/s and only ~(clip length + 6 s) of video.
- **More FPS headroom:** lower the frame rate to 60, or record at 1080p on a 1440p monitor (needs FFmpeg 8's `scale_d3d11`).
- **A/V out of sync:** adjust *Audio delay* in Settings (positive = audio later).

## Limitations / notes

- It records the **monitor** the game is on (not a single window). Exclusive-fullscreen games are captured fine on Windows 10/11 thanks to fullscreen optimizations; if one shows black, switch the game to borderless.
- Clip length is accurate to about ±2 s (segment granularity).
- Switching your default audio device while recording keeps capturing the old device until recording restarts (change any setting or restart the game).
- HEVC clips play in the built-in player only if Microsoft's HEVC Video Extensions are installed. H.264 always plays.
- Microphone capture isn't included yet. It would go in `audio.rs` as a second cpal stream mixed into the same pacer.
- This code was written without being able to compile it on Windows here, so the first `cargo tauri dev` may surface a few small compile errors (most likely around exact `windows`-crate signatures). They should be quick to fix.

## Code map

```
src-tauri/src/
  main.rs      Tauri app, commands, tray, global hotkey
  engine.rs    1 Hz watcher: which game is up, keep ffmpeg running, save clips
  recorder.rs  builds the ffmpeg capture command; stitches segments into clips
  audio.rs     WASAPI loopback -> paced f32 stream to ffmpeg stdin
  export.rs    trim / target-size export with progress
  library.rs   lists clips by game folder
  win.rs       DXGI monitors, foreground process, window list, job object
  config.rs    settings (JSON)
  ff.rs        ffmpeg helpers and capability check
ui/            index.html, style.css, app.js (no framework, no build step)
```
