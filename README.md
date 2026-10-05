# TUFFClip

A lightweight replay-buffer clipper for Windows. Press a hotkey and the last few seconds of your game are saved as a clip. No accounts, no ads, no telemetry.

## Features

- Records only your listed games (or a whole monitor), and nothing at all while no game is running.
- Captures and encodes on the GPU (NVIDIA, AMD or Intel), so the cost while playing is small. Clips are cut from the buffer without re-encoding.
- Sound on separate tracks: the game, Discord and your microphone, plus a mix that plays right in any player.
- Library with a trimmer, per-track sound levels, and export to `.mp4`, `.mkv`, `.mov`, `.webm`, `.gif` or a `.png` screenshot, optionally to a target file size.
- Capture-card window for recording a console.
- Benchmark tab that measures what recording costs your game.
- Lives in the tray with no window open while you play.

## Install

Download `TUFFClip v1.0.3.exe` (or newer) from the [releases page](https://github.com/SpicyDennis/tuffclip/releases/latest) and run it. There is no installer: the exe is the whole app, so put it wherever you like. Windows SmartScreen may warn about an unknown publisher the first time (the exe isn't code-signed); choose *More info* > *Run anyway*.

You need Windows 10 or 11 with an NVIDIA, AMD or Intel GPU. TUFFClip also needs FFmpeg 8 or newer (full build) and offers to download it the first time you open it. To use your own, put `ffmpeg.exe` on your PATH or set its path in Settings > Advanced.

To get updates, turn on Settings > About > *Check for updates*. It is off by default, and while it's off TUFFClip never goes online on its own.

## Using it

1. Open **Settings** and choose *Only my games* or *Always record a monitor*.
2. Add games: start the game, click Refresh, pick it from the list and Add.
3. Play. Press the hotkey (default **Alt+F10**) to save the last N seconds. You'll hear a beep.
4. Open the **Library** to play clips, trim them (drag the handles, or press **I** / **O**), pick a file type and Export.

Clips are saved to `Videos\TUFFClip\Raw\<Game>\` and exports to `Videos\TUFFClip\Exports\<Game>\`. Settings and `ffmpeg.log` are in `%LOCALAPPDATA%\TUFFClip\`.

### Exporting

| Choice | What happens |
|---|---|
| Original quality | Stream copy with no quality loss. Cuts snap to the nearest keyframe (up to 2 s). Another codec re-encodes on the GPU. |
| A target size (10/25/50/100 MB or custom) | GPU encode at a computed bitrate. *Exact size* uses a slower two-pass CPU encode that lands closest. |
| `.webm` | VP9 + Opus, encoded on the CPU. |
| `.gif` | Looping, no sound, up to 60 s. |
| `.png` | The exact frame under the playhead, lossless. |

File type and codec are chosen per export. Tick *Low impact* to export at idle priority while you game.

### Capture card (consoles)

Plug a USB HDMI capture card into a USB 3 port and your console into its input, then click **Capture card** in the top bar. The window shows the card's picture and plays its sound, and TUFFClip records it at the card's own resolution and frame rate. Close OBS or any other app using the card first, since Windows lets only one program use it at a time.

### Benchmark

Click *Start test*, switch to your game and keep the scene steady. TUFFClip switches recording on and off in rounds and compares the two. Basic needs no admin rights; Full also measures frame rate and 1% lows and needs admin rights for a helper process. Any result can become its game's favorite settings.

## Notes

- By default only the game's own window is recorded, so windows in front of it never show. If a game comes out black, set Settings > Capture > Capture method to *Whole display*.
- Clip length is accurate to about 2 s.
- Game-only sound needs Windows 10 version 2004 or newer; older versions record everything you hear.
- HEVC clips play in the built-in player only if Microsoft's HEVC Video Extensions are installed. H.264 always plays.

## Build from source

You need [Rust](https://rustup.rs) and the Tauri CLI (`cargo install tauri-cli --version "^2" --locked`). There is no Node.js; the UI is plain HTML/CSS/JS.

- **Release build:** run `build.bat`. It produces `build\TUFFClip v1.0.3.exe`.
- **Development:** `cd src-tauri`, then `cargo tauri dev`.
- **Tests:** `cargo test` in `src-tauri` runs the unit tests. `cargo test -- --ignored --test-threads=1` also runs the live tests, which need FFmpeg on PATH and an NVIDIA GPU.

## Code map

```
src-tauri/src/
  main.rs      app, commands, single instance, hotkeys
  engine.rs    watches for games, keeps recording running, saves clips
  recorder.rs  builds the ffmpeg capture command, stitches segments into clips
  audio.rs     WASAPI capture per sound track
  sound.rs     decoded tracks and waveforms for the viewer
  export.rs    trim, codec and target-size export
  library.rs   clip listing and storage
  bench.rs     benchmark (etw.rs counts frames)
  preview.rs   live preview tab
  overlay.rs   recording dot on the game window
  tray.rs      tray icon
  win.rs       Windows API helpers
  config.rs    settings
  ff.rs        ffmpeg helpers and download
  update.rs    opt-in self-update
ui/            index.html, style.css, app.js (no framework); capture.* is the capture-card window
```

## License

MIT, see [LICENSE](LICENSE).