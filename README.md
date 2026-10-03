# TUFFClip

A lightweight replay-buffer clipper for Windows. No accounts, no ads, no telemetry, no background services.

## How it stays light

The expensive work never touches the CPU or system RAM:

```
Game window ──gfxcapture (Windows Graphics Capture, frames stay in VRAM, scaled on the GPU if asked)
(or monitor ──ddagrab)  ──► NVENC / AMF / QSV (dedicated encoder chip) ──► 2 s .ts segments (disk or RAM ring)
Hotkey      ──► newest segments stitched with -c copy (no re-encode) ──► clip.mp4
```

This is the same pipeline ShadowPlay and SteelSeries Moments use, so the in-game cost is in the same range (typically 1 to 5% FPS, mostly from the capture copy). Around it:

- **Rust core** (~0% CPU idle). Once a second it looks for your listed games; that's all it does until you press the hotkey.
- **No UI while you play.** Closing the window destroys the webview completely; TUFFClip lives in the tray. The UI only exists when you open it.
- **Games mode** runs nothing at all until one of your listed games is running, and stops when it exits. The last buffer stays saveable for a few minutes afterwards (Settings > Capture).
- **ffmpeg is tied to TUFFClip with a Job Object**, so it can never be left running orphaned, even after a crash.
- **Sound on separate tracks.** By default only the game's own sound is recorded (Windows per-app capture), with Discord and your microphone (optional) on tracks of their own; or pick "everything you hear" for one desktop track. All tracks go to ffmpeg through one pipe, paced in 10 ms chunks (silence-filled), so they stay in sync and ffmpeg never stalls. Each clip starts with a mix of all tracks, so it sounds right in any player; a Discord or mic track that stayed silent is left out of that clip.

## Get it

Download `TUFFClip v1.0.0.exe` (or newer) from the [releases page](https://github.com/SpicyDennis/tuffclip/releases/latest) and run it. There is no installer: the exe is the whole app, so put it wherever you like. Windows SmartScreen may warn about an unknown publisher the first time (the exe isn't code-signed); choose *More info* > *Run anyway*.

You need:

1. **Windows 10 or 11** with an NVIDIA, AMD, or Intel GPU.
2. **FFmpeg 8.0 or newer, full build** (needs `gfxcapture` and `ddagrab`; gfxcapture also does the scaling when you record below native resolution). TUFFClip offers to download one for you the first time you open it (button in the strip under the top bar, or Settings > Advanced); it is stored in `%LOCALAPPDATA%\TUFFClip\ffmpeg\`. Or get your own from https://www.gyan.dev/ffmpeg/builds/ (the "full" build) and put `ffmpeg.exe` on your PATH, or set its full path in Settings.
WebView2 (for the window) is already on Windows 10/11. Nothing else is installed or needed.

**Updates:** turn on Settings > About > *Check for updates* and TUFFClip asks GitHub every 15 minutes while it runs. A new version shows as a strip at the top; *Update* downloads it, checks its fingerprint, swaps the exe and restarts. Off (the default), TUFFClip never goes online on its own.

## Build from source

You need **Rust** (https://rustup.rs) and the **Tauri CLI** (`cargo install tauri-cli --version "^2" --locked`). No Node.js or npm; the UI is plain HTML/CSS/JS.

Double-click **`build.bat`** in the project folder. It installs the Tauri CLI if needed, compiles a release build, and creates a `build` folder right next to `src-tauri` and `ui`:

```
TUFFClip\
  build\
    TUFFClip v0.0.0.exe     <- run this (the number is the version)
  src-tauri\
  ui\
  build.bat
```

`TUFFClip v0.0.0.exe` is self-contained (the UI is embedded, the C runtime is linked in), so you can move it anywhere or make a shortcut to it. FFmpeg is not part of the build; it lives in TUFFClip's data folder. Only one TUFFClip runs at a time: starting the exe again just opens the existing window. `release.bat` publishes the built exe as a GitHub release (needs the GitHub CLI).

For development with live reload instead: `cd src-tauri` then `cargo tauri dev`.

**Tests:** `cargo test` (in `src-tauri`) runs the unit tests. `cargo test -- --ignored --test-threads=1` also runs the live ones: they record the screen, a window and sound with the real FFmpeg (on PATH) and an NVIDIA GPU, save clips from the RAM and disk buffers, and export them every way the Library can.

## Using it

1. Open **Settings**. Choose *Only my games* or *Always record a monitor*.
2. Add games: start the game, click Refresh, pick it from the list, Add. Or type the exe name.
3. Pick your encoder (NVIDIA / AMD / Intel), frame rate, bitrate, clip length, and hotkey (you can add a second one). Settings save as you change them.
4. Play. Press the hotkey (default **Alt+F10**) to save the last N seconds. You'll hear a beep.
5. In **Library**, raw and exported clips are separate tabs. Filter by game, group by game with the checkbox, play clips, drag the amber handles (or press **I** / **O**) to trim, pick a file type, codec and target size, and Export. Click the open clip again to close it.

Files go to `Videos\TUFFClip\Raw\<Game>\` and `Videos\TUFFClip\Exports\<Game>\`.
Settings and `ffmpeg.log` live in `%LOCALAPPDATA%\TUFFClip\`.

The tray icon gets a red dot while recording (amber while a closed game's buffer is still saveable); hover it for what is being recorded. If several of your games are running, TUFFClip records the one you've clipped most; switch from the top bar.

### Capture card (consoles)

Plug a USB HDMI capture card (e.g. the Guermok USB 3.0 one) into a blue USB 3 port and the console's dock into its HDMI input. Click **Capture card** in the top bar (or Settings > Capture card). The window shows the card's live picture and plays its sound, so you play on it; move the mouse to reach the card / sound / volume / fullscreen controls (F11 or double-click for fullscreen). While the window is open TUFFClip records it at the card's own resolution and frame rate (1080p60 for that card), and the clip hotkey saves as usual. Clips are filed under *Capture card* (rename it in Settings > Capture card).

Close OBS or any other app using the card first: Windows lets only one program use a capture card at a time. The first time, the window asks before it looks for the card (*Look for capture card*), because Windows counts capture cards as cameras; TUFFClip never uses the camera anywhere else.

### Export modes

| Choice | What happens | Speed |
|---|---|---|
| Original quality, same codec | Stream copy, no quality loss. Cuts snap to the nearest keyframe (≤ 2 s). | Instant |
| Original quality, other codec | GPU re-encode (H.264 ⇄ HEVC) at a quality-matched bitrate; HEVC needs about 35% less. Audio is copied (or mixed, see below). | Fast |
| 10/25/50/100 MB or custom | GPU encode at a computed bitrate; re-done once if it overshoots. | Fast |
| + "Exact size" | x264 / x265 two-pass on the CPU. Lands closest to the target. | Slower |
| `.webm` | Always re-encoded as VP9 + Opus on the CPU. | Slow |
| `.gif` | Looping, no sound. Frame rate (10-30 fps) and size (240p-720p) are chosen next to the Export button; up to 60 s. | Medium |
| `.png` | A screenshot: the exact frame under the playhead, lossless, at the clip's own resolution. Saved in `Exports\<Game>\Screenshots\`. | Instant |

"Auto" resolution drops to 1080p/720p/480p when the size budget is too small to look good. File type (`.mp4`, `.mkv`, `.mov`, `.webm`, `.gif`, `.png`) and codec are chosen per export, next to the Export button. Exports run at full speed; tick *Low impact* to run them at idle priority on half the CPU threads while you game. H.263 is not offered: it is a 1990s codec that only supports a handful of fixed frame sizes, compresses far worse than H.264 and has no GPU encoder.

**Sound levels:** under the trim bar the Library shows each sound track (game, Discord, mic) with its waveform, a Mute button and a level slider (0-200%; double-click for 100%). You hear the changes while watching, they're remembered per clip, and exports mix the tracks into one at those levels. With the levels untouched, exports keep the clip's own mix (no re-encode).

TUFFClip remembers which raw clips you've exported; Settings > Storage can delete the ones that already have a trimmed version.

### Benchmark

The **Benchmark** tab measures what recording costs your game. Click *Start test*, switch to the game and keep the scene steady; TUFFClip switches recording on and off in rounds (2, 4 or 8 minutes in total), skipping the seconds right after each switch and any time the game isn't in front. The dot flashes when it starts and ends. The result compares on and off, with a range for how much the scene itself varied, so a difference smaller than that is reported as "no measurable difference".

| Access | What it measures |
|---|---|
| Basic (no admin) | GPU load (the counters Task Manager reads), the game's share of the GPU, the video encoder, CPU load. Estimates the frame cost: a GPU with room to spare loses no frames; a maxed-out one loses about the share recording took. |
| Full | Also the game's real frames (average FPS and 1% lows), read from Windows' DXGI/D3D9 frame events like PresentMon. This needs admin rights or membership of the *Performance Log Users* group; without them Windows shows a UAC prompt each test, and only a small helper process runs as admin until the test ends. Vulkan/OpenGL games that don't present through DXGI fall back to the Basic readings. |

The replay buffer is emptied when the test starts. Every result is kept (the newest 300): **All tests…** opens them in a table you can sort by any column and search. Search words are combined (`poe 1080p hevc`), a comma means either (`res:1080,1440`), a minus leaves out (`-basic`), and fields take comparisons (`fps>=120`, `on>144`, `cost<5`). Dates work as `2026`, `october`, `2026-10`, `2026-10-03`, `today` or `date>=2026-09`. Click a row to show that test on the tab, tick two to compare them side by side, or delete one.

**Favorite settings:** any test can become its game's favorite (resolution, frame rate, codec, bitrate, capture method). That game then records with them while the normal settings stay as they are for every other game; the quick settings in the top bar show "★ Favorite" and change the favorite while it's in use. Settings > Games lists the favorites, each with *Use normal settings* / *Use favorite* and *Remove*; Settings > Capture also has *Use normal settings* while one is in use.

## Tuning

- **Zero SSD writes:** set the buffer folder to a RAM disk (e.g. ImDisk). At 30 Mbps the buffer is about 4 MB/s and only ~(clip length + 6 s) of video.
- **More FPS headroom:** lower the frame rate to 60, or record at 1080p on a 1440p monitor (scaled on the GPU by FFmpeg 8's `gfxcapture`). The Benchmark tab tells you whether it helps on your game.
- **A/V out of sync:** adjust *Audio delay* in Settings (positive = audio later).

## Limitations / notes

- By default only the **game's own window** is recorded, so windows in front of it never show. A few games (some exclusive-fullscreen ones) can come out black this way; switch Settings > Capture > Capture method to *Whole display* for those.
- Clip length is accurate to about ±2 s (segment granularity).
- Game-only sound needs Windows 10 version 2004 or newer; on older Windows the game track falls back to everything you hear. Programs that play sound through another process (some launchers, Windows system sounds) aren't caught by per-app capture.
- The Discord track is everything Discord plays (calls and its notification sounds); TUFFClip can't tell a call apart from other Discord sounds.
- HEVC clips play in the built-in player only if Microsoft's HEVC Video Extensions are installed. H.264 always plays.

## Code map

```
src-tauri/src/
  main.rs      Tauri app, commands, single instance, window events, global hotkeys, self-heal
  tray.rs      tray icon (recording dot) and tooltip
  overlay.rs   the recording dot on the game window (flashes on save)
  preview.rs   the Preview tab's low-rate live view (a second ffmpeg, only while the tab is open)
  engine.rs    1 Hz watcher: which game is up, keep ffmpeg running, hold the buffer after a game closes, save clips
  recorder.rs  builds the ffmpeg capture command; stitches segments into clips
  audio.rs     WASAPI capture per track (game / Discord / desktop / mic) -> one paced multichannel f32 stream to ffmpeg stdin
  sound.rs     a clip's tracks for the viewer: decoded .flac copies + waveforms
  export.rs    trim / codec / target-size export with progress
  bench.rs     recording benchmark: on/off rounds, GPU/CPU counters, results
  etw.rs       the benchmark's frame counter (`--bench-helper`, DXGI/D3D9 Present events)
  library.rs   lists clips by game folder, remembers which raws were exported, storage totals
  win.rs       DXGI monitors, game windows, memory info, job object
  config.rs    settings (JSON)
  ff.rs        ffmpeg helpers, capability check, FFmpeg download
  update.rs    opt-in self-update from GitHub releases
  live_tests.rs  end-to-end tests with the real FFmpeg and GPU (`cargo test -- --ignored`)
ui/            index.html, style.css, app.js (no framework, no build step)
               capture.html/.css/.js: the capture card window
```
