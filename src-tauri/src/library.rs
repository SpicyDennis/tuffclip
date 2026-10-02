use serde::Serialize;
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
}

/// Clips live in `<root>/<Game>/<file>.mp4`; the folder name is the game.
pub fn list(root: &Path) -> Vec<Clip> {
    let mut v = Vec::new();
    let Ok(rd) = fs::read_dir(root) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let game = e.file_name().to_string_lossy().into_owned();
            if let Ok(inner) = fs::read_dir(&p) {
                for f in inner.flatten() {
                    push(&mut v, f.path(), &game);
                }
            }
        } else {
            push(&mut v, p, "Unsorted");
        }
    }
    v.sort_by(|a, b| b.modified.cmp(&a.modified));
    v
}

fn push(v: &mut Vec<Clip>, p: PathBuf, game: &str) {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !matches!(ext.as_str(), "mp4" | "mkv" | "mov" | "webm") {
        return;
    }
    let Ok(m) = fs::metadata(&p) else { return };
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    v.push(Clip {
        name: p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        path: p.to_string_lossy().into_owned(),
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
