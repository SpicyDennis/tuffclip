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
pub enum ExportFormat {
    Mp4,
    Mkv,
    Mov,
    Webm,
}

impl ExportFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ExportFormat::Mp4 => "mp4",
            ExportFormat::Mkv => "mkv",
            ExportFormat::Mov => "mov",
            ExportFormat::Webm => "webm",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameEntry {
    pub exe: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
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
    pub clip_seconds: u32,
    pub encoder: Encoder,
    pub codec: Codec,
    pub draw_mouse: bool,
    pub audio: bool,
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
    pub export_format: ExportFormat,
    /// Throttle exports while a game is being recorded so they never cost frames.
    pub export_gentle: bool,
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
            clip_seconds: 30,
            encoder: Encoder::Nvenc,
            codec: Codec::H264,
            draw_mouse: true,
            audio: true,
            audio_kbps: 160,
            audio_offset_ms: 0,
            hotkey: "Alt+F10".into(),
            clips_dir: dirs::video_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join("Clipr"),
            buffer_dir: None,
            ffmpeg: "ffmpeg".into(),
            beep: true,
            start_hidden: false,
            buffer_in_ram: false,
            bitrate_unit: BitrateUnit::Mbps,
            export_format: ExportFormat::Mp4,
            export_gentle: true,
        }
    }
}

pub static CONFIG_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipr")
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
        if self.ffmpeg.trim().is_empty() {
            self.ffmpeg = "ffmpeg".into();
        }
        self.games.retain(|g| !g.exe.trim().is_empty());
        for g in &mut self.games {
            g.exe = g.exe.trim().to_string();
            if g.name.trim().is_empty() {
                g.name = g.exe.trim_end_matches(".exe").to_string();
            }
        }
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
