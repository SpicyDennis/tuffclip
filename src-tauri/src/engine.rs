//! Background brain: once a second, decide what (if anything) should be
//! buffering, keep ffmpeg running with the right settings, and save clips.
//! Idle cost is one foreground-window lookup per second — effectively 0% CPU.
use crate::config::{data_dir, CaptureMode, Config};
use crate::recorder::{self, Crop, RecordSpec, Recorder};
use crate::win::{self, MonitorInfo, WinGeom};
use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde::Serialize;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Status {
    pub recording: bool,
    pub game: Option<String>,
    pub monitor: Option<String>,
    /// What part of the screen is captured, e.g. "1280×720 window" or "whole display".
    pub region: Option<String>,
    pub fps: u32,
    pub error: Option<String>,
    /// Something that needs attention but doesn't stop recording (e.g. hotkey taken).
    pub warn: Option<String>,
    pub buffer_bytes: u64,
    pub buffer_ram: bool,
}

#[derive(Clone)]
struct Tracked {
    pid: u32,
    name: String,
    hwnd: isize,
    hmon: isize,
    geom: Option<WinGeom>,
}

pub struct Engine {
    pub cfg: Mutex<Config>,
    recorder: Mutex<Recorder>,
    status: Mutex<Status>,
    tracked: Mutex<Option<Tracked>>,
    strikes: Mutex<u32>,
    retry_at: Mutex<Option<Instant>>,
    crop_pending: Mutex<Option<(Option<Crop>, Instant)>>,
    last_save: Mutex<Option<Instant>>,
    hotkey_err: Mutex<Option<String>>,
    app: AppHandle,
}

fn log_path() -> PathBuf {
    data_dir().join("ffmpeg.log")
}

pub fn sanitize_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let t = cleaned.trim().trim_end_matches('.').trim().to_string();
    if t.is_empty() { "Unknown".into() } else { t }
}

/// How long to wait before restarting ffmpeg after `strikes` quick crashes in a row.
fn backoff(strikes: u32) -> Duration {
    Duration::from_secs(match strikes {
        0..=1 => 0,
        2 => 3,
        3 => 8,
        4 => 20,
        _ => 45,
    })
}

impl Engine {
    pub fn new(app: AppHandle, cfg: Config) -> Self {
        Engine {
            cfg: Mutex::new(cfg),
            recorder: Mutex::new(Recorder::default()),
            status: Mutex::new(Status::default()),
            tracked: Mutex::new(None),
            strikes: Mutex::new(0),
            retry_at: Mutex::new(None),
            crop_pending: Mutex::new(None),
            last_save: Mutex::new(None),
            hotkey_err: Mutex::new(None),
            app,
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }

    pub fn set_hotkey_error(&self, e: Option<String>) {
        *self.hotkey_err.lock() = e;
    }

    /// Settings changed: forget previous ffmpeg failures so we retry right away.
    pub fn config_changed(&self) {
        *self.retry_at.lock() = None;
        *self.strikes.lock() = 0;
    }

    pub fn shutdown(&self) {
        self.recorder.lock().stop();
    }

    pub fn run_watcher(self: Arc<Self>) {
        let mut n: u64 = 0;
        loop {
            // A panic in one tick must never end the watcher: that would silently stop recording.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.tick();
                if n % 30 == 0 {
                    self.check_hotkey();
                }
            }));
            n += 1;
            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Re-register the hotkey if Windows or another app dropped it.
    fn check_hotkey(&self) {
        let hk = self.cfg.lock().hotkey.clone();
        let gs = self.app.global_shortcut();
        if gs.is_registered(hk.as_str()) {
            *self.hotkey_err.lock() = None;
            return;
        }
        *self.hotkey_err.lock() = match gs.register(hk.as_str()) {
            Ok(()) => None,
            Err(e) => Some(format!("The hotkey {hk} isn't working ({e}). Another app may be using it.")),
        };
    }

    fn tick(&self) {
        let cfg = self.cfg.lock().clone();
        let monitors = win::list_monitors();
        let tracked = self.update_tracked(&cfg);
        let mut status = Status::default();
        status.warn = self.hotkey_err.lock().clone();

        match pick_target(&cfg, tracked.as_ref(), &monitors) {
            Some((mon, game)) => {
                let (cand, minimized) = match (&cfg.mode, tracked.as_ref().and_then(|t| t.geom)) {
                    (CaptureMode::Games, Some(g)) => (recorder::crop_for(&mon, &g), g.minimized),
                    _ => (None, false),
                };
                let crop = self.resolve_crop(&mon, cand, minimized);
                let spec = RecordSpec::new(&cfg, &mon, crop);
                status.game = Some(game);
                status.monitor = Some(mon.label.clone());
                status.fps = spec.fps;
                status.region = Some(match spec.crop {
                    Some(c) => format!("{}×{} window", c.w, c.h),
                    None => "whole display".into(),
                });
                match self.ensure_recording(spec) {
                    Ok(()) => status.recording = true,
                    Err(e) => status.error = Some(format!("{e:#}")),
                }
            }
            None => {
                self.recorder.lock().stop();
                *self.strikes.lock() = 0;
                *self.retry_at.lock() = None;
                *self.crop_pending.lock() = None;
            }
        }

        if let Some(b) = self.recorder.lock().buffer_info() {
            status.buffer_bytes = b.bytes / 100_000 * 100_000;
            status.buffer_ram = b.ram;
        }
        self.set_status(status);
    }

    fn update_tracked(&self, cfg: &Config) -> Option<Tracked> {
        let mut t = self.tracked.lock();
        if let Some(fg) = win::foreground() {
            if let Some(g) = cfg
                .games
                .iter()
                .find(|g| g.enabled && g.exe.eq_ignore_ascii_case(&fg.exe))
            {
                *t = Some(Tracked { pid: fg.pid, name: g.name.clone(), hwnd: fg.hwnd, hmon: fg.hmon, geom: None });
            }
        }
        // Keep tracking through alt-tab; drop it when the game exits or is removed.
        if let Some(cur) = t.as_mut() {
            let listed = cfg.games.iter().any(|g| g.enabled && g.name == cur.name);
            if !listed || !win::process_alive(cur.pid) {
                *t = None;
            } else if let Some(g) = win::window_geometry(cur.hwnd) {
                cur.hmon = g.hmon;
                cur.geom = Some(g);
            }
        }
        t.clone()
    }

    /// A window that is being dragged or resized shouldn't restart the recording on every
    /// pixel (a restart empties the buffer), so a new region has to hold still for 3 seconds.
    fn resolve_crop(&self, mon: &MonitorInfo, cand: Option<Crop>, minimized: bool) -> Option<Crop> {
        let running = {
            let rec = self.recorder.lock();
            rec.spec().map(|s| (s.adapter, s.output, s.crop))
        };
        let Some((adapter, output, cur)) = running else { return cand };
        if adapter != mon.adapter || output != mon.output {
            return cand;
        }
        let mut pending = self.crop_pending.lock();
        if minimized || cur == cand {
            *pending = None;
            return cur;
        }
        match *pending {
            Some((c, since)) if c == cand => {
                if since.elapsed() >= Duration::from_secs(3) {
                    *pending = None;
                    cand
                } else {
                    cur
                }
            }
            _ => {
                *pending = Some((cand, Instant::now()));
                cur
            }
        }
    }

    fn note_crash(&self, uptime: Duration) {
        let mut s = self.strikes.lock();
        *s = if uptime < Duration::from_secs(15) { *s + 1 } else { 1 };
        *self.retry_at.lock() = Some(Instant::now() + backoff(*s));
    }

    fn waiting_message(&self) -> String {
        let s = *self.strikes.lock();
        let left = self
            .retry_at
            .lock()
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs() + 1)
            .unwrap_or(0);
        if s >= 3 {
            format!("FFmpeg keeps stopping with these settings. Trying again in {left}s. Check ffmpeg.log in the data folder.")
        } else {
            "Recording stopped. Restarting…".into()
        }
    }

    fn ensure_recording(&self, spec: RecordSpec) -> Result<()> {
        let wait_until = *self.retry_at.lock();
        if wait_until.is_some_and(|t| Instant::now() < t) {
            bail!(self.waiting_message());
        }
        let mut rec = self.recorder.lock();
        let same = rec.spec() == Some(&spec);
        if same && rec.is_alive() {
            let up = rec.uptime();
            // ffmpeg is running but has produced nothing for a long time: it's hung. Restart it.
            let stalled = up > Duration::from_secs(25)
                && rec.buffer_info().is_some_and(|b| b.idle_secs > 20);
            if !stalled {
                if up > Duration::from_secs(30) {
                    *self.strikes.lock() = 0;
                }
                return Ok(());
            }
            rec.stop();
            self.note_crash(up);
            bail!(self.waiting_message());
        }
        if same {
            // ffmpeg died on its own. Retry, waiting longer after repeated quick crashes.
            let up = rec.uptime();
            rec.stop();
            self.note_crash(up);
            let wait_until = *self.retry_at.lock();
            if wait_until.is_some_and(|t| Instant::now() < t) {
                bail!(self.waiting_message());
            }
        }
        *self.retry_at.lock() = None;
        match rec.start(spec, &log_path()) {
            Ok(()) => Ok(()),
            Err(e) => {
                *self.retry_at.lock() = Some(Instant::now() + Duration::from_secs(5));
                Err(e)
            }
        }
    }

    fn set_status(&self, s: Status) {
        let mut cur = self.status.lock();
        if *cur != s {
            *cur = s.clone();
            drop(cur);
            let _ = self.app.emit("status", s);
        }
    }

    pub fn save_clip(&self) -> Result<PathBuf> {
        {
            let mut last = self.last_save.lock();
            // Holding the key repeats the hotkey; ignore repeats within a second.
            if last.is_some_and(|t| t.elapsed() < Duration::from_millis(1000)) {
                bail!("Already saving a clip");
            }
            *last = Some(Instant::now());
        }
        let res = self.save_clip_inner();
        let beep = self.cfg.lock().beep;
        match &res {
            Ok(p) => {
                if beep {
                    win::beep();
                }
                let _ = self.app.emit("clip-saved", p.to_string_lossy().to_string());
            }
            Err(e) => {
                // A failed save must not block the next try.
                *self.last_save.lock() = None;
                if beep {
                    win::beep_error();
                }
                let _ = self.app.emit("clip-error", format!("{e:#}"));
            }
        }
        res
    }

    fn save_clip_inner(&self) -> Result<PathBuf> {
        let cfg = self.cfg.lock().clone();
        let (ram, buffer_dir) = {
            let rec = self.recorder.lock();
            let spec = rec.spec().ok_or_else(|| anyhow!("Nothing is being recorded right now"))?;
            (rec.ram(), spec.buffer_dir.clone())
        };
        let game = sanitize_name(&self.status().game.unwrap_or_else(|| "Desktop".into()));
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let out = cfg.raw_dir().join(&game).join(format!("{game}_{stamp}.mp4"));
        match ram {
            Some(r) => recorder::save_ram(&cfg.ffmpeg, &r, cfg.clip_seconds, &out)?,
            None => recorder::save_buffer(&cfg.ffmpeg, &buffer_dir, cfg.clip_seconds, &out)?,
        }
        Ok(out)
    }
}

fn pick_target(
    cfg: &Config,
    tracked: Option<&Tracked>,
    monitors: &[MonitorInfo],
) -> Option<(MonitorInfo, String)> {
    let primary = || monitors.iter().find(|m| m.primary).or(monitors.first()).cloned();
    match cfg.mode {
        CaptureMode::Games => {
            let t = tracked?;
            let m = monitors.iter().find(|m| m.hmon == t.hmon).cloned().or_else(primary)?;
            Some((m, t.name.clone()))
        }
        CaptureMode::Desktop => {
            let m = cfg
                .monitor
                .as_ref()
                .and_then(|id| monitors.iter().find(|m| &m.id == id).cloned())
                .or_else(primary)?;
            let game = tracked.map(|t| t.name.clone()).unwrap_or_else(|| "Desktop".into());
            Some((m, game))
        }
    }
}
