#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod engine;
mod export;
mod ff;
mod library;
mod recorder;
mod win;

use config::Config;
use engine::{Engine, Status};
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
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
    let old = eng.cfg.lock().clone();
    if old.hotkey != cfg.hotkey {
        if let Err(e) = set_hotkey(app, &cfg.hotkey) {
            let _ = set_hotkey(app, &old.hotkey);
            return Err(format!("Couldn't use \"{}\" as a hotkey ({e}). Another app may own it.", cfg.hotkey));
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

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        std::thread::spawn(move || {
            let _ = w.close();
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
    }
    Ok(new.to_string_lossy().into_owned())
}

#[tauri::command]
fn get_status(eng: Eng<'_>) -> Status {
    eng.status()
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
    let root = if kind == "exports" { cfg.exports_dir() } else { cfg.raw_dir() };
    library::list(&root)
}

fn inside_clips_dir(eng: &Engine, path: &str) -> Result<std::path::PathBuf, String> {
    let root = std::fs::canonicalize(&eng.cfg.lock().clips_dir).map_err(e2s)?;
    let p = std::fs::canonicalize(path).map_err(e2s)?;
    if p.starts_with(&root) { Ok(p) } else { Err("That file isn't in your clips folder".into()) }
}

#[tauri::command]
fn delete_clip(eng: Eng<'_>, path: String) -> Result<(), String> {
    let p = inside_clips_dir(&eng, &path)?;
    std::fs::remove_file(p).map_err(e2s)
}

#[tauri::command]
fn reveal_clip(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path))
        .spawn()
        .map(|_| ())
        .map_err(e2s)
}

#[tauri::command]
fn open_clips_folder(eng: Eng<'_>, kind: String) -> Result<(), String> {
    let cfg = eng.cfg.lock().clone();
    let dir = if kind == "exports" { cfg.exports_dir() } else { cfg.raw_dir() };
    std::fs::create_dir_all(&dir).map_err(e2s)?;
    std::process::Command::new("explorer").arg(dir).spawn().map(|_| ()).map_err(e2s)
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
    let busy = eng.status().recording;
    tauri::async_runtime::spawn_blocking(move || {
        let path = req.path.clone();
        export::export(&cfg, &req, busy, |pct| {
            let _ = app.emit("export-progress", serde_json::json!({ "path": path, "pct": pct }));
        })
        .map(|p| p.to_string_lossy().into_owned())
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

// ------------------------------------------------------------------- helpers

fn set_hotkey(app: &AppHandle, hk: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    gs.unregister_all().map_err(e2s)?;
    gs.register(hk).map_err(e2s)
}

fn save_in_background(app: &AppHandle) {
    let eng = app.state::<Arc<Engine>>().inner().clone();
    std::thread::spawn(move || {
        let _ = eng.save_clip();
    });
}

/// The window is created on demand and destroyed on close, so while you game
/// there is no webview in memory at all — just the tiny Rust core + ffmpeg.
fn show_main(app: &AppHandle) {
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

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Clipr", true, None::<&str>)?;
    let save = MenuItem::with_id(app, "save", "Save clip now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Clipr", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &save, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("Clipr")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "open" => show_main(app),
            "save" => save_in_background(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

// ---------------------------------------------------------------------- main

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        save_in_background(app);
                    }
                })
                .build(),
        )
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
            list_monitors,
            list_windows,
            list_clips,
            delete_clip,
            reveal_clip,
            open_clips_folder,
            save_clip_now,
            export_clip,
            ffmpeg_info,
        ])
        .setup(|app| {
            let cfg = Config::load();
            let _ = std::fs::create_dir_all(&cfg.clips_dir);
            let _ = app.asset_protocol_scope().allow_directory(&cfg.clips_dir, true);
            let hotkey = cfg.hotkey.clone();
            let start_hidden = cfg.start_hidden || std::env::args().any(|a| a == "--hidden");

            let engine = Arc::new(Engine::new(app.handle().clone(), cfg));
            app.manage(engine.clone());

            if let Err(e) = app.global_shortcut().register(hotkey.as_str()) {
                engine.set_hotkey_error(Some(format!(
                    "The hotkey {hotkey} isn't working ({e}). Another app may be using it."
                )));
            }
            build_tray(app)?;
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
