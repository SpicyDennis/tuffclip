use crate::config::{data_dir, Config};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Serialize)]
pub struct Clip {
    pub path: String,
    pub name: String,
    pub game: String,
    pub size: u64,
    pub modified: u64,
    /// Raw clips only: a trimmed version of this clip has been exported (and still exists).
    pub exported: bool,
}

/// Clips live in `<root>/<Game>/<file>.mp4`; the folder name is the game.
pub fn list(root: &Path, raw: bool) -> Vec<Clip> {
    let mut v = Vec::new();
    let Ok(rd) = fs::read_dir(root) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let game = e.file_name().to_string_lossy().into_owned();
            if let Ok(inner) = fs::read_dir(&p) {
                for f in inner.flatten() {
                    push(&mut v, f.path(), &game, raw);
                }
            }
        } else {
            push(&mut v, p, "Unsorted", raw);
        }
    }
    v.sort_by(|a, b| b.modified.cmp(&a.modified));
    v
}

fn is_video(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "mp4" | "mkv" | "mov" | "webm")
}

fn push(v: &mut Vec<Clip>, p: PathBuf, game: &str, raw: bool) {
    if !is_video(&p) {
        return;
    }
    let Ok(m) = fs::metadata(&p) else { return };
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = p.to_string_lossy().into_owned();
    v.push(Clip {
        name: p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        exported: raw && has_export(&path),
        path,
        game: game.to_string(),
        size: m.len(),
        modified,
    });
}

pub fn game_of(p: &Path) -> String {
    let g = p
        .parent()
        .and_then(|d| d.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if g.is_empty() || g == "Raw" || g == "Exports" { "Unsorted".into() } else { g }
}

/// How many raw clips a game has. Used to pick the "main" game when several are running.
pub fn count_raw(cfg: &Config, game_folder: &str) -> u32 {
    let Ok(rd) = fs::read_dir(cfg.raw_dir().join(game_folder)) else { return 0 };
    rd.flatten().filter(|e| is_video(&e.path())).count() as u32
}

// ------------------------------------------------------- which raws were exported

/// raw clip path (normalised) -> the export files made from it.
static LOG: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

fn norm(p: &str) -> String {
    p.replace('/', "\\").to_lowercase()
}

fn log_path() -> PathBuf {
    data_dir().join("exported.json")
}

fn with_log<R>(f: impl FnOnce(&mut HashMap<String, Vec<String>>) -> R) -> R {
    let mut g = LOG.lock();
    let map = g.get_or_insert_with(|| {
        fs::read_to_string(log_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    });
    f(map)
}

fn persist(map: &HashMap<String, Vec<String>>) {
    let _ = fs::create_dir_all(data_dir());
    if let Ok(t) = serde_json::to_string(map) {
        let _ = fs::write(log_path(), t);
    }
}

/// Remember that `export` was made from the raw clip `raw`.
pub fn record_export(raw: &str, export: &Path) {
    with_log(|m| {
        let list = m.entry(norm(raw)).or_default();
        let e = export.to_string_lossy().into_owned();
        if !list.contains(&e) {
            list.push(e);
        }
        persist(m);
    });
}

/// A raw clip was renamed: keep its export history.
pub fn rename_raw(old: &str, new: &str) {
    with_log(|m| {
        if let Some(v) = m.remove(&norm(old)) {
            m.insert(norm(new), v);
            persist(m);
        }
    });
}

pub fn forget_raw(raw: &str) {
    with_log(|m| {
        if m.remove(&norm(raw)).is_some() {
            persist(m);
        }
    });
}

/// A trimmed version exists on disk. If you deleted every export, the raw clip counts as not exported.
fn has_export(raw: &str) -> bool {
    with_log(|m| m.get(&norm(raw)).is_some_and(|v| v.iter().any(|e| Path::new(e).is_file())))
}

// ------------------------------------------------------------------ storage

#[derive(Serialize, Default)]
pub struct Storage {
    pub raw_bytes: u64,
    pub raw_count: u32,
    pub exports_bytes: u64,
    pub exports_count: u32,
    /// Raw clips that already have a trimmed export.
    pub done_bytes: u64,
    pub done_count: u32,
}

pub fn storage(cfg: &Config) -> Storage {
    let mut s = Storage::default();
    for c in list(&cfg.raw_dir(), true) {
        s.raw_bytes += c.size;
        s.raw_count += 1;
        if c.exported {
            s.done_bytes += c.size;
            s.done_count += 1;
        }
    }
    for c in list(&cfg.exports_dir(), false) {
        s.exports_bytes += c.size;
        s.exports_count += 1;
    }
    s
}

/// Delete every raw clip that has a trimmed export. Returns (clips deleted, bytes freed).
pub fn delete_exported_raws(cfg: &Config) -> (u32, u64) {
    let (mut n, mut bytes) = (0, 0);
    for c in list(&cfg.raw_dir(), true) {
        if c.exported && fs::remove_file(&c.path).is_ok() {
            forget_raw(&c.path);
            n += 1;
            bytes += c.size;
        }
    }
    (n, bytes)
}
