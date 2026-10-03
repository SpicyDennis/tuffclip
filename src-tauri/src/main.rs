#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod engine;
mod export;
mod ff;
mod library;
mod overlay;
mod preview;
mod recorder;
mod tray;
mod win;

use config::Config;
use engine::{Engine, Status};
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

type Eng<'a> = State<'a, Arc<Engine>>;

fn e2s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ------------------------------------------------------------------ commands

#[tauri::command]
fn get_config(eng: Eng<'_>) -> Config {
    eng.cfg.lock().clone()
}

fn apply_config(app: &AppHandle, eng: &Engine, mut cfg: Config) -> Result<(), String> {
    cfg.sanitize();
    if !cfg.hotkey2.is_empty() && cfg.hotkey2.eq_ignore_ascii_case(&cfg.hotkey) {
        return Err("Both shortcuts are the same. Pick a different one for the second.".into());
    }
    let old = eng.cfg.lock().clone();
    if old.hotkey != cfg.hotkey || old.hotkey2 != cfg.hotkey2 {
        if let Err(e) = set_hotkeys(app, &cfg.hotkey, &cfg.hotkey2) {
            let _ = set_hotkeys(app, &old.hotkey, &old.hotkey2);
            return Err(e);
        }
        eng.set_hotkey_error(None);
    }
    if old.clips_dir != cfg.clips_dir {
        let _ = app.asset_protocol_scope().allow_directory(&cfg.clips_dir, true);
    }
    cfg.save().map_err(e2s)?;
    // A game's name changed: its clips follow it (older clips move to the new name's folder).
    for g in &cfg.games {
        if let Some(og) = old.games.iter().find(|o| o.exe.eq_ignore_ascii_case(&g.exe)) {
            if og.name != g.name {
                library::rename_game(&cfg, &og.name, &g.name);
            }
        }
    }
    if old.capture_name != cfg.capture_name {
        library::rename_game(&cfg, &old.capture_name, &cfg.capture_name);
    }
    let changed = old != cfg;
    *eng.cfg.lock() = cfg;
    if changed {
        eng.config_changed();
    }
    Ok(())
}

#[tauri::command]
fn save_config(app: AppHandle, eng: Eng<'_>, mut cfg: Config) -> Result<(), String> {
    // Camera access changes only through set_camera_access (a stale settings page must not undo it).
    cfg.camera_access = eng.cfg.lock().camera_access;
    apply_config(&app, &eng, cfg)
}

#[tauri::command]
fn reset_config(app: AppHandle, eng: Eng<'_>) -> Result<Config, String> {
    let _ = set_autostart(false);
    let mut cfg = Config::default();
    // Keep a working ffmpeg path and clips location; those aren't "preferences".
    let cur = eng.cfg.lock().clone();
    cfg.ffmpeg = cur.ffmpeg;
    cfg.clips_dir = cur.clips_dir;
    apply_config(&app, &eng, cfg.clone())?;
    Ok(eng.cfg.lock().clone())
}

#[derive(serde::Serialize)]
struct AppInfo {
    version: String,
    build_date: String,
    data_dir: String,
    config_broken: bool,
    autostart: bool,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        build_date: env!("CLIPR_BUILD_DATE").into(),
        data_dir: config::data_dir().to_string_lossy().into_owned(),
        config_broken: config::CONFIG_BROKEN.load(std::sync::atomic::Ordering::Relaxed),
        autostart: autostart_enabled(),
    }
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn reg(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
}

fn autostart_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "TUFFClip"]).map(|s| s.success()).unwrap_or(false)
}

fn set_autostart(on: bool) -> Result<(), String> {
    let _ = reg(&["delete", RUN_KEY, "/v", "Clipr", "/f"]); // entry from before the rename
    if on {
        let exe = std::env::current_exe().map_err(e2s)?;
        let value = format!("\"{}\" --hidden", exe.display());
        reg(&["add", RUN_KEY, "/v", "TUFFClip", "/t", "REG_SZ", "/d", &value, "/f"])
            .map_err(e2s)
            .and_then(|s| if s.success() { Ok(()) } else { Err("Windows wouldn't let TUFFClip add itself to startup.".into()) })
    } else {
        let _ = reg(&["delete", RUN_KEY, "/v", "TUFFClip", "/f"]);
        Ok(())
    }
}

#[tauri::command]
fn set_autostart_cmd(on: bool) -> Result<(), String> {
    set_autostart(on)
}

#[tauri::command]
fn open_data_folder() -> Result<(), String> {
    let dir = config::data_dir();
    std::fs::create_dir_all(&dir).map_err(e2s)?;
    std::process::Command::new("explorer").arg(dir).spawn().map(|_| ()).map_err(e2s)
}

/// Close the window (not the app). `destroy`, not `close`, so the "X quits" setting isn't triggered.
#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        std::thread::spawn(move || {
            let _ = w.destroy();
        });
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn rename_clip(eng: Eng<'_>, path: String, name: String) -> Result<String, String> {
    inside_clips_dir(&eng, &path)?;
    let old = std::path::PathBuf::from(&path);
    let ext = old.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = export::clean_stem(&name, &ext);
    if stem.is_empty() {
        return Err("Give the clip a name".into());
    }
    let new = old.with_file_name(format!("{stem}.{ext}"));
    let same_name = new.file_name().map(|n| n.to_string_lossy().to_lowercase())
        == old.file_name().map(|n| n.to_string_lossy().to_lowercase());
    if !same_name && new.exists() {
        return Err("A clip with that name already exists".into());
    }
    if new != old {
        std::fs::rename(&old, &new).map_err(e2s)?;
        library::rename_raw(&path, &new.to_string_lossy());
    }
    Ok(new.to_string_lossy().into_owned())
}

#[tauri::command]
fn get_status(eng: Eng<'_>) -> Status {
    eng.status()
}

/// Record a different running game than the automatic pick (None = automatic again).
#[tauri::command]
fn set_target(eng: Eng<'_>, exe: Option<String>) {
    eng.set_target(exe.filter(|e| !e.is_empty()));
}

#[tauri::command]
fn list_monitors() -> Vec<win::MonitorInfo> {
    win::list_monitors()
}

#[tauri::command]
fn list_windows(eng: Eng<'_>) -> Vec<win::AppWindow> {
    let ignored: Vec<String> = eng.cfg.lock().ignored_exes.iter().map(|e| e.to_lowercase()).collect();
    let mut v = win::list_windows();
    v.retain(|w| !ignored.contains(&w.exe.to_lowercase()));
    v
}

/// Start the live preview of what is being recorded.
#[tauri::command]
fn preview_start(app: AppHandle, eng: Eng<'_>) -> Result<(), String> {
    let spec = eng.spec().ok_or("Nothing is being recorded right now")?;
    preview::start(&app, &spec).map_err(e2s)
}

#[tauri::command]
fn preview_stop() {
    preview::stop();
}

/// Open (or bring forward) the capture card window.
#[tauri::command]
fn open_capture(app: AppHandle) {
    show_capture(&app);
}

/// Allow (or withdraw) the capture window's camera access.
#[tauri::command]
fn set_camera_access(app: AppHandle, eng: Eng<'_>, on: bool) -> Result<(), String> {
    let mut cfg = eng.cfg.lock().clone();
    cfg.camera_access = on;
    apply_config(&app, &eng, cfg)
}

/// The capture window reports the card's picture size and frame rate (all None = no picture).
#[tauri::command]
fn capture_feed(eng: Eng<'_>, width: Option<u32>, height: Option<u32>, fps: Option<u32>) {
    let feed = match (width, height) {
        (Some(w), Some(h)) => Some((w, h, fps.filter(|f| *f > 0).unwrap_or(60))),
        _ => None,
    };
    eng.set_feed(feed);
}

/// Switch the calling window in or out of fullscreen (None = toggle). Returns the new state.
#[tauri::command]
fn set_fullscreen(window: tauri::WebviewWindow, on: Option<bool>) -> bool {
    let on = on.unwrap_or_else(|| !window.is_fullscreen().unwrap_or(false));
    let _ = window.set_fullscreen(on);
    on
}

#[tauri::command]
fn list_clips(eng: Eng<'_>, kind: String) -> Vec<library::Clip> {
    let cfg = eng.cfg.lock().clone();
    if kind == "exports" {
        library::list(&cfg.exports_dir(), false)
    } else {
        library::list(&cfg.raw_dir(), true)
    }
}

fn inside_clips_dir(eng: &Engine, path: &str) -> Result<std::path::PathBuf, String> {
    let root = std::fs::canonicalize(&eng.cfg.lock().clips_dir).map_err(e2s)?;
    let p = std::fs::canonicalize(path).map_err(e2s)?;
    if p.starts_with(&root) { Ok(p) } else { Err("That file isn't in your clips folder".into()) }
}

#[tauri::command]
fn delete_clip(eng: Eng<'_>, path: String) -> Result<(), String> {
    let p = inside_clips_dir(&eng, &path)?;
    std::fs::remove_file(p).map_err(e2s)?;
    library::forget_raw(&path);
    Ok(())
}

#[tauri::command]
fn reveal_clip(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path))
        .spawn()
        .map(|_| ())
        .map_err(e2s)
}

/// kind: "raw", "exports", or anything else for the whole clips folder.
#[tauri::command]
fn open_clips_folder(eng: Eng<'_>, kind: String) -> Result<(), String> {
    let cfg = eng.cfg.lock().clone();
    let dir = match kind.as_str() {
        "exports" => cfg.exports_dir(),
        "raw" => cfg.raw_dir(),
        _ => cfg.clips_dir.clone(),
    };
    std::fs::create_dir_all(&dir).map_err(e2s)?;
    std::process::Command::new("explorer").arg(dir).spawn().map(|_| ()).map_err(e2s)
}

#[tauri::command]
async fn storage_info(eng: Eng<'_>) -> Result<library::Storage, String> {
    let cfg = eng.cfg.lock().clone();
    tauri::async_runtime::spawn_blocking(move || library::storage(&cfg)).await.map_err(e2s)
}

#[derive(serde::Serialize)]
struct Deleted {
    count: u32,
    bytes: u64,
}

/// Delete every raw clip that already has a trimmed export.
#[tauri::command]
async fn delete_exported_raws(eng: Eng<'_>) -> Result<Deleted, String> {
    let cfg = eng.cfg.lock().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (count, bytes) = library::delete_exported_raws(&cfg);
        Deleted { count, bytes }
    })
    .await
    .map_err(e2s)
}

#[tauri::command]
async fn memory_info(eng: Eng<'_>) -> Result<win::MemInfo, String> {
    // Off the main thread: it waits on the recorder, which must never freeze the window or tray.
    let eng = eng.inner().clone();
    tauri::async_runtime::spawn_blocking(move || eng.memory()).await.map_err(e2s)
}

/// The clip's video codec ("h264", "hevc", ...), for size estimates.
#[tauri::command]
async fn probe_clip(eng: Eng<'_>, path: String) -> Result<String, String> {
    let ffmpeg = eng.cfg.lock().ffmpeg.clone();
    tauri::async_runtime::spawn_blocking(move || ff::probe_codec(&ffmpeg, std::path::Path::new(&path)))
        .await
        .map_err(e2s)
}

/// An H.264 copy of a clip for the viewer: the embedded WebView2 can't always decode HEVC (the
/// picture stays black). Cached under `<clips_dir>\.playback` (inside the asset scope), made on
/// demand; the original is never touched. Returns the proxy's path.
#[tauri::command]
async fn playback_proxy(eng: Eng<'_>, path: String) -> Result<String, String> {
    let src = inside_clips_dir(&eng, &path)?;
    let (ffmpeg, dir) = {
        let c = eng.cfg.lock();
        (c.ffmpeg.clone(), std::path::PathBuf::from(&c.clips_dir).join(".playback"))
    };
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        std::fs::create_dir_all(&dir).map_err(e2s)?;
        let meta = std::fs::metadata(&src).map_err(e2s)?;
        let stamp = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let out = dir.join(format!("{stem}_{stamp}_{}.mp4", meta.len()));
        if out.is_file() {
            return Ok(out.to_string_lossy().into_owned());
        }
        let tmp = out.with_extension("part.mp4");
        let mut c = ff::cmd_low(&ffmpeg);
        c.args(["-hide_banner", "-y", "-i"]).arg(&src).args([
            "-vf", "scale=-2:'min(1080,ih)',fps=60", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "26",
            "-pix_fmt", "yuv420p", "-g", "30", "-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart",
        ]).arg(&tmp);
        ff::run(c).map_err(|e| format!("{e:#}"))?;
        std::fs::rename(&tmp, &out).map_err(e2s)?;
        // Keep the cache small: drop the oldest copies beyond 8.
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<_> = rd.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
            files.sort();
            let extra = files.len().saturating_sub(8);
            for (_, p) in files.into_iter().take(extra) {
                let _ = std::fs::remove_file(p);
            }
        }
        Ok(out.to_string_lossy().into_owned())
    })
    .await
    .map_err(e2s)?
}

#[tauri::command]
async fn save_clip_now(eng: Eng<'_>) -> Result<String, String> {
    let eng = eng.inner().clone();
    tauri::async_runtime::spawn_blocking(move || eng.save_clip())
        .await
        .map_err(e2s)?
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn export_clip(app: AppHandle, eng: Eng<'_>, req: export::ExportRequest) -> Result<String, String> {
    let cfg = eng.cfg.lock().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = req.path.clone();
        let out = export::export(&cfg, &req, |pct| {
            let _ = app.emit("export-progress", serde_json::json!({ "path": path, "pct": pct }));
        })?;
        // A screenshot isn't a trimmed version of the clip, so it doesn't mark the raw as exported.
        if req.format != export::ExportFormat::Png {
            library::record_export(&req.path, &out);
        }
        Ok::<_, anyhow::Error>(out.to_string_lossy().into_owned())
    })
    .await
    .map_err(e2s)?
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn ffmpeg_info(eng: Eng<'_>) -> Result<ff::FfInfo, String> {
    let path = eng.cfg.lock().ffmpeg.clone();
    tauri::async_runtime::spawn_blocking(move || ff::info(&path)).await.map_err(e2s)
}

/// Download FFmpeg into TUFFClip's data folder and point the setting at it. Returns the new path.
#[tauri::command]
async fn download_ffmpeg(app: AppHandle, eng: Eng<'_>) -> Result<String, String> {
    let dir = config::data_dir().join("ffmpeg");
    let a = app.clone();
    let path = tauri::async_runtime::spawn_blocking(move || {
        ff::download(&dir, |bytes| {
            let _ = a.emit("ffmpeg-download", bytes);
        })
    })
    .await
    .map_err(e2s)?
    .map_err(|e| format!("{e:#}"))?;
    let path = path.to_string_lossy().into_owned();
    let mut cfg = eng.cfg.lock().clone();
    cfg.ffmpeg = path.clone();
    apply_config(&app, &eng, cfg)?;
    Ok(path)
}

// ------------------------------------------------------------------- helpers

/// Replace all registered shortcuts with these (an empty second one is skipped).
fn set_hotkeys(app: &AppHandle, a: &str, b: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    gs.unregister_all().map_err(e2s)?;
    gs.register(a)
        .map_err(|e| format!("Couldn't use \"{a}\" as a shortcut ({e}). Another app may own it."))?;
    if !b.is_empty() {
        gs.register(b)
            .map_err(|e| format!("Couldn't use \"{b}\" as a shortcut ({e}). Another app may own it."))?;
    }
    Ok(())
}

pub(crate) fn save_in_background(app: &AppHandle) {
    let eng = app.state::<Arc<Engine>>().inner().clone();
    std::thread::spawn(move || {
        let _ = eng.save_clip();
    });
}

/// The window is created on demand and destroyed on close, so while you game
/// there is no webview in memory at all — just the tiny Rust core + ffmpeg.
pub(crate) fn show_main(app: &AppHandle) {
    let app = app.clone();
    // Building a window inside an event handler can deadlock on Windows; use a thread.
    // Opening must never silently fail (a window still closing, WebView2 hiccup), so retry.
    std::thread::spawn(move || {
        for _ in 0..8 {
            if let Some(w) = app.get_webview_window("main") {
                if w.show().is_ok() {
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                    // If it was one that is just being destroyed, it will be gone shortly: go round again.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    if app.get_webview_window("main").is_some() {
                        return;
                    }
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
                continue;
            }
            let built = WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
                .title("TUFFClip")
                .inner_size(1240.0, 780.0)
                .min_inner_size(960.0, 600.0)
                .build();
            if built.is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    });
}

/// The capture card window: shows the card's live picture and sound so you can play on it,
/// and while it is open the recorder captures it like a game. Created on demand, destroyed on close.
pub(crate) fn show_capture(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Some(w) = app.get_webview_window("capture") {
            let _ = w.show();
            let _ = w.unminimize();
            let _ = w.set_focus();
            return;
        }
        let built = WebviewWindowBuilder::new(&app, "capture", WebviewUrl::App("capture.html".into()))
            .title("TUFFClip · Capture card")
            .inner_size(1280.0, 720.0)
            .min_inner_size(480.0, 270.0)
            .build();
        if let Ok(w) = built {
            gate_camera(&app, &w);
        }
    });
}

/// The camera permission is ours, not WebView2's: every request from the capture window is
/// answered from `Config.camera_access` and never saved by WebView2, so there is no browser prompt
/// and the permission is ours to give.
fn gate_camera(app: &AppHandle, w: &tauri::WebviewWindow) {
    use webview2_com::{Microsoft::Web::WebView2::Win32::*, PermissionRequestedEventHandler};
    let eng: Arc<Engine> = app.state::<Arc<Engine>>().inner().clone();
    let _ = w.with_webview(move |wv| unsafe {
        let Ok(core) = wv.controller().CoreWebView2() else { return };
        let mut token = Default::default();
        let handler = PermissionRequestedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let mut kind = COREWEBVIEW2_PERMISSION_KIND_UNKNOWN_PERMISSION;
                args.PermissionKind(&mut kind)?;
                if kind == COREWEBVIEW2_PERMISSION_KIND_CAMERA || kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE {
                    let state = if eng.cfg.lock().camera_access {
                        COREWEBVIEW2_PERMISSION_STATE_ALLOW
                    } else {
                        COREWEBVIEW2_PERMISSION_STATE_DENY
                    };
                    args.SetState(state)?;
                    if let Ok(a3) = windows_core::Interface::cast::<ICoreWebView2PermissionRequestedEventArgs3>(&args) {
                        let _ = a3.SetSavesInProfile(false);
                    }
                }
            }
            Ok(())
        }));
        let _ = core.add_PermissionRequested(&handler, &mut token);
    });
}

// ---------------------------------------------------------------------- main

/// Backup for the global shortcuts: while something is recording, poll the keys directly, so the
/// clip hotkey still works when the registered shortcut is swallowed (another app or a game that
/// grabs the keyboard, a window of ours open but behind). A press seen by both only saves once.
fn start_key_poll(app: AppHandle) {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    fn vk(name: &str) -> Option<i32> {
        let n = name.to_ascii_uppercase();
        if let Some(f) = n.strip_prefix('F').and_then(|d| d.parse::<i32>().ok()) {
            return (1..=24).contains(&f).then_some(0x6F + f);
        }
        if n.len() == 1 {
            let c = n.as_bytes()[0];
            if c.is_ascii_alphanumeric() {
                return Some(c as i32);
            }
        }
        Some(match n.as_str() {
            "SPACE" => 0x20,
            "ENTER" => 0x0D,
            "TAB" => 0x09,
            "INSERT" => 0x2D,
            "DELETE" => 0x2E,
            "HOME" => 0x24,
            "END" => 0x23,
            "PAGEUP" => 0x21,
            "PAGEDOWN" => 0x22,
            "ARROWLEFT" => 0x25,
            "ARROWUP" => 0x26,
            "ARROWRIGHT" => 0x27,
            "ARROWDOWN" => 0x28,
            _ => return None,
        })
    }
    /// (ctrl, alt, shift, super, key)
    fn parse(s: &str) -> Option<(bool, bool, bool, bool, i32)> {
        let (mut c, mut a, mut sh, mut su, mut key) = (false, false, false, false, None);
        for p in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "commandorcontrol" | "cmdorctrl" => c = true,
                "alt" | "option" => a = true,
                "shift" => sh = true,
                "super" | "meta" | "cmd" | "command" | "win" => su = true,
                _ => key = vk(p),
            }
        }
        key.map(|k| (c, a, sh, su, k))
    }
    fn down(vk: i32) -> bool {
        unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
    }
    std::thread::spawn(move || {
        let mut held = [false; 2];
        loop {
            std::thread::sleep(std::time::Duration::from_millis(40));
            let Some(eng) = app.try_state::<Arc<Engine>>() else { continue };
            let (a, b) = {
                let c = eng.cfg.lock();
                (c.hotkey.clone(), c.hotkey2.clone())
            };
            let st = eng.status();
            if !st.recording && st.held_until_ms.is_none() {
                held = [false; 2];
                std::thread::sleep(std::time::Duration::from_millis(400));
                continue;
            }
            for (i, hk) in [a, b].iter().enumerate() {
                let Some((c, al, sh, su, k)) = parse(hk) else { continue };
                let now = down(k)
                    && down(0x11) == c
                    && down(0x12) == al
                    && down(0x10) == sh
                    && (down(0x5B) || down(0x5C)) == su;
                if now && !held[i] {
                    save_in_background(&app);
                }
                held[i] = now;
            }
        }
    });
}

/// If the app's main thread stops answering (a hung webview, a stuck driver call), a background
/// thread notices and restarts TUFFClip, so a frozen copy can never block you from opening it again.
fn start_self_heal(app: AppHandle) {
    use std::sync::atomic::{AtomicU64, Ordering};
    fn secs() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
    let last_ok = Arc::new(AtomicU64::new(secs()));
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let ok = last_ok.clone();
        let _ = app.run_on_main_thread(move || ok.store(secs(), Ordering::Relaxed));
        if secs().saturating_sub(last_ok.load(Ordering::Relaxed)) > 90 {
            if let Ok(exe) = std::env::current_exe() {
                // The new copy waits a moment (see main) so this one is gone before it starts.
                if std::process::Command::new(exe).args(["--hidden", "--relaunch"]).spawn().is_ok() {
                    std::process::exit(1);
                }
            }
            return;
        }
    });
}

fn main() {
    if std::env::args().any(|a| a == "--relaunch") {
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    tauri::Builder::default()
        // Must be first: a second TUFFClip.exe hands over to this one and exits.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if !args.iter().any(|a| a == "--hidden") {
                show_main(app);
            }
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        save_in_background(app);
                    }
                })
                .build(),
        )
        .on_window_event(|window, event| {
            if window.label() == "capture" {
                if let WindowEvent::Destroyed = event {
                    if let Some(eng) = window.app_handle().try_state::<Arc<Engine>>() {
                        eng.set_feed(None);
                    }
                }
                return;
            }
            if window.label() != "main" {
                return;
            }
            let app = window.app_handle();
            let Some(eng) = app.try_state::<Arc<Engine>>() else { return };
            match event {
                // X: hide to the tray (the default), or quit if the setting says so.
                WindowEvent::CloseRequested { .. } => {
                    if !eng.cfg.lock().close_to_tray {
                        app.exit(0);
                    }
                }
                WindowEvent::Destroyed => preview::stop(),
                WindowEvent::Resized(_) => {
                    if eng.cfg.lock().minimize_to_tray && window.is_minimized().unwrap_or(false) {
                        let w = window.clone();
                        std::thread::spawn(move || {
                            let _ = w.destroy();
                        });
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            reset_config,
            app_info,
            set_autostart_cmd,
            open_data_folder,
            hide_window,
            quit_app,
            rename_clip,
            get_status,
            set_target,
            list_monitors,
            list_windows,
            preview_start,
            preview_stop,
            open_capture,
            set_camera_access,
            capture_feed,
            set_fullscreen,
            list_clips,
            delete_clip,
            reveal_clip,
            open_clips_folder,
            storage_info,
            delete_exported_raws,
            memory_info,
            probe_clip,
            playback_proxy,
            save_clip_now,
            export_clip,
            ffmpeg_info,
            download_ffmpeg,
        ])
        .setup(|app| {
            let cfg = Config::load();
            let _ = std::fs::create_dir_all(&cfg.clips_dir);
            let _ = app.asset_protocol_scope().allow_directory(&cfg.clips_dir, true);
            let (hk1, hk2) = (cfg.hotkey.clone(), cfg.hotkey2.clone());
            let start_hidden = cfg.start_hidden || std::env::args().any(|a| a == "--hidden");

            let engine = Arc::new(Engine::new(app.handle().clone(), cfg));
            app.manage(engine.clone());

            if let Err(e) = set_hotkeys(app.handle(), &hk1, &hk2) {
                engine.set_hotkey_error(Some(e));
            }
            tray::build(app)?;
            std::thread::spawn(move || engine.run_watcher());
            start_self_heal(app.handle().clone());
            start_key_poll(app.handle().clone());

            if !start_hidden {
                show_main(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build TUFFClip")
        .run(|app, event| match event {
            // Closing the window keeps TUFFClip running in the tray.
            tauri::RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
            tauri::RunEvent::Exit => app.state::<Arc<Engine>>().shutdown(),
            _ => {}
        });
}
