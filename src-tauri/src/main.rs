#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod engine;
mod export;
mod ff;
mod library;
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
    let changed = old != cfg;
    *eng.cfg.lock() = cfg;
    if changed {
        eng.config_changed();
    }
    Ok(())
}

#[tauri::command]
fn save_config(app: AppHandle, eng: Eng<'_>, cfg: Config) -> Result<(), String> {
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
    reg(&["query", RUN_KEY, "/v", "Clipr"]).map(|s| s.success()).unwrap_or(false)
}

fn set_autostart(on: bool) -> Result<(), String> {
    if on {
        let exe = std::env::current_exe().map_err(e2s)?;
        let value = format!("\"{}\" --hidden", exe.display());
        reg(&["add", RUN_KEY, "/v", "Clipr", "/t", "REG_SZ", "/d", &value, "/f"])
            .map_err(e2s)
            .and_then(|s| if s.success() { Ok(()) } else { Err("Windows wouldn't let Clipr add itself to startup.".into()) })
    } else {
        let _ = reg(&["delete", RUN_KEY, "/v", "Clipr", "/f"]);
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
fn list_windows() -> Vec<win::AppWindow> {
    win::list_windows()
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
fn memory_info(eng: Eng<'_>) -> win::MemInfo {
    eng.memory()
}

/// The clip's video codec ("h264", "hevc", ...), for size estimates.
#[tauri::command]
async fn probe_clip(eng: Eng<'_>, path: String) -> Result<String, String> {
    let ffmpeg = eng.cfg.lock().ffmpeg.clone();
    tauri::async_runtime::spawn_blocking(move || ff::probe_codec(&ffmpeg, std::path::Path::new(&path)))
        .await
        .map_err(e2s)
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
        library::record_export(&req.path, &out);
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

/// Download FFmpeg into Clipr's data folder and point the setting at it. Returns the new path.
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
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let app = app.clone();
    // Building a window inside an event handler can deadlock on Windows; use a thread.
    std::thread::spawn(move || {
        let _ = WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
            .title("Clipr")
            .inner_size(1240.0, 780.0)
            .min_inner_size(960.0, 600.0)
            .build();
    });
}

// ---------------------------------------------------------------------- main

fn main() {
    tauri::Builder::default()
        // Must be first: a second Clipr.exe hands over to this one and exits.
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
            list_clips,
            delete_clip,
            reveal_clip,
            open_clips_folder,
            storage_info,
            delete_exported_raws,
            memory_info,
            probe_clip,
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

            if !start_hidden {
                show_main(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build Clipr")
        .run(|app, event| match event {
            // Closing the window keeps Clipr running in the tray.
            tauri::RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
            tauri::RunEvent::Exit => app.state::<Arc<Engine>>().shutdown(),
            _ => {}
        });
}
