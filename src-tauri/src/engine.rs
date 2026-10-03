//! Background brain: once a second, decide what (if anything) should be
//! buffering, keep ffmpeg running with the right settings, and save clips.
//! Idle cost is one window lookup per second — effectively 0% CPU.
use crate::config::{data_dir, BitrateUnit, CaptureMethod, CaptureMode, Codec, Config, Indicator};
use crate::recorder::{self, Crop, RamBuf, RecordSpec, Recorder};
use crate::win::{self, MonitorInfo, WinGeom};
use crate::{audio, library, overlay, tray};
use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

/// Stands in for an exe name when the thing being recorded is TUFFClip's own capture card window.
pub const CAPTURE_EXE: &str = "<capture card>";

/// One game that is running and could be recorded.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Choice {
    pub exe: String,
    pub name: String,
    pub clips: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Status {
    pub recording: bool,
    pub game: Option<String>,
    pub monitor: Option<String>,
    /// What part of the screen is captured, e.g. "1280×720 window" or "whole display".
    pub region: Option<String>,
    pub fps: u32,
    /// e.g. "1920×1080 · 144 fps · 30 Mbps · H.264"
    pub summary: String,
    pub error: Option<String>,
    /// Something that needs attention but doesn't stop recording (e.g. hotkey taken).
    pub warn: Option<String>,
    pub buffer_bytes: u64,
    pub buffer_ram: bool,
    /// The game closed, but its last buffer can still be saved until this moment (ms since epoch).
    pub held_until_ms: Option<u64>,
    /// exe of the game being recorded (games mode).
    pub target: Option<String>,
    /// Title of the window being recorded, so you can check it is the right one.
    pub window_title: Option<String>,
    /// All running games, when there is more than one to pick from.
    pub choices: Vec<Choice>,
    /// What a running benchmark is doing, e.g. "Benchmark: round 2 of 6 · recording off".
    pub bench: Option<String>,
    /// The game recording with its favorite settings (its name), if any.
    pub favorite: Option<String>,
}

#[derive(Clone)]
struct Tracked {
    pid: u32,
    exe: String,
    name: String,
    hwnd: isize,
    hmon: isize,
    geom: Option<WinGeom>,
}

/// The buffer of a game that has closed: kept so the clip hotkey still works for a few minutes.
struct Held {
    game: String,
    buffer_dir: PathBuf,
    ram: Option<Arc<RamBuf>>,
    layout: Option<audio::Layout>,
    /// When recording stopped (ms since epoch): clips end here.
    ended_ms: u64,
    until: Instant,
    until_ms: u64,
}

pub struct Engine {
    pub cfg: Mutex<Config>,
    recorder: Mutex<Recorder>,
    status: Mutex<Status>,
    tracked: Mutex<Option<Tracked>>,
    held: Mutex<Option<Held>>,
    /// The game the user picked by hand (exe, lowercase); cleared when that game closes.
    manual: Mutex<Option<String>>,
    clip_counts: Mutex<HashMap<String, (Instant, u32)>>,
    tray_state: Mutex<String>,
    strikes: Mutex<u32>,
    retry_at: Mutex<Option<Instant>>,
    crop_pending: Mutex<Option<(Option<Crop>, Instant)>>,
    last_save: Mutex<Option<Instant>>,
    hotkey_err: Mutex<Option<String>>,
    /// The capture card picture shown in the capture window: (width, height, fps).
    feed: Mutex<Option<(u32, u32, u32)>>,
    /// The benchmark switched recording off for a while.
    bench_paused: AtomicBool,
    bench_note: Mutex<Option<String>>,
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

fn even_down(x: u32) -> u32 {
    (x / 2 * 2).max(2)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn fmt_rate(kbps: u32, unit: BitrateUnit) -> String {
    match unit {
        BitrateUnit::Mbps => {
            let m = kbps as f64 / 1000.0;
            if m.fract() == 0.0 { format!("{m:.0} Mbps") } else { format!("{m:.1} Mbps") }
        }
        BitrateUnit::Kbps => format!("{kbps} kbps"),
    }
}

impl Engine {
    pub fn new(app: AppHandle, cfg: Config) -> Self {
        Engine {
            cfg: Mutex::new(cfg),
            recorder: Mutex::new(Recorder::default()),
            status: Mutex::new(Status::default()),
            tracked: Mutex::new(None),
            held: Mutex::new(None),
            manual: Mutex::new(None),
            clip_counts: Mutex::new(HashMap::new()),
            tray_state: Mutex::new(String::new()),
            strikes: Mutex::new(0),
            retry_at: Mutex::new(None),
            crop_pending: Mutex::new(None),
            last_save: Mutex::new(None),
            hotkey_err: Mutex::new(None),
            feed: Mutex::new(None),
            bench_paused: AtomicBool::new(false),
            bench_note: Mutex::new(None),
            app,
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }

    /// What the recorder is capturing right now (None when idle).
    pub fn spec(&self) -> Option<RecordSpec> {
        self.recorder.lock().spec().cloned()
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

    /// Pick which running game to record (None = back to the automatic choice).
    pub fn set_target(&self, exe: Option<String>) {
        *self.manual.lock() = exe.map(|e| e.to_lowercase());
    }

    /// The capture window started showing the card (or stopped: None).
    pub fn set_feed(&self, feed: Option<(u32, u32, u32)>) {
        *self.feed.lock() = feed.filter(|f| f.0 >= 64 && f.1 >= 64);
    }

    /// The capture card window, while it is open.
    fn capture_hwnd(&self) -> Option<isize> {
        let w = self.app.get_webview_window("capture")?;
        if !w.is_visible().unwrap_or(false) {
            return None;
        }
        w.hwnd().ok().map(|h| h.0 as isize)
    }

    fn capture_tracked(&self, cfg: &Config, hwnd: isize) -> Tracked {
        let geom = win::window_geometry(hwnd);
        Tracked {
            pid: std::process::id(),
            exe: CAPTURE_EXE.into(),
            name: cfg.capture_name.clone(),
            hwnd,
            hmon: geom.map(|g| g.hmon).unwrap_or(0),
            geom,
        }
    }

    /// Benchmark: switch recording off (true) or back on (false). Off stops ffmpeg right away.
    pub fn bench_pause(&self, on: bool) {
        let was = self.bench_paused.swap(on, SeqCst);
        if on {
            self.recorder.lock().stop();
        } else if was {
            self.config_changed();
        }
    }

    pub fn set_bench_note(&self, note: Option<String>) {
        *self.bench_note.lock() = note;
    }

    /// The game a benchmark would measure: (pid, name). Not the capture card window.
    pub fn bench_target(&self) -> Option<(u32, String)> {
        let t = self.tracked.lock();
        t.as_ref().filter(|t| t.exe != CAPTURE_EXE).map(|t| (t.pid, t.name.clone()))
    }

    /// ffmpeg's process id while recording.
    pub fn recorder_pid(&self) -> Option<u32> {
        self.recorder.lock().child_pid()
    }

    pub fn memory(&self) -> win::MemInfo {
        win::memory_info(self.recorder.lock().child_handle())
    }

    pub fn run_watcher(self: Arc<Self>) {
        let mut n: u64 = 0;
        loop {
            // A panic in one tick must never end the watcher: that would silently stop recording.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.tick();
                if n % 30 == 0 {
                    self.check_hotkeys();
                }
            }));
            n += 1;
            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Re-register the hotkeys if Windows or another app dropped them.
    fn check_hotkeys(&self) {
        let (a, b) = {
            let c = self.cfg.lock();
            (c.hotkey.clone(), c.hotkey2.clone())
        };
        let gs = self.app.global_shortcut();
        let mut err = None;
        for hk in [a, b].into_iter().filter(|h| !h.is_empty()) {
            if gs.is_registered(hk.as_str()) {
                continue;
            }
            if let Err(e) = gs.register(hk.as_str()) {
                err = Some(format!("The hotkey {hk} isn't working ({e}). Another app may be using it."));
            }
        }
        *self.hotkey_err.lock() = err;
    }

    fn tick(&self) {
        let cfg = self.cfg.lock().clone();
        let monitors = win::list_monitors();
        let (tracked, choices) = self.update_tracked(&cfg);
        // A game with favorite settings records with those instead of the normal ones.
        let (cfg, favorite) = cfg.for_game(tracked.as_ref().map(|t| t.exe.as_str()));
        let mut status = Status::default();
        status.warn = self.hotkey_err.lock().clone();
        status.choices = choices;
        status.target = tracked.as_ref().map(|t| t.exe.clone());
        status.bench = self.bench_note.lock().clone();
        status.favorite = favorite;
        let mut dot: Option<overlay::Target> = None;

        let paused = self.bench_paused.load(SeqCst);
        let target = if paused {
            // The benchmark is measuring the game without recording.
            self.recorder.lock().stop();
            status.game = tracked.as_ref().map(|t| t.name.clone());
            None
        } else {
            pick_target(&cfg, tracked.as_ref(), &monitors)
        };
        match target {
            Some((mon, game)) => {
                status.game = Some(game);
                status.monitor = Some(mon.label.clone());
                let geom = tracked.as_ref().and_then(|t| t.geom);
                let card = cfg.mode == CaptureMode::Games && tracked.as_ref().is_some_and(|t| t.exe == CAPTURE_EXE);
                let want_window = cfg.mode == CaptureMode::Games && (card || cfg.capture_method == CaptureMethod::Window);
                let window_mode = want_window && crate::ff::has_gfxcapture(&cfg.ffmpeg);
                if want_window && !window_mode && status.warn.is_none() {
                    status.warn = Some("This FFmpeg has no gfxcapture filter, so the whole game area is recorded instead. Update FFmpeg in Settings > Advanced.".into());
                }
                let minimized = geom.is_some_and(|g| g.minimized);

                let spec = if window_mode {
                    self.window_spec(&cfg, &mon, tracked.as_ref().map(|t| t.hwnd), geom, card)
                } else {
                    let (cand, min) = match (&cfg.mode, geom) {
                        (CaptureMode::Games, Some(g)) => (recorder::crop_for(&mon, &g), g.minimized),
                        _ => (None, false),
                    };
                    let crop = self.resolve_crop(&mon, cand, min);
                    let mut s = RecordSpec::new(&cfg, &mon, crop, None);
                    // Below native the display is captured with gfxcapture (it scales on the GPU).
                    if s.height != 0 && s.height < s.src_h && !crate::ff::has_gfxcapture(&cfg.ffmpeg) {
                        s.height = 0;
                        s.apply_auto_bitrate(&cfg);
                        if status.warn.is_none() {
                            status.warn = Some("This FFmpeg can't scale the screen on the graphics card (no gfxcapture), so it records at full resolution. Update FFmpeg in Settings > Advanced.".into());
                        }
                    }
                    Some(s)
                };

                match spec {
                    Some(mut spec) => {
                        // Game-only sound follows the recorded program (in games mode).
                        let game_pid = tracked.as_ref().filter(|_| cfg.mode == CaptureMode::Games).map(|t| t.pid);
                        spec.tracks = audio::plan(&cfg, game_pid);
                        status.fps = spec.fps;
                        status.region = Some(match (spec.window, spec.crop) {
                            (Some(_), _) if card => format!("{}×{} capture card", spec.src_w, spec.src_h),
                            (Some(_), _) => format!("{}×{} window", spec.src_w, spec.src_h),
                            (None, Some(c)) => format!("{}×{} window", c.w, c.h),
                            _ => "whole display".into(),
                        });
                        status.summary = summarize(&spec, &cfg);
                        match self.ensure_recording(spec, minimized) {
                            Ok(()) => {
                                status.recording = true;
                                dot = indicator_for(&cfg, tracked.as_ref(), &mon);
                                if cfg.mode == CaptureMode::Games {
                                    status.window_title = tracked.as_ref().and_then(|t| win::window_title(t.hwnd));
                                }
                            }
                            Err(e) => status.error = Some(format!("{e:#}")),
                        }
                    }
                    None if card => status.region = Some("waiting for the capture card".into()),
                    None => status.region = Some("waiting for the game window".into()),
                }
            }
            None if paused => {}
            None => {
                self.keep_or_discard(&cfg);
                *self.strikes.lock() = 0;
                *self.retry_at.lock() = None;
                *self.crop_pending.lock() = None;
            }
        }
        self.expire_held();
        overlay::set(dot);

        if status.recording {
            if let Some(b) = self.recorder.lock().buffer_info() {
                status.buffer_bytes = b.bytes / 100_000 * 100_000;
                status.buffer_ram = b.ram;
            }
        } else if let Some(h) = self.held.lock().as_ref() {
            status.game = Some(h.game.clone());
            status.held_until_ms = Some(h.until_ms);
            status.buffer_ram = h.ram.is_some();
            let bytes = match &h.ram {
                Some(r) => r.bytes(),
                None => recorder::dir_bytes(&h.buffer_dir),
            };
            status.buffer_bytes = bytes / 100_000 * 100_000;
        }
        self.update_tray(&status, &cfg);
        self.set_status(status);
    }

    /// The capture spec for window mode. `None` while there is no usable window yet.
    /// `card`: it is the capture card window, recorded at the card's own size and frame rate.
    fn window_spec(&self, cfg: &Config, mon: &MonitorInfo, hwnd: Option<isize>, geom: Option<WinGeom>, card: bool) -> Option<RecordSpec> {
        let hwnd = hwnd?;
        let running = self.recorder.lock().spec().cloned();
        if card {
            // Nothing to record until the window shows the card's picture.
            let (w, h, fps) = (*self.feed.lock())?;
            let mut s = RecordSpec::new(cfg, mon, None, Some((hwnd, even_down(w), even_down(h))));
            s.force_size = true;
            if let Some(cur) = running.filter(|c| c.window == Some(hwnd)) {
                s.adapter = cur.adapter;
                s.output = cur.output;
                if cfg.fps == 0 && cur.native_fps {
                    s.fps = cur.fps;
                }
            }
            // A 60 fps console recorded at 144 fps would only store copies of frames.
            s.fps = s.fps.min(fps.clamp(24, 240));
            s.apply_auto_bitrate(cfg);
            return Some(s);
        }
        if let Some(cur) = running.filter(|c| c.window == Some(hwnd)) {
            // Already capturing this window: a resize or a move to another monitor must not
            // restart the recording (that would empty the buffer), so keep its size and GPU.
            let mut s = RecordSpec::new(cfg, mon, None, Some((hwnd, cur.src_w, cur.src_h)));
            s.adapter = cur.adapter;
            s.output = cur.output;
            // Native keeps the rate it started with, but switching to native from a picked
            // rate (60 -> native) must change it.
            if cfg.fps == 0 && cur.native_fps {
                s.fps = cur.fps;
                s.apply_auto_bitrate(cfg);
            }
            return Some(s);
        }
        let g = geom.filter(|g| !g.minimized && g.w >= 64 && g.h >= 64)?;
        Some(RecordSpec::new(cfg, mon, None, Some((hwnd, even_down(g.w), even_down(g.h)))))
    }

    /// Nothing to record any more. If a buffer was running, hold on to it for a while.
    fn keep_or_discard(&self, cfg: &Config) {
        let kept = self.recorder.lock().stop_and_keep();
        let Some(recorder::Kept { dir, ram, layout }) = kept else { return };
        if cfg.hold_minutes == 0 {
            if ram.is_none() {
                let _ = recorder::clear_buffer(&dir);
            }
            return;
        }
        let game = self.status.lock().game.clone().unwrap_or_else(|| "Desktop".into());
        let hold = Duration::from_secs(cfg.hold_minutes as u64 * 60);
        *self.held.lock() = Some(Held {
            game,
            buffer_dir: dir,
            ram,
            layout,
            ended_ms: now_ms(),
            until: Instant::now() + hold,
            until_ms: now_ms() + hold.as_millis() as u64,
        });
    }

    /// Flush a held buffer once its time is up.
    fn expire_held(&self) {
        let mut h = self.held.lock();
        if h.as_ref().is_some_and(|x| Instant::now() >= x.until) {
            if let Some(x) = h.take() {
                if x.ram.is_none() {
                    let _ = recorder::clear_buffer(&x.buffer_dir);
                }
            }
        }
    }

    /// Games mode: of the listed games that are running, record the one with the most clips
    /// (or the one picked by hand) and stay on it. Desktop mode: whichever listed game is focused.
    fn update_tracked(&self, cfg: &Config) -> (Option<Tracked>, Vec<Choice>) {
        if cfg.mode == CaptureMode::Desktop {
            return (self.update_tracked_focus(cfg), Vec::new());
        }
        // An open capture card window is something you opened to play on: it wins over games.
        if let Some(hwnd) = self.capture_hwnd() {
            let t = self.capture_tracked(cfg, hwnd);
            *self.tracked.lock() = Some(t.clone());
            return (Some(t), Vec::new());
        }
        let exes: HashSet<String> = cfg.games.iter().filter(|g| g.enabled).map(|g| g.exe.to_lowercase()).collect();
        let running = if exes.is_empty() { Vec::new() } else { win::running_games(&exes) };
        let fg = win::foreground();

        // (game entry, window) for each running game
        let mut cands: Vec<(&crate::config::GameEntry, win::RunWin)> = Vec::new();
        for w in running {
            if let Some(g) = cfg.games.iter().find(|g| g.enabled && g.exe.eq_ignore_ascii_case(&w.exe)) {
                if !cands.iter().any(|(e, _)| e.exe.eq_ignore_ascii_case(&g.exe)) {
                    cands.push((g, w));
                }
            }
        }

        let mut t = self.tracked.lock();
        let mut manual = self.manual.lock();
        if manual.as_ref().is_some_and(|m| !cands.iter().any(|(g, _)| g.exe.eq_ignore_ascii_case(m))) {
            *manual = None; // the game you picked has closed: back to automatic
        }

        let pick = if let Some(m) = manual.as_ref() {
            cands.iter().find(|(g, _)| g.exe.eq_ignore_ascii_case(m))
        } else if let Some(cur) = t.as_ref().filter(|c| cands.iter().any(|(_, w)| w.pid == c.pid)) {
            cands.iter().find(|(_, w)| w.pid == cur.pid)
        } else {
            let fg_pid = fg.as_ref().map(|f| f.pid);
            cands.iter().max_by_key(|(g, w)| (self.clips_of(cfg, &g.name), Some(w.pid) == fg_pid, std::cmp::Reverse(w.pid)))
        };

        let choices: Vec<Choice> = if cands.len() > 1 {
            cands
                .iter()
                .map(|(g, _)| Choice { exe: g.exe.clone(), name: g.name.clone(), clips: self.clips_of(cfg, &g.name) })
                .collect()
        } else {
            Vec::new()
        };

        let Some((g, w)) = pick else {
            *t = None;
            return (None, choices);
        };
        let prev = t.as_ref().filter(|p| p.pid == w.pid);
        // Follow the game's focused window if it has several; otherwise keep the one we had.
        let hwnd = match &fg {
            Some(f) if f.pid == w.pid && win::window_exists(f.hwnd) => f.hwnd,
            _ => match prev {
                Some(p) if win::window_exists(p.hwnd) => p.hwnd,
                _ => w.hwnd,
            },
        };
        let geom = win::window_geometry(hwnd);
        let hmon = geom.map(|g| g.hmon).or(prev.map(|p| p.hmon)).unwrap_or(0);
        let cur = Tracked { pid: w.pid, exe: g.exe.clone(), name: g.name.clone(), hwnd, hmon, geom };
        *t = Some(cur.clone());
        (Some(cur), choices)
    }

    /// Desktop mode: remember the last focused listed game so clips are filed under it.
    fn update_tracked_focus(&self, cfg: &Config) -> Option<Tracked> {
        let card = self.capture_hwnd();
        let mut t = self.tracked.lock();
        if let Some(fg) = win::foreground() {
            if card == Some(fg.hwnd) {
                *t = Some(self.capture_tracked(cfg, fg.hwnd));
            } else if let Some(g) = cfg.games.iter().find(|g| g.enabled && g.exe.eq_ignore_ascii_case(&fg.exe)) {
                *t = Some(Tracked { pid: fg.pid, exe: g.exe.clone(), name: g.name.clone(), hwnd: fg.hwnd, hmon: fg.hmon, geom: None });
            }
        }
        if let Some(cur) = t.as_mut() {
            let listed = if cur.exe == CAPTURE_EXE {
                card == Some(cur.hwnd)
            } else {
                cfg.games.iter().any(|g| g.enabled && g.name == cur.name)
            };
            if !listed || !win::process_alive(cur.pid) {
                *t = None;
            } else if let Some(g) = win::window_geometry(cur.hwnd) {
                cur.hmon = g.hmon;
                cur.geom = Some(g);
            }
        }
        t.clone()
    }

    /// Raw clips this game already has (cached for a few seconds; this runs every tick).
    fn clips_of(&self, cfg: &Config, name: &str) -> u32 {
        let mut cache = self.clip_counts.lock();
        if let Some((at, n)) = cache.get(name) {
            if at.elapsed() < Duration::from_secs(20) {
                return *n;
            }
        }
        let n = library::count_raw(cfg, &sanitize_name(name));
        cache.insert(name.to_string(), (Instant::now(), n));
        n
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

    /// `paused`: the game window is minimized, so no new frames are expected and that is not a hang.
    fn ensure_recording(&self, spec: RecordSpec, paused: bool) -> Result<()> {
        let wait_until = *self.retry_at.lock();
        if wait_until.is_some_and(|t| Instant::now() < t) {
            bail!(self.waiting_message());
        }
        let mut rec = self.recorder.lock();
        let same = rec.spec() == Some(&spec);
        if same && rec.is_alive() {
            let up = rec.uptime();
            // ffmpeg is running but has produced nothing for a long time: it's hung. Restart it.
            let stalled = !paused
                && up > Duration::from_secs(25)
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
        // A new recording overwrites whatever buffer was being held.
        *self.held.lock() = None;
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

    /// Tray icon (red dot while recording, amber while a closed game's buffer is held) and tooltip.
    fn update_tray(&self, st: &Status, cfg: &Config) {
        let short = |s: &str, n: usize| -> String {
            if s.chars().count() > n { format!("{}…", s.chars().take(n - 1).collect::<String>()) } else { s.to_string() }
        };
        let hk = if cfg.hotkey2.is_empty() { cfg.hotkey.clone() } else { format!("{} / {}", cfg.hotkey, cfg.hotkey2) };
        let game = short(st.game.as_deref().unwrap_or("Desktop"), 24);
        let (kind, tip) = if let Some(e) = &st.error {
            (tray::Kind::Idle, format!("TUFFClip · {}", short(e, 80)))
        } else if let Some(b) = &st.bench {
            let kind = if st.recording { tray::Kind::Recording } else { tray::Kind::Idle };
            (kind, format!("TUFFClip · {game}\n{}", short(b, 80)))
        } else if st.recording {
            (
                tray::Kind::Recording,
                format!(
                    "TUFFClip · Recording {game}\n{}\n{} buffer · {} s clips · {hk}",
                    st.summary,
                    if st.buffer_ram { "RAM" } else { "Disk" },
                    cfg.clip_seconds
                ),
            )
        } else if let Some(until) = st.held_until_ms {
            let mins = until.saturating_sub(now_ms()).div_ceil(60_000).max(1);
            (
                tray::Kind::Held,
                format!("TUFFClip · {game} closed\nLast buffer kept for {mins} more min\n{hk} still saves it"),
            )
        } else if cfg.mode == CaptureMode::Games {
            (tray::Kind::Idle, "TUFFClip · Waiting for a game".to_string())
        } else {
            (tray::Kind::Idle, "TUFFClip · Not recording".to_string())
        };
        let tip: String = tip.chars().take(120).collect();
        let key = format!("{}|{tip}", kind as u8);
        let mut last = self.tray_state.lock();
        if *last != key {
            *last = key;
            drop(last);
            tray::update(&self.app, kind, &tip);
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
                overlay::flash(true);
                self.clip_counts.lock().clear();
                let _ = self.app.emit("clip-saved", p.to_string_lossy().to_string());
            }
            Err(e) => {
                // A failed save must not block the next try.
                *self.last_save.lock() = None;
                if beep {
                    win::beep_error();
                }
                overlay::flash(false);
                let _ = self.app.emit("clip-error", format!("{e:#}"));
            }
        }
        res
    }

    fn save_clip_inner(&self) -> Result<PathBuf> {
        let cfg = self.cfg.lock().clone();
        let (ram, buffer_dir, game, sound) = {
            let rec = self.recorder.lock();
            if let Some(spec) = rec.spec() {
                let sound = rec.layout().map(|l| l.pick(cfg.clip_seconds, now_ms()));
                (rec.ram(), spec.buffer_dir.clone(), self.status().game, sound)
            } else if let Some(h) = self.held.lock().as_ref() {
                let sound = h.layout.as_ref().map(|l| l.pick(cfg.clip_seconds, h.ended_ms));
                (h.ram.clone(), h.buffer_dir.clone(), Some(h.game.clone()), sound)
            } else {
                return Err(anyhow!("Nothing is being recorded right now"));
            }
        };
        let game = sanitize_name(&game.unwrap_or_else(|| "Desktop".into()));
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let out = cfg.raw_dir().join(&game).join(format!("{game}_{stamp}.mp4"));
        match ram {
            Some(r) => recorder::save_ram(&cfg.ffmpeg, &r, cfg.clip_seconds, &out, cfg.gentle_save, sound.as_deref())?,
            None => recorder::save_buffer(&cfg.ffmpeg, &buffer_dir, cfg.clip_seconds, &out, cfg.gentle_save, sound.as_deref())?,
        }
        Ok(out)
    }
}

/// "1920×1080 · 144 fps · 30 Mbps · H.264"
fn summarize(spec: &RecordSpec, cfg: &Config) -> String {
    let (w, h) = if spec.height != 0 && spec.height < spec.src_h {
        (even_down(spec.src_w * spec.height / spec.src_h), even_down(spec.height))
    } else {
        (spec.src_w, spec.src_h)
    };
    format!(
        "{w}×{h} · {} fps · {} · {}",
        spec.fps,
        fmt_rate(spec.bitrate_kbps, cfg.bitrate_unit),
        match spec.codec {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        }
    )
}

/// Where the recording dot goes for the tracked game (its own choice, else the global one).
fn indicator_for(cfg: &Config, tracked: Option<&Tracked>, mon: &MonitorInfo) -> Option<overlay::Target> {
    let pos = tracked
        .and_then(|t| cfg.games.iter().find(|g| g.exe.eq_ignore_ascii_case(&t.exe)))
        .and_then(|g| g.indicator)
        .unwrap_or(cfg.indicator);
    if pos == Indicator::Off {
        return None;
    }
    let rect = (mon.x, mon.y, mon.width as i32, mon.height as i32);
    match (cfg.mode, tracked) {
        (CaptureMode::Games, Some(t)) => Some(overlay::Target { hwnd: Some(t.hwnd), rect, pos }),
        _ => Some(overlay::Target { hwnd: None, rect, pos }),
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
