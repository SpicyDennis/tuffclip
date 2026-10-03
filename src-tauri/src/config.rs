use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Buffer only while one of the enabled games is running.
    Games,
    /// Always buffer the chosen monitor.
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoder {
    Nvenc,
    Amf,
    Qsv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BitrateUnit {
    Kbps,
    Mbps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMethod {
    /// Record only the game's own window, so nothing in front of it shows up.
    Window,
    /// Record the whole monitor (cropped to the game's window if it isn't fullscreen).
    Display,
}
/// Which sound goes on the main track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSource {
    /// Only the game's own sound (Discord and the mic can get their own tracks).
    Game,
    /// Everything you hear, on one track.
    Desktop,
}

/// Where the small "recording" dot sits on the captured window (or Off).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Indicator {
    Off,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameEntry {
    pub exe: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// This game's own indicator choice; None follows the global setting.
    #[serde(default)]
    pub indicator: Option<Indicator>,
    /// This game's favorite recording settings (set from a benchmark test).
    #[serde(default)]
    pub favorite: Option<Favorite>,
    /// Record with `favorite` (off = the normal settings, the favorite stays saved).
    #[serde(default = "yes")]
    pub use_favorite: bool,
}

/// Recording settings one game uses instead of the normal ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Favorite {
    /// 0 = match the monitor's refresh rate.
    pub fps: u32,
    /// Output height; 0 = native resolution.
    pub height: u32,
    pub codec: Codec,
    pub bitrate_auto: bool,
    pub bitrate_kbps: u32,
    pub capture_method: CaptureMethod,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mode: CaptureMode,
    /// DXGI device name, e.g. `\\.\DISPLAY1`. Used in desktop mode.
    pub monitor: Option<String>,
    pub games: Vec<GameEntry>,
    /// 0 = match the monitor's refresh rate.
    pub fps: u32,
    /// Output height; 0 = native resolution.
    pub height: u32,
    pub bitrate_kbps: u32,
    /// Pick the bitrate from resolution, frame rate and codec (off = use `bitrate_kbps`).
    pub bitrate_auto: bool,
    pub clip_seconds: u32,
    pub encoder: Encoder,
    pub codec: Codec,
    pub draw_mouse: bool,
    pub audio: bool,
    /// Game-only sound or everything you hear (desktop capture mode always records everything).
    pub audio_source: AudioSource,
    /// With game-only sound: Discord on a track of its own.
    pub discord_track: bool,
    /// Record a microphone on a track of its own.
    pub mic: bool,
    /// Windows device id of the microphone; empty = the Windows default.
    pub mic_device: String,
    pub audio_kbps: u32,
    pub audio_offset_ms: i32,
    pub hotkey: String,
    pub clips_dir: PathBuf,
    pub buffer_dir: Option<PathBuf>,
    pub ffmpeg: String,
    pub beep: bool,
    pub start_hidden: bool,
    /// Keep the rolling buffer in memory instead of on disk.
    pub buffer_in_ram: bool,
    pub bitrate_unit: BitrateUnit,
    /// Save raw clips at low priority so the save itself never costs a game frames.
    #[serde(alias = "export_gentle")]
    pub gentle_save: bool,
    /// Optional second shortcut for saving a clip (empty = none).
    pub hotkey2: String,
    /// How long the last buffer stays saveable after a game closes (0 = discard right away).
    pub hold_minutes: u32,
    /// Closing the window with X hides TUFFClip to the tray (off = X quits).
    pub close_to_tray: bool,
    /// Minimizing the window hides it to the tray.
    pub minimize_to_tray: bool,
    pub capture_method: CaptureMethod,
    /// Where the recording dot sits (games can override it).
    pub indicator: Indicator,
    /// Programs hidden from the "Add a running app" list (exe names).
    pub ignored_exes: Vec<String>,
    /// Clips recorded from the capture card window are filed under this name.
    pub capture_name: String,
    /// The capture card window may use the camera API (Windows sees capture cards as cameras).
    /// Off until the user allows it from that window.
    pub camera_access: bool,
    /// Ask GitHub for a newer TUFFClip every 15 minutes. Off: TUFFClip never goes online for updates.
    pub check_updates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: CaptureMode::Games,
            monitor: None,
            games: Vec::new(),
            fps: 0,
            height: 0,
            bitrate_kbps: 30_000,
            bitrate_auto: true,
            clip_seconds: 30,
            encoder: Encoder::Nvenc,
            codec: Codec::Hevc,
            draw_mouse: true,
            audio: true,
            audio_source: AudioSource::Game,
            discord_track: true,
            mic: true,
            mic_device: String::new(),
            audio_kbps: 160,
            audio_offset_ms: 0,
            hotkey: "Alt+F10".into(),
            clips_dir: dirs::video_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join("TUFFClip"),
            buffer_dir: None,
            ffmpeg: "ffmpeg".into(),
            beep: true,
            start_hidden: false,
            buffer_in_ram: false,
            bitrate_unit: BitrateUnit::Mbps,
            gentle_save: true,
            hotkey2: String::new(),
            hold_minutes: 5,
            close_to_tray: true,
            minimize_to_tray: false,
            capture_method: CaptureMethod::Window,
            indicator: Indicator::TopRight,
            ignored_exes: Vec::new(),
            capture_name: "Capture card".into(),
            camera_access: false,
            check_updates: false,
        }
    }
}

/// Bitrate for game footage at the given output size, frame rate and codec.
/// Mirrored in `ui/app.js` (`autoRate`) for the settings display; keep them equal.
pub fn auto_bitrate(w: u32, h: u32, fps: u32, codec: Codec) -> u32 {
    let px = w as f64 * h as f64;
    // motion-compensated codecs gain less than linearly from a higher frame rate
    let eff_fps = 60.0 * (fps.max(1) as f64 / 60.0).powf(0.75);
    let h264 = px * eff_fps * 0.2 / 1000.0;
    let kbps = if codec == Codec::Hevc { h264 * 0.65 } else { h264 };
    ((kbps / 500.0).round() * 500.0).clamp(4_000.0, 150_000.0) as u32
}

pub static CONFIG_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn data_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("TUFFClip");
    // The app used to be called Clipr: carry its settings and ffmpeg over once.
    let old = base.join("Clipr");
    if !dir.exists() && old.exists() {
        let _ = fs::rename(&old, &dir);
    }
    dir
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let Ok(text) = fs::read_to_string(config_path()) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(_) => {
                // Keep the broken file around and start fresh.
                let _ = fs::rename(config_path(), data_dir().join("config.broken.json"));
                CONFIG_BROKEN.store(true, std::sync::atomic::Ordering::Relaxed);
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(data_dir())?;
        fs::write(config_path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Clamp values coming from the UI into sane ranges.
    pub fn sanitize(&mut self) {
        if self.fps != 0 {
            self.fps = self.fps.clamp(10, 360);
        }
        self.clip_seconds = self.clip_seconds.clamp(5, 600);
        self.bitrate_kbps = self.bitrate_kbps.clamp(2_000, 150_000);
        self.audio_kbps = self.audio_kbps.clamp(64, 320);
        self.audio_offset_ms = self.audio_offset_ms.clamp(-2_000, 2_000);
        self.hold_minutes = self.hold_minutes.min(60);
        self.hotkey2 = self.hotkey2.trim().to_string();
        if self.ffmpeg.trim().is_empty() {
            self.ffmpeg = "ffmpeg".into();
        }
        let mut seen = std::collections::HashSet::new();
        self.ignored_exes.retain_mut(|e| {
            *e = e.trim().to_string();
            !e.is_empty() && seen.insert(e.to_lowercase())
        });
        self.capture_name = self.capture_name.trim().to_string();
        if self.capture_name.is_empty() {
            self.capture_name = "Capture card".into();
        }
        self.games.retain(|g| !g.exe.trim().is_empty());
        for g in &mut self.games {
            g.exe = g.exe.trim().to_string();
            if g.name.trim().is_empty() {
                g.name = g.exe.trim_end_matches(".exe").to_string();
            }
            if let Some(f) = &mut g.favorite {
                if f.fps != 0 {
                    f.fps = f.fps.clamp(10, 360);
                }
                f.bitrate_kbps = f.bitrate_kbps.clamp(2_000, 150_000);
            }
        }
    }

    /// The settings to record `exe` with: its favorite, if it has one switched on.
    /// Returns the game's name when a favorite applies.
    pub fn for_game(&self, exe: Option<&str>) -> (Config, Option<String>) {
        let fav = exe.filter(|_| self.mode == CaptureMode::Games).and_then(|e| {
            self.games.iter().find(|g| g.exe.eq_ignore_ascii_case(e) && g.use_favorite && g.favorite.is_some())
        });
        let mut c = self.clone();
        let Some(g) = fav else { return (c, None) };
        let f = g.favorite.clone().unwrap();
        c.fps = f.fps;
        c.height = f.height;
        c.codec = f.codec;
        c.bitrate_auto = f.bitrate_auto;
        c.bitrate_kbps = f.bitrate_kbps;
        c.capture_method = f.capture_method;
        (c, Some(g.name.clone()))
    }

    pub fn raw_dir(&self) -> PathBuf {
        self.clips_dir.join("Raw")
    }

    pub fn exports_dir(&self) -> PathBuf {
        self.clips_dir.join("Exports")
    }

    pub fn buffer_dir(&self) -> PathBuf {
        self.buffer_dir
            .clone()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| data_dir().join("buffer"))
    }
}
